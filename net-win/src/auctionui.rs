//! 拍卖行面板（DLL 原生窗口，独立线程 + 系统输入法支持）
//!
//! 与 authui.rs（登录面板）/ input.rs（聊天输入条）同一路线：
//! DLL 自己的线程上开窗口，IME 原生接管。三页切换：
//!   [市场]     在售挂单列表（LISTBOX，双击=购买）
//!   [我的挂单] 自己的单子（LISTBOX，双击=下架）
//!   [上架]     物品下拉 + 数量 + 单价（COMBOBOX + 数字输入框）
//!
//! 数据由 Ruby 侧喂进来（物品名等游戏数据 DLL 不知道）：
//!   net_aui_set_list(kind, ptr, len)  kind 0=市场 1=我的 2=背包下拉
//!     行格式（UTF-8，\t 分隔列，\n 分隔行，行 id 与显示文字）：
//!       kind0/kind1: "listing_id\t显示文字"
//!       kind2:       "item_id\t显示文字\t参考单价"
//!     kind2 的第三列（游戏内参考单价）用于上架页默认价格：
//!     选中物品时自动填入价格框，数量默认 1。
//!
//! 导出函数（stdcall）：
//!   net_aui_open()                -> 0 成功（幂等）
//!   net_aui_poll()                -> 0 打开中 1 有提交待取 2 用户取消 3 未打开
//!   net_aui_get_action()          -> 0 购买 1 上架 2 下架 3 刷新
//!   net_aui_get_int(which)        -> 0 listing_id 1 item_id 2 quantity 3 price
//!   net_aui_set_list(kind,ptr,len)-> 填充列表数据，0 成功
//!   net_aui_set_status(ptr,len)   -> 状态栏文字，0 成功
//!   net_aui_close()               -> 0 成功（幂等）

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// 复用 input.rs 的 IME 组合检测（上架页输入框组合中不误提交）
use crate::input::{is_composing, read_edit_text};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 面板客户区尺寸
const PANEL_W: i32 = 460;
const PANEL_H: i32 = 360;
/// 整窗不透明度（LWA_ALPHA，与聊天输入条同款 200 → 半透明观感）
const PANEL_ALPHA: u8 = 200;
/// 定时器间隔（毫秒）
const SYNC_TIMER_MS: u32 = 30;

// 窗口样式
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_BORDER: u32 = 0x0080_0000;
const WS_VSCROLL: u32 = 0x0020_0000;
const ES_AUTOHSCROLL: u32 = 0x0000_0080;
const ES_NUMBER: u32 = 0x0000_2000;
const SS_CENTER: u32 = 0x0000_0001;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const LWA_ALPHA: u32 = 2;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;

// ShowWindow 命令
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;

// 列表框通知与消息（物品选择也用 LISTBOX，弃用组合框——
//   CBS_DROPDOWNLIST 闭合区的颜色不受 WM_CTLCOLOR 控制，是白底的主要来源）
const LBN_DBLCLK: u32 = 2;
const LBN_SELCHANGE: u32 = 1;
/// ★★ LB_GETCURSEL = 0x0188（曾误写 0x0187 = LB_GETSEL：单行列表读成越界、
///   多行列表永远读第 1 行 → 双击提示"未选中"/操作错物品的根因）
const WM_LB_GETCURSEL: u32 = 0x0188;
const WM_LB_SETCURSEL: u32 = 0x0186;
const WM_LB_RESETCONTENT: u32 = 0x0184;
const WM_LB_ADDSTRING: u32 = 0x0181;
/// LBS_NOTIFY：把双击/选中变化等通知发给父窗口
const LBS_NOTIFY: u32 = 0x0001;
const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;

// 按钮走系统默认绘制（★ 自绘曾导致面板打不开/文字方块，全面回退）。
// 「按下去的效果」用 BM_SETSTATE 让选中页签保持按下视觉（系统自带样式）
/// BM_SETSTATE：wParam 1=按下外观 0=常态（页签选中态用它锁住按下视觉）
const BM_SETSTATE: u32 = 0x00F3;

// 列表项自绘（★ 画游戏真实物品图标：IconSet.png 24x24 格子）
//   LBS_OWNERDRAWFIXED → 父窗口收 WM_MEASUREITEM（定行高）+ WM_DRAWITEM（画每行）
const LBS_OWNERDRAWFIXED: u32 = 0x0010;
/// 自绘行高（图标 24 + 上下边距）
const ITEM_H: i32 = 28;
/// WM_MEASUREITEM（0x002C）：owner-draw 控件创建时询问行高
/// ★ 曾误写 0x002A（实为 WM_NCHITTEST 前的无关消息号）→ 行高永不被设置，
///   LBS_OWNERDRAWFIXED 默认 itemHeight=0 → 每行高 0 → 列表整个空白（数据都在）
const WM_MEASUREITEM: u32 = 0x002C;
/// WM_DRAWITEM（0x002B，★ 曾误写 0x0009 导致自绘从未触发——白底方块按钮的真凶）
const WM_DRAWITEM: u32 = 0x002B;
/// DRAWITEMSTRUCT.itemState：行处于选中态
const ODS_SELECTED: u32 = 0x0001;
/// DrawTextW：左对齐 + 垂直居中 + 单行
const DT_LEFT_VCENTER_SINGLE: u32 = 0x0004 | 0x0020;
/// GDI+ UnitPixel
const GP_UNIT_PIXEL: i32 = 2;
/// RMVA IconSet 规格：24x24 格子，每行 16 个
const ICON_GRID: i32 = 16;
const ICON_SIZE: i32 = 24;

// 键盘 VK（方向键切页签用）
/// VK_LEFT（←）
const VK_LEFT: usize = 0x25;
/// VK_RIGHT（→）
const VK_RIGHT: usize = 0x27;
/// LB_SETITEMHEIGHT：显式设 LBS_OWNERDRAWFIXED 行高（0x01A0）
/// ★ 比 WM_MEASUREITEM 可靠（后者只在创建瞬间发一次，时序不保证）
const WM_LB_SETITEMHEIGHT: u32 = 0x01A0;

// 消息
const WM_DESTROY: u32 = 0x0002;
/// WM_CLOSE（0x0010）
const WM_CLOSE: u32 = 0x0010;
/// WM_ERASEBKGND（0x0014）：擦背景——★ 窗口类背景刷为 0 时系统不擦，
///   隐藏控件的像素会永久残留（页切换叠影的根因），必须自擦
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETFONT: u32 = 0x0030;
const WM_TIMER: u32 = 0x0113;
const WM_COMMAND: u32 = 0x0111;
const WM_CTLCOLOREDIT: u32 = 0x0133;
const WM_CTLCOLORDLG: u32 = 0x0136;
const WM_CTLCOLORSTATIC: u32 = 0x0138;
const WM_CTLCOLORLISTBOX: u32 = 0x0134;
/// 拖动/尺寸调整结束（用户手动移动过 → 不再自动居中，标题栏拖动）
const WM_EXITSIZEMOVE: u32 = 0x0232;
/// 跨线程唤醒消息
const WM_APP_WAKE: u32 = 0x8002;
/// WM_KEYDOWN（Esc/Tab 自处理用）
const WM_KEYDOWN: u32 = 0x0100;

// 高低字提取
const WM_COMMAND_ID: usize = 0xFFFF;

