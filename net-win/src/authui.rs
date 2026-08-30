//! 登录/注册面板（DLL 原生窗口，独立线程 + 系统输入法支持）
//!
//! 与 input.rs（聊天输入条）同一路线：RGSS Player 主线程被禁用 IME，
//! 面板开在 DLL 自己的线程上，输入法原生接管（微软拼音/搜狗可直接打中文用户名）。
//! 相比 Ruby 侧 Scene_P2PLogin + 两次借用聊天输入条的旧方案：
//!   - 用户名/密码两个输入框一屏完成，Tab 切换，Enter=登录，Esc=取消
//!   - 服务器返回的错误（密码太短/用户名占用等）直接显示在面板状态栏，可就地重试
//!   - 不再借道右上角 Toast，也不与聊天记录栏抢位置
//!
//! 导出函数（stdcall）：
//!   net_authui_open()                 -> 0 成功（幂等；窗口创建失败 -1）
//!   net_authui_poll()                 -> 0 面板打开中 1 有提交待取 2 用户已取消 3 未打开
//!   net_authui_get_field(which,buf,n) -> 拷贝 UTF-8 字节数（which 0=用户名 1=密码）
//!   net_authui_get_mode()             -> 最近一次提交的模式：0=登录 1=注册
//!   net_authui_set_status(ptr,len)    -> 设置面板状态栏文字（UTF-8），0 成功
//!   net_authui_close()                -> 0 成功（幂等）

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// 复用 input.rs 的 RichEdit 文本读取与 IME 组合检测
use crate::input::{is_composing, read_edit_text};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 面板客户区尺寸
const PANEL_W: i32 = 400;
const PANEL_H: i32 = 180;
/// 整窗不透明度（LWA_ALPHA，与聊天输入条同款 200 → 半透明观感）
const PANEL_ALPHA: u8 = 200;
/// 位置/焦点/状态同步定时器间隔（毫秒）
const SYNC_TIMER_MS: u32 = 30;

// 窗口样式
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const ES_AUTOHSCROLL: u32 = 0x0000_0080;
const ES_PASSWORD: u32 = 0x0000_0020;
/// 默认按钮（Enter 直接触发登录；系统绘制，样式回退版本）
const BS_DEFPUSHBUTTON: u32 = 0x0000_0001;
const SS_CENTER: u32 = 0x0000_0001;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const LWA_ALPHA: u32 = 2;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;

// 消息
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_SETFONT: u32 = 0x0030;
const WM_TIMER: u32 = 0x0113;
const WM_COMMAND: u32 = 0x0111;
const WM_CTLCOLOREDIT: u32 = 0x0133;
const WM_CTLCOLORDLG: u32 = 0x0136;
const WM_CTLCOLORSTATIC: u32 = 0x0138;
/// 拖动结束（用户手动移动过 → 不再自动居中，标题栏拖动）
const WM_EXITSIZEMOVE: u32 = 0x0232;
/// 自定义唤醒消息（net_authui_close 跨线程叫醒消息循环用）
const WM_APP_WAKE: u32 = 0x8001;

// 对话框管理消息（IsDialogMessageW 的 Enter/Esc 转换目标）
const IDOK_V: usize = 1;
const IDCANCEL_V: usize = 2;

// 控件 ID（按钮用；编辑框 ID 置 0 以吞掉 EN_CHANGE 通知）
const BTN_LOGIN: usize = 1001;
const BTN_REG: usize = 1002;
const BTN_CANCEL: usize = 1003;

// 控件 ID 常量（CreateWindowExW 的 hmenu 参数兼作子控件 ID）
const ID_EDIT: usize = 0;
const ID_STATIC: usize = 0;

// 颜色（COLORREF = 0x00BBGGRR）
const COLOR_BG: u32 = 0x0010_1010; // 深黑（与聊天输入条一致）
const COLOR_FG: u32 = 0x00FF_FFFF; // 白