// 控件 ID
const BTN_TAB_MARKET: usize = 2001;
const BTN_TAB_MINE: usize = 2002;
const BTN_TAB_SELL: usize = 2003;
const BTN_BUY: usize = 2004;
const BTN_CANCEL_L: usize = 2005;
const BTN_REFRESH: usize = 2006;
const BTN_DO_SELL: usize = 2007;
const LIST_MARKET: usize = 2008;
const LIST_MINE: usize = 2009;
/// 上架页物品选择列表（原组合框，已改 LISTBOX）
const LIST_ITEM: usize = 2010;
const EDIT_QTY: usize = 2011;
const EDIT_PRICE: usize = 2012;

// 动作码（与导出函数注释一致）
const ACT_BUY: i32 = 0;
const ACT_SELL: i32 = 1;
const ACT_CANCEL: i32 = 2;
const ACT_REFRESH: i32 = 3;

// 颜色（COLORREF = 0x00BBGGRR，与聊天输入条一致：深黑底 + 白字）
const COLOR_BG: u32 = 0x0010_1010;
const COLOR_FG: u32 = 0x00FF_FFFF;
/// 自绘列表选中行背景（深蓝，白字可读）
const COLOR_SEL: u32 = 0x0050_5030;

// 字体
const FONT_H: i32 = -15;
const FW_NORMAL: i32 = 400;
const DEFAULT_CHARSET: u32 = 1;
const CLEARTYPE_QUALITY: u32 = 5;
const DEFAULT_PITCH: u32 = 0;

// ---------------------------------------------------------------------------
// Win32 API 手写声明
// ---------------------------------------------------------------------------

type HWND = isize;
type HIMC = isize;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MSG {
    hwnd: HWND,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: POINT,
}

#[repr(C)]
struct WNDCLASSW {
    style: u32,
    lpfn_wnd_proc: unsafe extern "system" fn(HWND, u32, usize, isize) -> isize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: isize,
    h_icon: isize,
    h_cursor: isize,
    h_br_background: isize,
    lpsz_menu_name: isize,
    lpsz_class_name: isize,
}

/// owner-draw 控件创建时询问尺寸（LBS_OWNERDRAWFIXED → 定行高）
#[repr(C)]
struct MEASUREITEMSTRUCT {
    ctl_type: u32,
    ctl_id: u32,
    item_id: u32,
    item_width: u32,
    item_height: u32,
    item_data: usize,
}

/// owner-draw 绘制信息（列表行 / 按钮通用）
/// ★★ 曾漏 item_action 字段 → 后续字段全部错位 4 字节：hdc 拿到的是 hwnd、
///   rc_item 拿到的是 hdc……所有 GDI 调用静默失败，一个像素都画不出，
///   而 ctl_id/item_id 在错位点之前仍正确 → 日志看似全对（连环谜案根因）
#[repr(C)]
struct DRAWITEMSTRUCT {
    ctl_type: u32,
    ctl_id: u32,
    item_id: u32,
    item_action: u32,
    item_state: u32,
    hwnd_item: HWND,
    hdc: isize,
    rc_item: RECT,
    item_data: usize,
}

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(cls: *const u16, title: *const u16) -> HWND;
    fn RegisterClassW(wc: *const WNDCLASSW) -> u16;
    fn DefWindowProcW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    fn CreateWindowExW(
        ex_style: u32, cls: *const u16, name: *const u16, style: u32,
        x: i32, y: i32, w: i32, h: i32,
        parent: HWND, menu: isize, inst: isize, param: *const u8,
    ) -> HWND;
    fn DestroyWindow(hwnd: HWND) -> i32;
    fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const MSG) -> i32;
    fn DispatchMessageW(msg: *const MSG) -> isize;
    fn PostThreadMessageW(tid: u32, msg: u32, wp: usize, lp: isize) -> i32;
    fn PostMessageW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> i32;
    fn PostQuitMessage(code: i32);
    fn SetWindowTextW(hwnd: HWND, text: *const u16) -> i32;
    fn SendMessageW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    fn SetFocus(hwnd: HWND) -> HWND;
    fn GetFocus() -> HWND;
    fn AttachThreadInput(id: u32, to: u32, attach: i32) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    fn GetClientRect(hwnd: HWND, rc: *mut RECT) -> i32;
    fn GetWindowRect(hwnd: HWND, rc: *mut RECT) -> i32;
    fn ClientToScreen(hwnd: HWND, pt: *mut POINT) -> i32;
    fn SetWindowPos(hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
    fn SetTimer(hwnd: HWND, id: usize, ms: u32, cb: usize) -> usize;
    fn KillTimer(hwnd: HWND, id: usize) -> i32;
    fn SetLayeredWindowAttributes(hwnd: HWND, key: u32, alpha: u8, flags: u32) -> i32;
    fn AdjustWindowRect(rc: *mut RECT, style: u32, menu: i32) -> i32;
    fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
    fn InvalidateRect(hwnd: HWND, rc: *const RECT, erase: i32) -> i32;
    fn UpdateWindow(hwnd: HWND) -> i32;
    /// 像素探针诊断（tests 用：屏幕 DC 取色验证自绘是否真的上屏）
    fn GetDC(hwnd: HWND) -> isize;
    fn ReleaseDC(hwnd: HWND, hdc: isize) -> i32;
    fn GetPixel(hdc: isize, x: i32, y: i32) -> u32;
    /// 列表项自绘：填充行背景
    fn FillRect(hdc: isize, rc: *const RECT, brush: isize) -> i32;
    /// 列表项自绘：画文字（在 SelectObject 选入字体后调用）
    fn DrawTextW(hdc: isize, text: *const u16, len: i32, rc: *mut RECT, format: u32) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateFontW(
        height: i32, width: i32, escapement: i32, orientation: i32, weight: i32,
        italic: u32, underline: u32, strike_out: u32, charset: u32, out_prec: u32,
        clip_prec: u32, quality: u32, pitch_family: u32, face: *const u16,
    ) -> isize;
    fn CreateSolidBrush(color: u32) -> isize;
    fn DeleteObject(handle: isize) -> i32;
    fn SetTextColor(hdc: isize, color: u32) -> u32;
    fn SetBkColor(hdc: isize, color: u32) -> u32;
    /// 自绘时把字体选进 DC
    fn SelectObject(hdc: isize, handle: isize) -> isize;
}

// GDI+ flat API：解码 IconSet.png（PNG 解码 GDI/GDI32 做不了，GDI+ 原生支持）
#[repr(C)]
struct GdiplusStartupInput {
    gdiplus_version: u32,
    debug_event_callback: usize,
    suppress_background_thread: i32,
    suppress_external_codecs: i32,
}

#[link(name = "gdiplus")]
extern "system" {
    fn GdiplusStartup(token: *mut usize, input: *const GdiplusStartupInput, output: *mut u8) -> i32;
    fn GdiplusShutdown(token: usize);
    fn GdipCreateBitmapFromFile(file: *const u16, bitmap: *mut *mut u8) -> i32;
    fn GdipDisposeImage(image: *mut u8) -> i32;
    fn GdipCreateFromHDC(hdc: isize, graphics: *mut *mut u8) -> i32;
    fn GdipDeleteGraphics(graphics: *mut u8) -> i32;
    /// 从源图指定矩形画到目标矩形（srcUnit=Pixel，带 alpha 混合）
    fn GdipDrawImageRectRectI(
        graphics: *mut u8, image: *mut u8,
        dst_x: i32, dst_y: i32, dst_w: i32, dst_h: i32,
        src_x: i32, src_y: i32, src_w: i32, src_h: i32,
        src_unit: i32, image_attributes: usize, callback: usize, callback_data: usize,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
}

#[link(name = "ole32")]
extern "system" {
    fn OleInitialize(param: *const u8) -> i32;
    fn OleUninitialize();
}

#[link(name = "imm32")]
extern "system" {
    fn ImmCreateContext() -> HIMC;
    fn ImmAssociateContext(hwnd: HWND, himc: HIMC) -> HIMC;
    fn ImmDestroyContext(himc: HIMC) -> i32;
    fn ImmSetOpenStatus(himc: HIMC, open: i32) -> i32;
}

// ---------------------------------------------------------------------------
// 跨线程共享状态
// ---------------------------------------------------------------------------

#[derive(Default)]
struct AuiState {
    /// 面板线程存活且窗口已创建
    open: bool,
    /// 用户关闭（Esc / X）
    cancelled: bool,
    /// 有一次提交待 Ruby 取
    submit_pending: bool,
    /// 本次提交动作：0 购买 1 上架 2 下架 3 刷新
    action: i32,
    /// 动作参数：listing_id / item_id / quantity / price
    arg_listing: i64,
    arg_item: i64,
    arg_qty: i64,
    arg_price: i64,
    /// Ruby → 面板状态栏文字
    status: String,
    /// Ruby → 面板列表数据（三份互不干扰）
    /// (行 id 列表, 显示文字列表, 参考单价列表[仅背包用], 图标索引列表)
    list_market: (Vec<i64>, Vec<String>, Vec<i64>, Vec<i64>),
    list_mine: (Vec<i64>, Vec<String>, Vec<i64>, Vec<i64>),
    list_bag: (Vec<i64>, Vec<String>, Vec<i64>, Vec<i64>),
    /// 列表脏标记（定时器搬到界面）
    dirty_market: bool,
    dirty_mine: bool,
    dirty_bag: bool,
}

struct AuiShared {
    state: Arc<Mutex<AuiState>>,
    stop: Arc<AtomicBool>,
    thread_id: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

static AUI: Mutex<Option<AuiShared>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// 线程局部句柄
// ---------------------------------------------------------------------------

thread_local! {
    static DLG: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LST_MARKET: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LST_MINE: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    /// 上架页物品选择列表（原组合框，已改 LISTBOX）
    static LST_ITEM: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static EDT_QTY: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static EDT_PRICE: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LBL_STATUS: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    /// 上架页静态标签（★ 必须存句柄：漏存导致市场页叠着「物品:/数量:/单价:」）
    static LBL_ITEM: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LBL_QTY: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LBL_PRICE: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static GAME_HWND: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static STATE: std::cell::RefCell<Option<Arc<Mutex<AuiState>>>> = const { std::cell::RefCell::new(None) };
    static USER_CANCEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LAST_STATUS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// 当前页：0 市场 1 我的 2 上架
    static CUR_PAGE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// 用户手动拖动过窗口（true 后不再自动跟随游戏窗口居中，位置自由）
    static USER_MOVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DARK_BRUSH: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// 页签按钮句柄（BM_SETSTATE 锁定选中页按下视觉用）
    static BTN_TABS: std::cell::Cell<[HWND; 3]> = const { std::cell::Cell::new([0; 3]) };
    /// 页相关按钮句柄：[购买, 下架, 上架提交, 刷新]
    static BTN_PAGES: std::cell::Cell<[HWND; 4]> = const { std::cell::Cell::new([0; 4]) };
    /// 列表行 id 缓存（与界面行一一对应，提交时按下标取回）
    static IDS_MARKET: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    static IDS_MINE: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    static IDS_BAG: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 背包行参考单价缓存（选中物品时自动填价格框）
    static PRICES_BAG: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    /// ★ 自绘行文字缓存（WM_DRAWITEM 只有行号，文字/图标从这里取）
    static TEXTS_MARKET: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static TEXTS_MINE: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static TEXTS_BAG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// ★ 自绘行图标缓存（icon_index，0=无图标）
    static ICONS_MARKET: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    static ICONS_MINE: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    static ICONS_BAG: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
    /// ★ 面板字体（自绘文字时 SelectObject 选入 DC）
    static FONT: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// ★ IconSet 位图（GpBitmap*，GDI+ 解码；0=加载失败→纯文字模式）
    static ICONSET: std::cell::Cell<*mut u8> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    /// ★ GDI+ 启动令牌（dispose 时用）
    static GDIP_TOKEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 选中行高亮画刷（自绘选中背景）
    static SEL_BRUSH: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn to_utf16z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 错误日志（追加写 System/rustnet_aui_debug.log；只在错误路径调用，正常运行为空文件）
fn aui_log(msg: &str) {
    use std::io::Write;
    let path = "System/rustnet_aui_debug.log";
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
}

/// 解析 Ruby 传来的列表数据
/// 行格式："\t" 分隔列，"\n" 分隔行
///   kind0/kind1（市场/我的）: "listing_id\t显示文字\t0\t图标索引"
///   kind2（背包下拉）:       "item_id\t显示文字\t参考单价\t图标索引"
/// 第四列 icon_index 对应 RMVA IconSet.png 的 24x24 格子编号（0=无图标）
fn parse_list(raw: &str) -> (Vec<i64>, Vec<String>, Vec<i64>, Vec<i64>) {
    let mut ids = Vec::new();
    let mut texts = Vec::new();
    let mut prices = Vec::new();
    let mut icons = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(4, '\t');
        let id = it.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        let text = it.next().unwrap_or("").to_string();
        let price = it.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        let icon = it.next().unwrap_or("0").parse::<i64>().unwrap_or(0);
        ids.push(id);
        texts.push(text);
        prices.push(price);
        icons.push(icon);
    }
    (ids, texts, prices, icons)
}

/// 把焦点设到面板控件（跨线程）
fn grab_focus(target: HWND) {
    unsafe {
        let game = GAME_HWND.get();
        let main_tid = GetWindowThreadProcessId(game, std::ptr::null_mut());
        let cur_tid = GetCurrentThreadId();
        if main_tid != 0 && main_tid != cur_tid {
            AttachThreadInput(cur_tid, main_tid, 1);
            SetFocus(target);
            AttachThreadInput(cur_tid, main_tid, 0);
        } else {
            SetFocus(target);
        }
    }
}

/// 面板初始居中到游戏客户区（用户手动拖动过 → 位置自由，不再强制拉回中央）
fn sync_position() {
    // ★ 曾在此无条件每 30ms 拉回中央 → 用户拖走立刻弹回，等于拖不动。
    //   现在拖动过（WM_EXITSIZEMOVE 置位）就完全交还给用户
    if USER_MOVED.get() {
        return;
    }
    unsafe {
        let game = GAME_HWND.get();
        let dlg = DLG.get();
        let mut rc = RECT::default();
        if GetClientRect(game, &mut rc) == 0 {
            return;
        }
        let mut pt = POINT { x: rc.left, y: rc.top };
        if ClientToScreen(game, &mut pt) == 0 {
            return;
        }
        let cw = rc.right - rc.left;
        let ch = rc.bottom - rc.top;
        let x = pt.x + (cw - PANEL_W) / 2;
        let y = pt.y + (ch - PANEL_H) / 2;
        let mut cur = RECT::default();
        if GetWindowRect(dlg, &mut cur) != 0 && (cur.left != x || cur.top != y) {
            SetWindowPos(dlg, 0, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// 焦点被游戏抢走时夺回
fn regrab_focus_if_needed() {
    unsafe {
        if GetFocus() != 0 {
            return;
        }
        let target = match CUR_PAGE.get() {
            1 => LST_MINE.get(),
            2 => LST_ITEM.get(),
            _ => LST_MARKET.get(),
        };
        grab_focus(target);
    }
}

/// 状态栏刷新
fn refresh_status() {
    let want = STATE.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|st| st.lock().ok().map(|g| g.status.clone()))
    });
    if let Some(want) = want {
        let changed = LAST_STATUS.with(|l| {
            let mut last = l.borrow_mut();
            if *last != want {
                *last = want.clone();
                true
            } else {
                false
            }
        });
        if changed {
            unsafe { SetWindowTextW(LBL_STATUS.get(), to_utf16z(&want).as_ptr()) };
        }
    }
}

/// 列表数据刷新（Ruby 写入 state，这里每帧搬到界面）
/// 背包下拉填完后：默认勾选第一项 + 价格框自动填参考单价 + 数量重置为 1
fn refresh_lists() {
    let mut jobs: Vec<(i32, (Vec<i64>, Vec<String>, Vec<i64>, Vec<i64>))> = Vec::new();
    STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            if let Ok(mut g) = st.lock() {
                if g.dirty_market {
                    jobs.push((0, g.list_market.clone()));
                    g.dirty_market = false;
                }
                if g.dirty_mine {
                    jobs.push((1, g.list_mine.clone()));
                    g.dirty_mine = false;
                }
                if g.dirty_bag {
                    jobs.push((2, g.list_bag.clone()));
                    g.dirty_bag = false;
                }
            }
        }
    });
    for (which, (ids, texts, prices, icons)) in jobs {
        let hwnd = if which == 0 {
            LST_MARKET.get()
        } else if which == 1 {
            LST_MINE.get()
        } else {
            LST_ITEM.get()
        };
        unsafe {
            SendMessageW(hwnd, WM_LB_RESETCONTENT, 0, 0);
            for t in &texts {
                let w = to_utf16z(t);
                SendMessageW(hwnd, WM_LB_ADDSTRING, 0, w.as_ptr() as isize);
            }
            match which {
                // ★ 自绘行需要：文字 + 图标缓存与界面行一一对应
                0 => {
                    IDS_MARKET.with(|c| *c.borrow_mut() = ids);
                    TEXTS_MARKET.with(|c| *c.borrow_mut() = texts.clone());
                    ICONS_MARKET.with(|c| *c.borrow_mut() = icons);
                    // ★ 列表重置会清空选择 → 恢复选中第 0 行
                    //   （否则回包刷新后双击操作读不到选中行，误报"请先选中一条"）
                    let sel = if texts.is_empty() { -1 } else { 0 };
                    SendMessageW(hwnd, WM_LB_SETCURSEL, sel as usize, 0);
                }
                1 => {
                    IDS_MINE.with(|c| *c.borrow_mut() = ids);
                    TEXTS_MINE.with(|c| *c.borrow_mut() = texts.clone());
                    ICONS_MINE.with(|c| *c.borrow_mut() = icons);
                    let sel = if texts.is_empty() { -1 } else { 0 };
                    SendMessageW(hwnd, WM_LB_SETCURSEL, sel as usize, 0);
                }
                _ => {
                    // 背包：缓存 id 与参考价
                    IDS_BAG.with(|c| *c.borrow_mut() = ids);
                    TEXTS_BAG.with(|c| *c.borrow_mut() = texts.clone());
                    ICONS_BAG.with(|c| *c.borrow_mut() = icons);
                    PRICES_BAG.with(|c| *c.borrow_mut() = prices);
                    // 默认勾选第一项（无数据则清空选择）
                    let sel = if texts.is_empty() { -1 } else { 0 };
                    SendMessageW(hwnd, WM_LB_SETCURSEL, sel as usize, 0);
                    // 数量默认 1；价格填选中物品的参考单价
                    apply_sell_defaults();
                }
            }
        }
    }
}