// 字体：12pt 微软雅黑（高度 -16 像素）
const FONT_H: i32 = -16;
const FW_NORMAL: i32 = 400;
const DEFAULT_CHARSET: u32 = 1;
const CLEARTYPE_QUALITY: u32 = 5;
const DEFAULT_PITCH: u32 = 0;

// ---------------------------------------------------------------------------
// Win32 API 手写声明（零第三方依赖）
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
    fn IsDialogMessageW(hwnd: HWND, msg: *mut MSG) -> i32;
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

/// Ruby ↔ 面板线程共享状态（一把互斥锁保护，字段都极小，锁粒度可忽略）
#[derive(Default)]
struct AuthState {
    /// 面板线程存活且窗口已创建
    open: bool,
    /// 用户按 Esc / 点关闭（与 open=false 区分：poll 返回 2 而非 3）
    cancelled: bool,
    /// 有一次提交待 Ruby 读取（poll 返回 1 后自动清零）
    submit_pending: bool,
    /// 最近一次提交的用户名/密码快照
    username: String,
    password: String,
    /// 最近一次提交的模式：0=登录 1=注册
    mode: i32,
    /// Ruby → 面板状态栏文字（定时器刷新到界面）
    status: String,
}

struct AuthShared {
    state: Arc<Mutex<AuthState>>,
    stop: Arc<AtomicBool>,
    /// 面板线程 id（close 时 PostThreadMessageW 唤醒用）
    thread_id: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

static AUTHUI: Mutex<Option<AuthShared>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// 面板线程局部句柄（WndProc 与主循环都在同一线程上，thread_local 即可）
// ---------------------------------------------------------------------------

thread_local! {
    static DLG: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static EDIT_USER: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static EDIT_PASS: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static LBL_STATUS: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static BTN_HANDLES: std::cell::Cell<[HWND; 3]> = const { std::cell::Cell::new([0; 3]) };
    static GAME_HWND: std::cell::Cell<HWND> = const { std::cell::Cell::new(0) };
    static STATE: std::cell::RefCell<Option<Arc<Mutex<AuthState>>>> = const { std::cell::RefCell::new(None) };
    static USER_CANCEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LAST_STATUS: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
    /// 用户手动拖动过窗口（true 后不再自动跟随游戏窗口居中，位置自由）
    static USER_MOVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DARK_BRUSH: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn to_utf16z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 把焦点设到面板控件（跨线程：需 AttachThreadInput 到游戏主线程）
fn grab_focus(edit: HWND) {
    unsafe {
        let game = GAME_HWND.get();
        let main_tid = GetWindowThreadProcessId(game, std::ptr::null_mut());
        let cur_tid = GetCurrentThreadId();
        if main_tid != 0 && main_tid != cur_tid {
            AttachThreadInput(cur_tid, main_tid, 1);
            SetFocus(edit);
            AttachThreadInput(cur_tid, main_tid, 0);
        } else {
            SetFocus(edit);
        }
    }
}

/// 面板初始居中到游戏客户区（用户手动拖动过 → 位置自由，不再强制拉回中央）
fn sync_position() {
    // ★ 曾每 30ms 无条件拉回中央 → 用户拖走立刻弹回，等于拖不动
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

/// 焦点被游戏抢走时夺回（用户点游戏画面后仍回到面板）
fn regrab_focus_if_needed() {
    unsafe {
        let f = GetFocus();
        if f != 0 {
            return; // 焦点在本线程某个控件上（GetFocus 只看得见本线程队列）
        }
        grab_focus(EDIT_USER.get());
    }
}

/// 状态栏文字刷新（Ruby 线程写 state.status，这里每 30ms 搬到界面）
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

/// 提交登录/注册：快照两个输入框内容到共享状态，等 Ruby 轮询取走
fn do_submit(mode: i32) {
    let eu = EDIT_USER.get();
    let ep = EDIT_PASS.get();
    // IME 组合中不提交（IsDialogMessage 一般已规避，双保险）
    if is_composing(eu) || is_composing(ep) {
        return;
    }
    let user = read_edit_text(eu);
    let pass = read_edit_text(ep);
    let shared = STATE.with(|s| s.borrow().clone());
    if let Some(st) = shared {
        if let Ok(mut g) = st.lock() {
            if g.open {
                g.username = user;
                g.password = pass;
                g.mode = mode;
                g.submit_pending = true;
                g.status = "处理中…".to_string();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 窗口过程
// ---------------------------------------------------------------------------

unsafe extern "system" fn authui_wndproc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_COMMAND => {
            let id = wp & 0xFFFF;
            match id {
                BTN_LOGIN | IDOK_V => do_submit(0),            // 登录（Enter 触发默认按钮）
                BTN_REG => do_submit(1),                       // 注册
                BTN_CANCEL | IDCANCEL_V => unsafe {
                    PostMessageW(hwnd, WM_CLOSE, 0, 0);
                },
                _ => {}
            }
            0
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
            }
            0
        }
        // 用户拖动结束（标题栏拖动）→ 记住，此后不再自动拉回中央
        WM_EXITSIZEMOVE => {
            USER_MOVED.set(true);
            0
        }
        // 深色主题：面板背景 / 编辑框 / 静态文字统一深底白字
        WM_CTLCOLORDLG | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
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

fn authui_thread_main(state: Arc<Mutex<AuthState>>, stop: Arc<AtomicBool>, thread_id: Arc<AtomicU32>) {
    // IME 候选框是 OLE 窗口：本线程初始化 OLE
    unsafe { OleInitialize(std::ptr::null()) };

    // 找到游戏窗口（owner + 定位参照）
    let class = to_utf16z("RGSS Player");
    let game = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if game == 0 {
        if let Ok(mut g) = state.lock() {
            g.open = false;
        }
        unsafe { OleUninitialize() };
        return;
    }
    GAME_HWND.set(game);

    // 注册窗口类（重复启动时已注册，忽略失败）
    // ★ hInstance 必须与下方 CreateWindowExW 的 inst 参数一致：
    //   自注册类按 (类名, hInstance) 查找，创建时传 0 会找不到类，
    //   CreateWindowExW 静默失败（面板打不开的直接原因）
    let cls_name = to_utf16z("RGSS_P2PAuthUI");
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    unsafe {
        let wc = WNDCLASSW {
            style: 0,
            lpfn_wnd_proc: authui_wndproc,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: hinst,
            h_icon: 0,
            h_cursor: 0,
            h_br_background: 0,
            lpsz_menu_name: 0,
            lpsz_class_name: cls_name.as_ptr() as isize,
        };
        RegisterClassW(&wc);
        DARK_BRUSH.set(CreateSolidBrush(COLOR_BG));
        // 新线程 → USER_MOVED 天然重置，每次打开默认居中
    }

    // 客户区 400x180 → 外框尺寸（含标题栏边框）
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut rc = RECT { left: 0, top: 0, right: PANEL_W, bottom: PANEL_H };
    unsafe { AdjustWindowRect(&mut rc, style, 0) };
    let win_w = rc.right - rc.left;
    let win_h = rc.bottom - rc.top;

    let title = to_utf16z("P2P 账号");
    let dlg = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW,
            cls_name.as_ptr(), title.as_ptr(), style | WS_VISIBLE,
            0, 0, win_w, win_h,
            game, 0, hinst, std::ptr::null(),
        )
    };
    if dlg == 0 {
        if let Ok(mut g) = state.lock() {
            g.open = false;
        }
        unsafe { OleUninitialize() };
        return;
    }
    DLG.set(dlg);

    // 12pt 微软雅黑，统一设置到所有控件
    let face: Vec<u16> = "微软雅黑".encode_utf16().chain(std::iter::once(0)).collect();
    let font = unsafe {
        CreateFontW(FONT_H, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET,
                    0, 0, CLEARTYPE_QUALITY, DEFAULT_PITCH, face.as_ptr())
    };

    // 控件：用户名 / 密码 / 登录 / 注册 / 取消 / 状态栏
    let lb_user_t = to_utf16z("用户名:");
    let lb_pass_t = to_utf16z("密　码:");
    let btn_login_t = to_utf16z("登录");
    let btn_reg_t = to_utf16z("注册");
    let btn_cancel_t = to_utf16z("取消");
    let lbl_status_t = to_utf16z("");
    let edit_cls = to_utf16z("EDIT");
    let static_cls = to_utf16z("STATIC");
    let button_cls = to_utf16z("BUTTON");
    let make = |ex: u32, cls: *const u16, name: *const u16, st: u32,
                x: i32, y: i32, w: i32, h: i32, menu: usize| unsafe {
        let h = CreateWindowExW(ex, cls, name, WS_CHILD | WS_VISIBLE | st,
                                x, y, w, h, dlg, menu as isize, 0, std::ptr::null());
        if font != 0 {
            set_ctrl_font(h, font);
        }
        h
    };

    let eu = make(0, edit_cls.as_ptr(), std::ptr::null(),
                  WS_TABSTOP | ES_AUTOHSCROLL, 92, 22, 288, 26, ID_EDIT);
    let ep = make(0, edit_cls.as_ptr(), std::ptr::null(),
                  WS_TABSTOP | ES_AUTOHSCROLL | ES_PASSWORD, 92, 60, 288, 26, ID_EDIT);
    // ★ 按钮回退系统默认绘制（自绘版文字变方块/不稳定）；系统灰按钮清晰可点
    let bl = make(0, button_cls.as_ptr(), btn_login_t.as_ptr(),
                  WS_TABSTOP | BS_DEFPUSHBUTTON, 92, 102, 88, 30, BTN_LOGIN);
    let br = make(0, button_cls.as_ptr(), btn_reg_t.as_ptr(),
                  WS_TABSTOP, 190, 102, 88, 30, BTN_REG);
    let bc = make(0, button_cls.as_ptr(), btn_cancel_t.as_ptr(),
                  WS_TABSTOP, 288, 102, 88, 30, BTN_CANCEL);
    let ls = make(0, static_cls.as_ptr(), lbl_status_t.as_ptr(),
                  SS_CENTER, 20, 146, 360, 24, ID_STATIC);
    let _ = make(0, static_cls.as_ptr(), lb_user_t.as_ptr(), 0, 20, 26, 68, 22, ID_STATIC);
    let _ = make(0, static_cls.as_ptr(), lb_pass_t.as_ptr(), 0, 20, 64, 68, 22, ID_STATIC);

    EDIT_USER.set(eu);
    EDIT_PASS.set(ep);
    LBL_STATUS.set(ls);
    BTN_HANDLES.set([bl, br, bc]);

    // 整窗半透明
    unsafe { SetLayeredWindowAttributes(dlg, 0, PANEL_ALPHA, LWA_ALPHA) };

    // IME：给两个输入框单独关联输入上下文并强制打开（新线程不受主线程 IME 禁用影响）
    let himc = unsafe { ImmCreateContext() };
    if himc != 0 {
        unsafe {
            ImmAssociateContext(eu, himc);
            ImmAssociateContext(ep, himc);
            ImmSetOpenStatus(himc, 1);
        }
    }

    // 定时器：跟随游戏窗口 + 焦点守护 + 状态栏刷新
    unsafe { SetTimer(dlg, 1, SYNC_TIMER_MS, 0) };

    // 标记面板可用，并共享状态句柄给 WndProc（open 已在 open 时预置 true）
    if let Ok(mut g) = state.lock() {
        g.status = "输入账号密码".to_string();
    }
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // 初始焦点到用户名框
    sync_position();
    grab_focus(eu);

    // ---- 消息循环 ----
    // IsDialogMessageW：Tab 在输入框间切换，Enter → 默认按钮（登录），Esc → WM_CLOSE
    loop {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let r = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
        if r <= 0 {
            break; // WM_QUIT（窗口已销毁）或错误
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if msg.message == WM_APP_WAKE && msg.hwnd == 0 {
            continue; // 唤醒消息，仅用于跳出 GetMessage 阻塞
        }
        if msg.hwnd != 0 && unsafe { IsDialogMessageW(dlg, &mut msg) } != 0 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // ---- 清理（窗口属于本线程，必须在本线程销毁）----
    unsafe {
        KillTimer(dlg, 1);
        if himc != 0 {
            ImmAssociateContext(eu, 0);
            ImmAssociateContext(ep, 0);
            ImmDestroyContext(himc);
        }
        DestroyWindow(dlg);
        // GDI 对象随线程创建必须显式回收（反复开关面板不累积句柄）
        if font != 0 {
            DeleteObject(font);
        }
        let brush = DARK_BRUSH.get();
        if brush != 0 {
            DeleteObject(brush);
            DARK_BRUSH.set(0);
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

/// WM_SETFONT：统一控件字体
unsafe fn set_ctrl_font(hwnd: HWND, font: isize) {
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
    }
}

// ---------------------------------------------------------------------------
// 导出函数（stdcall）
// ---------------------------------------------------------------------------

/// 打开登录/注册面板（幂等：已打开直接返回 0）
#[no_mangle]
pub unsafe extern "system" fn net_authui_open() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = AUTHUI.lock() else {
            return -99;
        };
        // 已有活跃面板：幂等返回
        if let Some(shared) = guard.as_ref() {
            let active = shared.state.lock().map(|s| s.open).unwrap_or(false);
            if active {
                return 0;
            }
        }
        // 清理旧线程（已结束但未被 close 收尸的）
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
        // 启动面板线程。open 预先置 true：Ruby 下一帧就会来 poll，
        // 若等线程自己置位会有竞态（poll 提前拿到 3 误判面板已关闭）；
        // 线程创建失败（找不到游戏窗口等）会把 open 置回 false → poll 返回 3
        let state = Arc::new(Mutex::new(AuthState {
            open: true,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (s2, p2, t2) = (state.clone(), stop.clone(), thread_id.clone());
        let worker = std::thread::spawn(move || {
            unsafe { t2.store(GetCurrentThreadId(), Ordering::Relaxed) };
            authui_thread_main(s2, p2, t2);
        });
        *guard = Some(AuthShared {
            state,
            stop,
            thread_id,
            worker: Some(worker),
        });
        0
    }))
    .unwrap_or(-99)
}

/// 轮询面板状态：0=打开中 1=有提交待取 2=用户取消 3=未打开
#[no_mangle]
pub unsafe extern "system" fn net_authui_poll() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = AUTHUI.lock() else {
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

/// 取最近一次提交的字段（which 0=用户名 1=密码）。返回拷贝的 UTF-8 字节数。
#[no_mangle]
pub unsafe extern "system" fn net_authui_get_field(which: i32, buf: *mut u8, buf_len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() || buf_len <= 0 || (which != 0 && which != 1) {
            return -1;
        }
        let Ok(guard) = AUTHUI.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        let Ok(st) = shared.state.lock() else {
            return -99;
        };
        let text = if which == 0 { &st.username } else { &st.password };
        let bytes = text.as_bytes();
        let n = bytes.len().min(buf_len as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        }
        n as i32
    }))
    .unwrap_or(-99)
}

/// 最近一次提交的模式：0=登录 1=注册（未提交过返回 0）
#[no_mangle]
pub unsafe extern "system" fn net_authui_get_mode() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = AUTHUI.lock() else {
            return 0;
        };
        let Some(shared) = guard.as_ref() else {
            return 0;
        };
        shared.state.lock().map(|s| s.mode).unwrap_or(0)
    }))
    .unwrap_or(0)
}

/// 设置面板状态栏文字（UTF-8），Ruby 在收到服务器回包后调用
#[no_mangle]
pub unsafe extern "system" fn net_authui_set_status(text: *const u8, len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if text.is_null() || len < 0 {
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(text, len as usize) };
        let s = String::from_utf8_lossy(bytes).to_string();
        let Ok(guard) = AUTHUI.lock() else {
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

/// 关闭面板并回收线程（幂等）。内部复用入口，net_shutdown 也调用它。
pub(crate) fn internal_close() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = AUTHUI.lock() else {
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
pub unsafe extern "system" fn net_authui_close() -> i32 {
    internal_close()
}