/// 上架页默认值：数量=1，价格=当前选中物品的参考单价（游戏内价格）
fn apply_sell_defaults() {
    unsafe {
        let one = to_utf16z("1");
        SetWindowTextW(EDT_QTY.get(), one.as_ptr());
        let price = bag_selected_price();
        let p = to_utf16z(&price.to_string());
        SetWindowTextW(EDT_PRICE.get(), p.as_ptr());
    }
}

/// 当前背包列表选中行的参考单价（无选中/无数据返回 0）
fn bag_selected_price() -> i64 {
    unsafe {
        let idx = SendMessageW(LST_ITEM.get(), WM_LB_GETCURSEL, 0, 0) as i32;
        if idx < 0 {
            return 0;
        }
        PRICES_BAG.with(|c| c.borrow().get(idx as usize).copied()).unwrap_or(0)
    }
}

/// 读列表当前选中行的 id（未选中返回 -1）
fn current_id(which: i32) -> i64 {
    unsafe {
        let hwnd = if which == 2 {
            LST_ITEM.get()
        } else if which == 1 {
            LST_MINE.get()
        } else {
            LST_MARKET.get()
        };
        let idx = SendMessageW(hwnd, WM_LB_GETCURSEL, 0, 0) as i32;
        if idx < 0 {
            return -1;
        }
        let id = match which {
            0 => IDS_MARKET.with(|c| c.borrow().get(idx as usize).copied()),
            1 => IDS_MINE.with(|c| c.borrow().get(idx as usize).copied()),
            _ => IDS_BAG.with(|c| c.borrow().get(idx as usize).copied()),
        };
        id.unwrap_or(-1)
    }
}

/// 读数字输入框内容（非法/空返回 0）
fn read_number(hwnd: HWND) -> i64 {
    let text = read_edit_text(hwnd);
    text.trim().parse::<i64>().unwrap_or(0)
}

/// ★ 列表行自绘：深色底 + 真实物品图标（IconSet 24x24）+ 白字
///   只有三个 LBS_OWNERDRAWFIXED 列表会走到这里（按钮保持系统绘制）
unsafe fn draw_list_item(dis: *const DRAWITEMSTRUCT) {
    let dis = unsafe { &*dis };
    let idx = dis.item_id as usize;
    // 按控件取对应缓存（文字 + 图标索引）
    let (text, icon) = match dis.ctl_id as usize {
        LIST_MARKET => {
            let t = TEXTS_MARKET.with(|c| c.borrow().get(idx).cloned().unwrap_or_default());
            let i = ICONS_MARKET.with(|c| c.borrow().get(idx).copied().unwrap_or(0));
            (t, i)
        }
        LIST_MINE => {
            let t = TEXTS_MINE.with(|c| c.borrow().get(idx).cloned().unwrap_or_default());
            let i = ICONS_MINE.with(|c| c.borrow().get(idx).copied().unwrap_or(0));
            (t, i)
        }
        _ => {
            let t = TEXTS_BAG.with(|c| c.borrow().get(idx).cloned().unwrap_or_default());
            let i = ICONS_BAG.with(|c| c.borrow().get(idx).copied().unwrap_or(0));
            (t, i)
        }
    };
    let selected = (dis.item_state & ODS_SELECTED) != 0;
    let bg_color = if selected { COLOR_SEL } else { COLOR_BG };
    let bg_brush = if selected { SEL_BRUSH.get() } else { DARK_BRUSH.get() };
    unsafe {
        // 1) 行背景
        let mut rc = dis.rc_item;
        // 兜底：系统给的行矩形若不足行高，撑到 ITEM_H（点击命中与绘制矩形无关）
        if rc.bottom - rc.top < ITEM_H {
            rc.bottom = rc.top + ITEM_H;
        }
        FillRect(dis.hdc, &rc, bg_brush);
        // 2) 图标（IconSet 加载成功且 icon_index>0 才画；用撑高后的 rc 垂直居中）
        let iconset = ICONSET.get();
        if !iconset.is_null() && icon > 0 {
            let ic = icon as i32;
            let src_x = (ic % ICON_GRID) * ICON_SIZE;
            let src_y = (ic / ICON_GRID) * ICON_SIZE;
            let dy = rc.top + (ITEM_H - ICON_SIZE) / 2;
            let mut gfx: *mut u8 = std::ptr::null_mut();
            if GdipCreateFromHDC(dis.hdc, &mut gfx) == 0 && !gfx.is_null() {
                GdipDrawImageRectRectI(
                    gfx, iconset,
                    rc.left + 4, dy, ICON_SIZE, ICON_SIZE,
                    src_x, src_y, ICON_SIZE, ICON_SIZE,
                    GP_UNIT_PIXEL, 0, 0, 0,
                );
                GdipDeleteGraphics(gfx);
            }
        }
        // 3) 文字（图标右侧留白，垂直居中于撑高后的行）
        if !text.is_empty() {
            let font = FONT.get();
            if font != 0 {
                SelectObject(dis.hdc, font);
            }
            SetTextColor(dis.hdc, COLOR_FG);
            SetBkColor(dis.hdc, bg_color);
            let w = to_utf16z(&text);
            let mut trc = rc;
            trc.left += ICON_SIZE + 10;
            trc.right -= 4;
            DrawTextW(dis.hdc, w.as_ptr(), -1, &mut trc, DT_LEFT_VCENTER_SINGLE);
        }
    }
}

/// 切换页：显示对应控件组（含静态标签——漏存标签句柄是上轮重叠 bug 的根因）
fn switch_page(page: i32) {
    CUR_PAGE.set(page);
    unsafe {
        let market = page == 0;
        let mine = page == 1;
        let sell = page == 2;
        ShowWindow(LST_MARKET.get(), if market { SW_SHOW } else { SW_HIDE });
        ShowWindow(LST_MINE.get(), if mine { SW_SHOW } else { SW_HIDE });
        // 上架页控件：物品列表 + 数量/单价输入框 + 三个静态标签
        for h in [
            LST_ITEM.get(), EDT_QTY.get(), EDT_PRICE.get(),
            LBL_ITEM.get(), LBL_QTY.get(), LBL_PRICE.get(),
        ] {
            ShowWindow(h, if sell { SW_SHOW } else { SW_HIDE });
        }
        BTN_PAGES.with(|b| {
            let [hb, hc, hs, hr] = b.get();
            // hb=购买 hc=下架 hs=上架提交 hr=刷新
            ShowWindow(hb, if market { SW_SHOW } else { SW_HIDE });
            ShowWindow(hc, if mine { SW_SHOW } else { SW_HIDE });
            ShowWindow(hs, if sell { SW_SHOW } else { SW_HIDE });
            // ★ 刷新按钮三页共用（上架页刷新=重新喂背包，市场/我的=拉服务器列表）
            ShowWindow(hr, SW_SHOW);
        });
        // 焦点移到当前页主控件
        let target = if market {
            LST_MARKET.get()
        } else if mine {
            LST_MINE.get()
        } else {
            LST_ITEM.get()
        };
        grab_focus(target);
        // 页签「按下去的效果」：选中页签用 BM_SETSTATE 锁定按下视觉（系统按钮样式）
        BTN_TABS.with(|t| {
            let tabs = t.get();
            for (i, h) in tabs.iter().enumerate() {
                if *h != 0 {
                    // 页签序号 i 与页号一一对应（0 市场 1 我的 2 上架）
                    let _ = SendMessageW(*h, BM_SETSTATE, usize::from(i as i32 == page), 0);
                }
            }
        });
        // ★ 三保险：① 先 SHOW/HIDE 再 ② 强制父窗口擦背景刷新 + ③ 立即重绘
        //   防止 ShowWindow 由于对话框管理器拦截/脏区合并导致旧控件残留
        let dlg = DLG.get();
        if dlg != 0 {
            InvalidateRect(dlg, std::ptr::null(), 1);
            UpdateWindow(dlg);
        }
    }
}

/// 提交动作
fn do_submit(action: i32) {
    // IME 组合中不提交
    if is_composing(EDT_QTY.get()) || is_composing(EDT_PRICE.get()) {
        return;
    }
    let (listing, item, qty, price) = match action {
        ACT_BUY => {
            let id = current_id(0);
            if id < 0 {
                set_status_text("请先在市场列表选中一件商品");
                return;
            }
            (id, 0, 0, 0)
        }
        ACT_CANCEL => {
            let id = current_id(1);
            if id < 0 {
                set_status_text("请先在我的挂单中选中一条");
                return;
            }
            (id, 0, 0, 0)
        }
        ACT_SELL => {
            let item = current_id(2);
            if item < 0 {
                set_status_text("请先选择要上架的物品");
                return;
            }
            let qty = read_number(EDT_QTY.get());
            let price = read_number(EDT_PRICE.get());
            if qty <= 0 {
                set_status_text("数量需为正整数");
                return;
            }
            if price <= 0 {
                set_status_text("单价需为正整数");
                return;
            }
            (0, item, qty, price)
        }
        _ => (0, 0, 0, 0), // 刷新不需要参数
    };
    let shared = STATE.with(|s| s.borrow().clone());
    if let Some(st) = shared {
        if let Ok(mut g) = st.lock() {
            if g.open {
                g.action = action;
                g.arg_listing = listing;
                g.arg_item = item;
                g.arg_qty = qty;
                g.arg_price = price;
                g.submit_pending = true;
                g.status = "处理中…".to_string();
            }
        }
    }
}

fn set_status_text(text: &str) {
    let shared = STATE.with(|s| s.borrow().clone());
    if let Some(st) = shared {
        if let Ok(mut g) = st.lock() {
            g.status = text.to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

unsafe extern "system" fn aui_wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_COMMAND => {
            // 高 16 位是通知码，低 16 位是控件 ID
            let id = wp & WM_COMMAND_ID;
            let code = (wp >> 16) & WM_COMMAND_ID;
            match id {
                BTN_TAB_MARKET => switch_page(0),
                BTN_TAB_MINE => switch_page(1),
                BTN_TAB_SELL => switch_page(2),
                BTN_BUY => do_submit(ACT_BUY),
                BTN_CANCEL_L => do_submit(ACT_CANCEL),
                BTN_REFRESH => do_submit(ACT_REFRESH),
                BTN_DO_SELL => do_submit(ACT_SELL),
                LIST_MARKET if code == (LBN_DBLCLK as usize) => do_submit(ACT_BUY),
                LIST_MINE if code == (LBN_DBLCLK as usize) => do_submit(ACT_CANCEL),
                // 上架页切换物品 → 价格框自动跟随游戏参考价
                LIST_ITEM if code == (LBN_SELCHANGE as usize) => apply_sell_defaults(),
                _ => {}
            }
            0
        }
        // ★ owner-draw 列表：行高（图标 24 + 边距）
        WM_MEASUREITEM => {
            let mi = lp as *mut MEASUREITEMSTRUCT;
            unsafe {
                (*mi).item_height = ITEM_H as u32;
            }
            1
        }
        // ★ owner-draw 列表：画每行（深底 + 物品图标 + 白字）
        //   常量 0x002B（曾误写 0x0009 → 自绘从未触发，白底方块的真凶）
        WM_DRAWITEM => {
            let dis = lp as *const DRAWITEMSTRUCT;
            let ctl = unsafe { (*dis).ctl_id };
            if matches!(ctl as usize, LIST_MARKET | LIST_MINE | LIST_ITEM) {
                unsafe { draw_list_item(dis) };
                1
            } else {
                0
            }
        }
        WM_CLOSE => {
            USER_CANCEL.set(true);
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_TIMER => {
            if wp == 1 {
                sync_position();
                regrab_focus_if_needed();
                refresh_status();
                refresh_lists();
            }
            0
        }
        // 用户拖动结束（标题栏拖动）→ 记住，此后不再自动拉回中央
        WM_EXITSIZEMOVE => {
            USER_MOVED.set(true);
            0
        }
        // ★ 自擦背景（双保险：类背景刷之外的兜底，隐藏控件像素残留必被清）
        WM_ERASEBKGND => {
            let hdc = wp as isize;
            let mut rc = RECT::default();
            if unsafe { GetClientRect(hwnd, &mut rc) } != 0 {
                unsafe { FillRect(hdc, &rc, DARK_BRUSH.get()) };
            }
            1
        }
        // 深色主题
        WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            let hdc = wp as isize;
            unsafe {
                SetTextColor(hdc, COLOR_FG);
                SetBkColor(hdc, COLOR_BG);
            }
            DARK_BRUSH.get()
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ---------------------------------------------------------------------------
// 面板线程
// ---------------------------------------------------------------------------

fn aui_thread_main(state: Arc<Mutex<AuiState>>, stop: Arc<AtomicBool>, thread_id: Arc<AtomicU32>) {
    // 面板线程 panic 也写日志（裸 panic 线程直接死，无任何痕迹）
    let result = catch_unwind(AssertUnwindSafe(|| {
        aui_thread_body(state, stop, thread_id)
    }));
    if let Err(_) = result {
        aui_log("[thread] ★ 面板线程 panic 退出");
    }
}

fn aui_thread_body(state: Arc<Mutex<AuiState>>, stop: Arc<AtomicBool>, thread_id: Arc<AtomicU32>) {
    unsafe { OleInitialize(std::ptr::null()) };

    // 找到游戏窗口
    let class = to_utf16z("RGSS Player");
    let game = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if game == 0 {
        aui_log("[thread] ★ 找不到游戏窗口(RGSS Player)，线程直接退出");
        if let Ok(mut g) = state.lock() {
            g.open = false;
        }
        unsafe { OleUninitialize() };
        return;
    }
    GAME_HWND.set(game);

    // 注册窗口类（★ hInstance 与 CreateWindowExW 保持一致，同 authui 修复）
    let cls_name = to_utf16z("RGSS_P2PAuctionUI");
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    unsafe {
        // ★ 背景刷先建再注册类：h_br_background 非 0 → InvalidateRect(erase=1)
        //   时系统自动用深色刷擦掉隐藏控件残留像素（页切换叠影修复）
        DARK_BRUSH.set(CreateSolidBrush(COLOR_BG));
        let wc = WNDCLASSW {
            style: 0,
            lpfn_wnd_proc: aui_wndproc,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: hinst,
            h_icon: 0,
            h_cursor: 0,
            h_br_background: DARK_BRUSH.get(),
            lpsz_menu_name: 0,
            lpsz_class_name: cls_name.as_ptr() as isize,
        };
        RegisterClassW(&wc);
        // 新线程 → 拖动标记天然重置（thread_local 初值 false），每次打开重新默认居中
    }

    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut rc = RECT { left: 0, top: 0, right: PANEL_W, bottom: PANEL_H };
    unsafe { AdjustWindowRect(&mut rc, style, 0) };
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;

    let title = to_utf16z("P2P 拍卖行");
    let dlg = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW,
            cls_name.as_ptr(), title.as_ptr(), style | WS_VISIBLE,
            0, 0, win_w, win_h,
            game, 0, hinst, std::ptr::null(),
        )
    };
    if dlg == 0 {
        aui_log("[thread] ★ CreateWindowExW 失败，线程退出");
        if let Ok(mut g) = state.lock() {
            g.open = false;
        }
        unsafe { OleUninitialize() };
        return;
    }
    DLG.set(dlg);

    // 字体
    let face: Vec<u16> = "微软雅黑".encode_utf16().chain(std::iter::once(0)).collect();
    let font = unsafe {
        CreateFontW(FONT_H, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET,
                    0, 0, CLEARTYPE_QUALITY, DEFAULT_PITCH, face.as_ptr())
    };
    FONT.set(font);

    // ---- GDI+：加载游戏 IconSet（真实物品图标数据源） ----
    // ★ 失败不致命：自绘降级为纯文字（IconSet 路径随游戏目录，cwd=游戏根目录）
    unsafe {
        let mut token: usize = 0;
        let input = GdiplusStartupInput {
            gdiplus_version: 1,
            debug_event_callback: 0,
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        };
        if GdiplusStartup(&mut token, &input, std::ptr::null_mut()) == 0 {
            GDIP_TOKEN.set(token);
            let path = to_utf16z("Graphics/System/IconSet.png");
            let mut bmp: *mut u8 = std::ptr::null_mut();
            if GdipCreateBitmapFromFile(path.as_ptr(), &mut bmp) == 0 && !bmp.is_null() {
                ICONSET.set(bmp);
            } else {
                aui_log("[gdi+] IconSet.png 加载失败 → 列表降级纯文字");
            }
        } else {
            aui_log("[gdi+] GdiplusStartup 失败 → 列表降级纯文字");
        }
        // 选中行高亮画刷
        SEL_BRUSH.set(CreateSolidBrush(COLOR_SEL));
    }

    // ---- 控件创建 ----
    // ★ 按钮走系统默认绘制（自绘曾导致面板打不开/文字方块，全面回退）。
    //   选中页签的「按下去」视觉由 switch_page 的 BM_SETSTATE 锁定
    let btn_style = WS_TABSTOP;
    let lbl_status_t = to_utf16z("");
    let tab_m_t = to_utf16z("市场");
    let tab_i_t = to_utf16z("我的挂单");
    let tab_s_t = to_utf16z("上架");
    let b_buy_t = to_utf16z("购买");
    let b_cancel_t = to_utf16z("下架");
    let b_sell_t = to_utf16z("上架");
    let b_refresh_t = to_utf16z("刷新");
    let lb_item_t = to_utf16z("物品:");
    let lb_qty_t = to_utf16z("数量:");
    let lb_price_t = to_utf16z("单价:");
    let edit_cls = to_utf16z("EDIT");
    let static_cls = to_utf16z("STATIC");
    let button_cls = to_utf16z("BUTTON");
    let list_cls = to_utf16z("LISTBOX");
    // ★ make 不再默认加 WS_VISIBLE（双保险：之前所有控件创建时立刻可见，
    //   若 ShowWindow 失效则三页控件全叠在一起——本版改为创建时全不可见，
    //   只有 switch_page 显式 SHOW 的控件才会显示）
    let make = |cls: *const u16, name: *const u16, st: u32,
                x: i32, y: i32, w: i32, h: i32, menu: usize| unsafe {
        let h = CreateWindowExW(0, cls, name, WS_CHILD | st,
                                x, y, w, h, dlg, menu as isize, 0, std::ptr::null());
        if font != 0 {
            SendMessageW(h, WM_SETFONT, font as usize, 1);
        }
        h
    };
    // 页签按钮（y=10，系统按钮 + 文字；选中按下视觉由 BM_SETSTATE 锁定）
    let tab_m = make(button_cls.as_ptr(), tab_m_t.as_ptr(), btn_style, 12, 10, 88, 28, BTN_TAB_MARKET);
    let tab_i = make(button_cls.as_ptr(), tab_i_t.as_ptr(), btn_style, 108, 10, 100, 28, BTN_TAB_MINE);
    let tab_s = make(button_cls.as_ptr(), tab_s_t.as_ptr(), btn_style, 216, 10, 88, 28, BTN_TAB_SELL);

    // 市场列表（y=50..250）
    // ★ LBS_OWNERDRAWFIXED 行自绘：深底 + 物品图标 + 白字。
    //   之前"自绘不上屏"的真凶是 DRAWITEMSTRUCT 漏 item_action 字段导致
    //   hdc/hwnd 错位（GDI 全静默失败），已修复——本注释留档防回退。
    let lb_style = WS_BORDER | WS_VSCROLL | WS_TABSTOP | LBS_NOTIFY
        | LBS_NOINTEGRALHEIGHT | LBS_OWNERDRAWFIXED;
    let lst_m = make(list_cls.as_ptr(), std::ptr::null(),
                     lb_style, 12, 50, 436, 200, LIST_MARKET);
    // 我的挂单列表（初始隐藏）
    let lst_i = make(list_cls.as_ptr(), std::ptr::null(),
                     lb_style, 12, 50, 436, 200, LIST_MINE);

    // 上架页控件（初始隐藏）：物品列表 + 数量 + 单价
    // ★ 布局「替换式整页」：物品列表占满内容区上半（与市场列表同宽），
    //   数量/单价一行放底部——三页视觉统一为整页切换，不再半截留白
    let lbl_item = make(static_cls.as_ptr(), lb_item_t.as_ptr(), 0, 12, 54, 44, 20, 0);
    let lst_item = make(list_cls.as_ptr(), std::ptr::null(),
                        lb_style, 60, 50, 388, 140, LIST_ITEM);
    let lbl_qty = make(static_cls.as_ptr(), lb_qty_t.as_ptr(), 0, 12, 212, 44, 20, 0);
    let edt_q = make(edit_cls.as_ptr(), std::ptr::null(),
                     WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL | ES_NUMBER,
                     60, 208, 100, 24, EDIT_QTY);
    let lbl_price = make(static_cls.as_ptr(), lb_price_t.as_ptr(), 0, 180, 212, 44, 20, 0);
    let edt_p = make(edit_cls.as_ptr(), std::ptr::null(),
                     WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL | ES_NUMBER,
                     228, 208, 220, 24, EDIT_PRICE);

    // 操作按钮（y=268，系统按钮 + 文字）
    let hb = make(button_cls.as_ptr(), b_buy_t.as_ptr(), btn_style, 12, 268, 100, 30, BTN_BUY);
    let hc = make(button_cls.as_ptr(), b_cancel_t.as_ptr(), btn_style, 12, 268, 100, 30, BTN_CANCEL_L);
    let hs = make(button_cls.as_ptr(), b_sell_t.as_ptr(), btn_style, 12, 268, 100, 30, BTN_DO_SELL);
    let hr = make(button_cls.as_ptr(), b_refresh_t.as_ptr(), btn_style, 348, 268, 100, 30, BTN_REFRESH);

    // 状态栏（y=312）
    let ls = make(static_cls.as_ptr(), lbl_status_t.as_ptr(), SS_CENTER, 20, 312, 420, 24, 0);

    LST_MARKET.set(lst_m);
    LST_MINE.set(lst_i);
    LST_ITEM.set(lst_item);
    EDT_QTY.set(edt_q);
    EDT_PRICE.set(edt_p);
    LBL_STATUS.set(ls);
    LBL_ITEM.set(lbl_item);
    LBL_QTY.set(lbl_qty);
    LBL_PRICE.set(lbl_price);
    BTN_TABS.with(|t| t.set([tab_m, tab_i, tab_s]));
    BTN_PAGES.with(|b| b.set([hb, hc, hs, hr]));

    // ★ 全局常显控件（始终 SHOW）：面板标题栏系统自绘、页签按钮、状态栏
    //   其他所有控件（三个列表、上架页输入框/标签、页相关按钮）全交给 switch_page 管
    unsafe {
        ShowWindow(tab_m, SW_SHOW);
        ShowWindow(tab_i, SW_SHOW);
        ShowWindow(tab_s, SW_SHOW);
        ShowWindow(ls, SW_SHOW);
        // ★ 显式设 owner-draw 行高（LB_SETITEMHEIGHT 消息，比 WM_MEASUREITEM 可靠：
        //   后者只在 CreateWindow 瞬间发一次，若时序错过行高就永远是默认值）
        for h in [lst_m, lst_i, lst_item] {
            SendMessageW(h, WM_LB_SETITEMHEIGHT, ITEM_H as usize, 0);
        }
    }

    // 整窗半透明
    unsafe { SetLayeredWindowAttributes(dlg, 0, PANEL_ALPHA, LWA_ALPHA) };

    // IME（数字输入框保持输入上下文一致处理）
    let himc = unsafe { ImmCreateContext() };
    if himc != 0 {
        unsafe {
            ImmAssociateContext(edt_q, himc);
            ImmAssociateContext(edt_p, himc);
            ImmSetOpenStatus(himc, 1);
        }
    }

    // 定时器
    unsafe { SetTimer(dlg, 1, SYNC_TIMER_MS, 0) };

    if let Ok(mut g) = state.lock() {
        g.status = "载入中…".to_string();
        // 打开时把已有数据标脏（Ruby 可能先喂了数据再 open）
        g.dirty_market = true;
        g.dirty_mine = true;
        g.dirty_bag = true;
    }
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    sync_position();
    switch_page(0);

    // ---- 消息循环 ----
    loop {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let r = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
        if r <= 0 {
            // r == 0 是正常 WM_QUIT；r == -1 是错误（日志留痕）
            if r == -1 {
                aui_log("[loop] ★ GetMessageW 返回 -1，异常退出消息循环");
            }
            break;
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if msg.message == WM_APP_WAKE && msg.hwnd == 0 {
            continue; // 唤醒消息
        }
        // 键盘：Esc 关闭（列表页 Tab 在列表间切换）
        if msg.hwnd != 0 && handle_hotkeys(dlg, &msg) {
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // ---- 清理 ----
    unsafe {
        KillTimer(dlg, 1);
        if himc != 0 {
            ImmAssociateContext(edt_q, 0);
            ImmAssociateContext(edt_p, 0);
            ImmDestroyContext(himc);
        }
        DestroyWindow(dlg);
        if font != 0 {
            DeleteObject(font);
            FONT.set(0);
        }
        let brush = DARK_BRUSH.get();
        if brush != 0 {
            DeleteObject(brush);
            DARK_BRUSH.set(0);
        }
        let sel = SEL_BRUSH.get();
        if sel != 0 {
            DeleteObject(sel);
            SEL_BRUSH.set(0);
        }
        // GDI+：释放 IconSet 位图（关面板重复开关不泄漏）
        let bmp = ICONSET.get();
        if !bmp.is_null() {
            GdipDisposeImage(bmp);
            ICONSET.set(std::ptr::null_mut());
        }
        let token = GDIP_TOKEN.get();
        if token != 0 {
            GdiplusShutdown(token);
            GDIP_TOKEN.set(0);
        }
    }
    if let Ok(mut g) = state.lock() {
        g.open = false;
        if USER_CANCEL.get() {
            g.cancelled = true;
        }
    }
    STATE.with(|s| *s.borrow_mut() = None);
    unsafe { OleUninitialize() };
    thread_id.store(0, Ordering::Relaxed);
}

/// Esc 关面板；列表页 Tab 在列表与刷新按钮间轮转（返回 true 表示已消费）
fn handle_hotkeys(dlg: HWND, msg: &MSG) -> bool {
    unsafe {
        if msg.message != WM_KEYDOWN {
            return false;
        }
        if msg.w_param == 27 {
            // Esc → 关闭
            PostMessageW(dlg, WM_CLOSE, 0, 0);
            return true;
        }
        if msg.w_param == 9 && CUR_PAGE.get() != 2 {
            // Tab：市场页在列表与按钮间切换不实用，直接忽略交给控件
            return false;
        }
        // ★ 方向键切页：→ 下一页签，← 上一页签（列表/页签聚焦时；
        //   EDIT 聚焦时不拦——数量/价格框里左右键是移动光标）
        if msg.w_param == VK_RIGHT || msg.w_param == VK_LEFT {
            let focus = GetFocus();
            let in_edit = focus == EDT_QTY.get() || focus == EDT_PRICE.get();
            if !in_edit && focus != 0 {
                let cur = CUR_PAGE.get();
                let next = if msg.w_param == VK_RIGHT {
                    (cur + 1) % 3
                } else {
                    (cur + 2) % 3 // 左：+2 ≡ -1 (mod 3)
                };
                if next != cur {
                    switch_page(next);
                }
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// 导出函数（stdcall）
// ---------------------------------------------------------------------------

/// 打开拍卖行面板（幂等）
#[no_mangle]
pub unsafe extern "system" fn net_aui_open() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = AUI.lock() else {
            return -99;
        };
        if let Some(shared) = guard.as_ref() {
            let active = shared.state.lock().map(|s| s.open).unwrap_or(false);
            if active {
                return 0;
            }
        }
        if let Some(mut old) = guard.take() {
            old.stop.store(true, Ordering::Relaxed);
            let tid = old.thread_id.load(Ordering::Relaxed);
            if tid != 0 {
                unsafe { PostThreadMessageW(tid, WM_APP_WAKE, 0, 0) };
            }
            if let Some(h) = old.worker.take() {
                let _ = h.join();
            }
        }
        // open 预置 true（避免轮询竞态，同 authui）
        let state = Arc::new(Mutex::new(AuiState {
            open: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (s2, p2, t2) = (state.clone(), stop.clone(), thread_id.clone());
        let worker = std::thread::spawn(move || {
            unsafe { t2.store(GetCurrentThreadId(), Ordering::Relaxed) };
            aui_thread_main(s2, p2, t2);
        });
        *guard = Some(AuiShared {
            state,
            stop,
            thread_id,
            worker: Some(worker),
        });
        0
    }))
    .unwrap_or(-99)
}

/// 轮询：0 打开中 1 有提交 2 用户取消 3 未打开
#[no_mangle]
pub unsafe extern "system" fn net_aui_poll() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = AUI.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return 3;
        };
        let Ok(mut st) = shared.state.lock() else {
            return -99;
        };
        if st.submit_pending {
            st.submit_pending = false;
            return 1;
        }
        if st.cancelled {
            return 2;
        }
        if st.open {
            return 0;
        }
        3
    }))
    .unwrap_or(-99)
}

/// 本次提交动作：0 购买 1 上架 2 下架 3 刷新
#[no_mangle]
pub unsafe extern "system" fn net_aui_get_action() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = AUI.lock() else {
            return -1;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        shared.state.lock().map(|s| s.action).unwrap_or(-1)
    }))
    .unwrap_or(-1)
}

/// 取动作参数：0 listing_id 1 item_id 2 quantity 3 price
#[no_mangle]
pub unsafe extern "system" fn net_aui_get_int(which: i32) -> i64 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = AUI.lock() else {
            return -1;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        let Ok(st) = shared.state.lock() else {
            return -1;
        };
        match which {
            0 => st.arg_listing,
            1 => st.arg_item,
            2 => st.arg_qty,
            3 => st.arg_price,
            _ => -1,
        }
    }))
    .unwrap_or(-1)
}

/// 填充列表数据（kind 0=市场 1=我的 2=背包下拉），UTF-8 文本
#[no_mangle]
pub unsafe extern "system" fn net_aui_set_list(kind: i32, ptr: *const u8, len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if ptr.is_null() || len < 0 || !(0..=2).contains(&kind) {
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        let raw = String::from_utf8_lossy(bytes).to_string();
        let (ids, texts, prices, icons) = parse_list(&raw);
        let Ok(guard) = AUI.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        if let Ok(mut st) = shared.state.lock() {
            match kind {
                0 => {
                    st.list_market = (ids, texts, prices, icons);
                    st.dirty_market = true;
                }
                1 => {
                    st.list_mine = (ids, texts, prices, icons);
                    st.dirty_mine = true;
                }
                _ => {
                    st.list_bag = (ids, texts, prices, icons);
                    st.dirty_bag = true;
                }
            }
        } else {
            return -99;
        }
        0
    }))
    .unwrap_or(-99)
}

/// 设置状态栏文字（UTF-8）
#[no_mangle]
pub unsafe extern "system" fn net_aui_set_status(ptr: *const u8, len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if ptr.is_null() || len < 0 {
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        let s = String::from_utf8_lossy(bytes).to_string();
        let Ok(guard) = AUI.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        if let Ok(mut st) = shared.state.lock() {
            st.status = s;
        } else {
            return -99;
        }
        0
    }))
    .unwrap_or(-99)
}

/// 关闭面板并回收线程（幂等）。net_shutdown 也调用。
pub(crate) fn aui_internal_close() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = AUI.lock() else {
            return -99;
        };
        if let Some(mut shared) = guard.take() {
            shared.stop.store(true, Ordering::Relaxed);
            let tid = shared.thread_id.load(Ordering::Relaxed);
            if tid != 0 {
                unsafe { PostThreadMessageW(tid, WM_APP_WAKE, 0, 0) };
            }
            if let Some(h) = shared.worker.take() {
                let _ = h.join();
            }
        }
        0
    }))
    .unwrap_or(-99)
}

/// 关闭面板（幂等）
#[no_mangle]
pub unsafe extern "system" fn net_aui_close() -> i32 {
    aui_internal_close()
}
