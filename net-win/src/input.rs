//! 游戏内聊天输入窗口（独立线程 + 系统输入法支持）
//!
//! RGSS Player 在主线程上调用了 IME 禁用（ImmDisableIME 或等效机制），
//! 导致主线程上创建的任何窗口（EDIT/RichEdit）都无法唤起输入法
//! （候选框弹不出、只英文字母直接上屏）——Ruby 侧的
//! ImmAssociateContext / OleInitialize / 强制焦点方案全部无效的原因。
//!
//! 解法：在 DLL 里开一条全新线程跑 RichEdit + 自有消息循环。
//! IME 禁用只作用于调用它的线程，新线程不受影响，
//! 键盘/IME 消息由该线程的消息循环原生派发，Ruby 主线程零参与。
//! （此为 RM 社区验证过的唯一可行路线，如 biud436 的 RS_Input DLL 方案）
//!
//! 导出函数（stdcall）：
//!   net_input_open()  -> 0 成功（幂等；窗口创建失败 -1）
//!   net_input_poll()  -> 0 输入中 1 已确认(文本就绪) 2 已取消 3 已结束
//!   net_input_get_text(buf, len) -> 拷贝 UTF-8 字节数（仅在状态 1 时有效）
//!   net_input_close() -> 0 成功（幂等；线程结束窗口自动销毁）

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 输入条宽高（像素）
/// ★ 高度 28：16pt 字在 24px 框里只能露出上半（RichEdit 行高含上下边距），
///   字号降到 12pt 后 28px 恰好完整容纳
const EDIT_W: i32 = 520;
const EDIT_H: i32 = 28;
/// 输入条距游戏客户区底部的距离
const EDIT_Y_FROM_BOTTOM: i32 = 64;
/// 整窗不透明度（LWA_ALPHA，0-255）
const WINDOW_ALPHA: u8 = 200;
/// 焦点/位置同步定时器间隔（毫秒）
const SYNC_TIMER_MS: u32 = 30;

// 窗口样式
const WS_POPUP: u32 = 0x8000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_BORDER: u32 = 0x0080_0000;
const ES_AUTOHSCROLL: u32 = 0x0000_0080;
/// 密码掩码样式（631 登录面板密码字段用）
const ES_PASSWORD: u32 = 0x0000_0020;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const LWA_ALPHA: u32 = 2;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;

// 消息
const WM_KEYDOWN: u32 = 0x0100;
const WM_TIMER: u32 = 0x0113;
const WM_GETTEXT: u32 = 13;
const WM_GETTEXTLENGTH: u32 = 14;
/// 自定义唤醒消息（net_input_close 跨线程叫醒消息循环用）
const WM_APP_WAKE: u32 = 0x8001;

// RichEdit 消息
const EM_SETBKGNDCOLOR: u32 = 1091; // WM_USER+67
const EM_SETCHARFORMAT: u32 = 1092; // WM_USER+68
const SCF_ALL: usize = 4;

// 按键
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;

// IME
const GCS_COMPSTR: u32 = 0x0008;

// CHARFORMATW 掩码
const CFM_COLOR: u32 = 0x4000_0000;
const CFM_SIZE: u32 = 0x8000_0000;
const CFM_FACE: u32 = 0x2000_0000;
/// 12pt（yHeight 单位 twips：1pt = 20；16pt 在 28px 框里会被裁切）
const FONT_TWIPS: i32 = 240;

// ---------------------------------------------------------------------------
// Win32 类型与结构
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

/// RichEdit CHARFORMATW 结构（C 布局 92 字节）
#[repr(C)]
struct CHARFORMATW {
    cb_size: u32,
    dw_mask: u32,
    dw_effects: u32,
    y_height: i32,
    y_offset: i32,
    cr_text_color: u32,
    b_char_set: u8,
    b_pitch_and_family: u8,
    sz_face_name: [u16; 32], // WCHAR[32]
}

impl CHARFORMATW {
    /// 白色 12pt 微软雅黑
    fn white_yahei() -> Self {
        let mut cf = CHARFORMATW {
            cb_size: std::mem::size_of::<CHARFORMATW>() as u32,
            dw_mask: CFM_COLOR | CFM_SIZE | CFM_FACE,
            dw_effects: 0,
            y_height: FONT_TWIPS,
            y_offset: 0,
            cr_text_color: 0x00FF_FFFF, // 白色（COLORREF BGR，白色三通道相同）
            b_char_set: 1,              // DEFAULT_CHARSET
            b_pitch_and_family: 1,      // DEFAULT_PITCH | FF_DONTCARE
            sz_face_name: [0u16; 32],
        };
        let face: Vec<u16> = "微软雅黑".encode_utf16().collect();
        let n = face.len().min(cf.sz_face_name.len() - 1);
        cf.sz_face_name[..n].copy_from_slice(&face[..n]);
        cf
    }
}

// ---------------------------------------------------------------------------
// Win32 API 手写声明（零第三方依赖，保持交叉编译简单）
// ---------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(cls: *const u16, title: *const u16) -> HWND;
    fn CreateWindowExW(
        ex_style: u32, cls: *const u16, name: *const u16, style: u32,
        x: i32, y: i32, w: i32, h: i32,
        parent: HWND, menu: HWND, inst: HWND, param: *const u8,
    ) -> HWND;
    fn DestroyWindow(hwnd: HWND) -> i32;
    fn GetMessageW(msg: *mut MSG, hwnd: HWND, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const MSG) -> i32;
    fn DispatchMessageW(msg: *const MSG) -> isize;
    fn PostThreadMessageW(tid: u32, msg: u32, wp: usize, lp: isize) -> i32;
    fn SetFocus(hwnd: HWND) -> HWND;
    fn GetFocus() -> HWND;
    fn AttachThreadInput(id: u32, to: u32, attach: i32) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    fn GetClientRect(hwnd: HWND, rc: *mut RECT) -> i32;
    fn ClientToScreen(hwnd: HWND, pt: *mut POINT) -> i32;
    fn GetWindowRect(hwnd: HWND, rc: *mut RECT) -> i32;
    fn SetWindowPos(
        hwnd: HWND, after: HWND, x: i32, y: i32, w: i32, h: i32, flags: u32,
    ) -> i32;
    fn SetTimer(hwnd: HWND, id: usize, ms: u32, cb: usize) -> usize;
    fn KillTimer(hwnd: HWND, id: usize) -> i32;
    fn SendMessageW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    fn SetLayeredWindowAttributes(hwnd: HWND, key: u32, alpha: u8, flags: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> HWND;
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
    fn ImmGetContext(hwnd: HWND) -> HIMC;
    fn ImmReleaseContext(hwnd: HWND, himc: HIMC) -> i32;
    fn ImmGetCompositionStringW(himc: HIMC, idx: u32, buf: *mut u8, len: u32) -> i32;
    fn ImmSetOpenStatus(himc: HIMC, open: i32) -> i32;
}

// ---------------------------------------------------------------------------
// 输入结果（跨线程共享）
// ---------------------------------------------------------------------------

enum InputResult {
    Active,
    Ok(String),
    Cancel,
    Closed,
}

struct InputShared {
    result: Arc<Mutex<InputResult>>,
    stop: Arc<AtomicBool>,
    /// 输入线程 id（close 时 PostThreadMessageW 唤醒用）
    thread_id: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

static INPUT: Mutex<Option<InputShared>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn to_utf16z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 检测 IME 是否处于拼音组合状态（组合串非空 = 正在打拼音，
/// 此时 Enter/Esc 属于输入法操作，不能当提交/取消）
pub(crate) fn is_composing(hwnd: HWND) -> bool {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc == 0 {
            return false;
        }
        let r = ImmGetCompositionStringW(himc, GCS_COMPSTR, std::ptr::null_mut(), 0);
        ImmReleaseContext(hwnd, himc);
        r > 0
    }
}

/// 把焦点设到输入条（跨线程：需 AttachThreadInput 到游戏主线程）
fn grab_focus(game: HWND, edit: HWND) {
    unsafe {
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

/// 同步输入条位置到游戏客户区下方居中（游戏窗口移动时跟随）
fn sync_position(game: HWND, edit: HWND) {
    unsafe {
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
        let x = pt.x + (cw - EDIT_W) / 2;
        let y = pt.y + ch - EDIT_H - EDIT_Y_FROM_BOTTOM;
        let mut cur = RECT::default();
        if GetWindowRect(edit, &mut cur) != 0
            && (cur.left != x || cur.top != y)
        {
            SetWindowPos(
                edit, 0, x, y, 0, 0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// 读取 RichEdit 全文（UTF-16 → UTF-8）
pub(crate) fn read_edit_text(edit: HWND) -> String {
    unsafe {
        let len = SendMessageW(edit, WM_GETTEXTLENGTH, 0, 0) as usize;
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        SendMessageW(edit, WM_GETTEXT, len + 1, buf.as_mut_ptr() as isize);
        String::from_utf16_lossy(&buf[..len])
    }
}

// ---------------------------------------------------------------------------
// 输入线程
// ---------------------------------------------------------------------------

fn input_thread_main(result: Arc<Mutex<InputResult>>, stop: Arc<AtomicBool>, thread_id: Arc<AtomicU32>, password: bool) {
    // IME 候选框是 OLE 窗口：本线程初始化 OLE
    unsafe { OleInitialize(std::ptr::null()) };

    // 找到游戏窗口（owner + 定位参照）
    let class = to_utf16z("RGSS Player");
    let game = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    if game == 0 {
        if let Ok(mut r) = result.lock() {
            *r = InputResult::Closed;
        }
        unsafe { OleUninitialize() };
        return;
    }

    // 加载 RichEdit：msftedit.dll (5.0) 优先，回退 riched20.dll (2.0)
    // ★ LoadLibrary 句柄保持到线程结束（不能 FreeLibrary，类会注销）
    let lib_name = to_utf16z("msftedit.dll");
    let class_name = to_utf16z("RICHEDIT50W");
    let loaded = unsafe { LoadLibraryW(lib_name.as_ptr()) };
    let (edit_class, lib_handle) = if loaded != 0 {
        (class_name, loaded)
    } else {
        let lib_name2 = to_utf16z("riched20.dll");
        let class_name2 = to_utf16z("RICHEDIT20W");
        let h = unsafe { LoadLibraryW(lib_name2.as_ptr()) };
        if h == 0 {
            if let Ok(mut r) = result.lock() {
                *r = InputResult::Closed;
            }
            unsafe { OleUninitialize() };
            return;
        }
        (class_name2, h)
    };
    let _keep_lib = lib_handle; // 线程结束前不释放

    // 创建输入条（owner = 游戏窗口；屏幕坐标先占位，立即 sync 修正）
    //   password=true 时加 ES_PASSWORD 掩码（登录面板密码字段）
    let style = WS_POPUP | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL
        | if password { ES_PASSWORD } else { 0 };
    let ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW;
    let edit = unsafe {
        CreateWindowExW(
            ex_style, edit_class.as_ptr(), std::ptr::null(),
            style, 0, 0, EDIT_W, EDIT_H,
            game, 0, 0, std::ptr::null(),
        )
    };
    if edit == 0 {
        if let Ok(mut r) = result.lock() {
            *r = InputResult::Closed;
        }
        unsafe { OleUninitialize() };
        return;
    }

    // 整窗半透明（LWA_ALPHA）
    unsafe { SetLayeredWindowAttributes(edit, 0, WINDOW_ALPHA, LWA_ALPHA) };
    // 深黑背景（EM_SETBKGNDCOLOR：wParam=0 表示用 lParam 指定色；
    //   传 1 会回退系统默认白色——之前白底白字的根因）
    unsafe { SendMessageW(edit, EM_SETBKGNDCOLOR, 0, 0x0010_1010) };
    // 白色 12pt 微软雅黑（EM_SETCHARFORMAT + CHARFORMATW）
    let cf = CHARFORMATW::white_yahei();
    unsafe {
        SendMessageW(edit, EM_SETCHARFORMAT, SCF_ALL, &cf as *const CHARFORMATW as isize)
    };

    // IME：给输入条单独关联一个输入上下文并强制打开。
    //   （新线程上 ImmDisableIME 不生效，正常 ImmGetContext 即可工作，
    //     显式关联 + 打开是为了万无一失）
    let himc = unsafe { ImmCreateContext() };
    let old_himc = if himc != 0 {
        let old = unsafe { ImmAssociateContext(edit, himc) };
        unsafe { ImmSetOpenStatus(himc, 1) };
        old
    } else {
        0
    };

    // 位置同步 + 焦点抢占
    sync_position(game, edit);
    grab_focus(game, edit);

    // 定时器：跟随游戏窗口移动 + 焦点被抢时夺回（IME 只为焦点窗口服务）
    unsafe { SetTimer(edit, 1, SYNC_TIMER_MS, 0) };

    // ---- 消息循环 ----
    let mut submitted = false;
    loop {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let r = unsafe { GetMessageW(&mut msg, 0, 0, 0) };
        if r <= 0 {
            break; // WM_QUIT 或错误
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if msg.message == WM_APP_WAKE {
            continue; // 唤醒消息（close 发的），仅用于跳出 GetMessage 阻塞
        }
        if msg.hwnd == edit && msg.message == WM_KEYDOWN {
            let vk = msg.w_param as u32;
            if (vk == VK_RETURN || vk == VK_ESCAPE) && !is_composing(edit) {
                submitted = vk == VK_RETURN;
                break;
            }
        }
        if msg.hwnd == edit && msg.message == WM_TIMER {
            sync_position(game, edit);
            if unsafe { GetFocus() } != edit {
                grab_focus(game, edit);
            }
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // ---- 清理（窗口属于本线程，必须在本线程销毁）----
    let text = if submitted { read_edit_text(edit) } else { String::new() };
    unsafe {
        KillTimer(edit, 1);
        if himc != 0 {
            ImmAssociateContext(edit, old_himc);
            ImmDestroyContext(himc);
        }
        DestroyWindow(edit);
    }
    if let Ok(mut r) = result.lock() {
        *r = if submitted {
            InputResult::Ok(text)
        } else {
            InputResult::Cancel
        };
    }
    unsafe { OleUninitialize() };
    thread_id.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// 导出函数（stdcall）
// ---------------------------------------------------------------------------

/// 打开输入条（幂等：已在输入中直接返回 0）
///   is_password 非 0 时密码掩码显示（631 登录面板密码字段）
#[no_mangle]
pub unsafe extern "system" fn net_input_open(is_password: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = INPUT.lock() else {
            return -99;
        };
        // 已有活跃输入：幂等返回
        if let Some(shared) = guard.as_ref() {
            let active = shared
                .result
                .lock()
                .map(|r| matches!(*r, InputResult::Active))
                .unwrap_or(false);
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
        // 启动新输入线程
        let result = Arc::new(Mutex::new(InputResult::Active));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (r2, s2, t2) = (result.clone(), stop.clone(), thread_id.clone());
        let password = is_password != 0;
        let worker = std::thread::spawn(move || {
            // 线程自身 id 用 Win32 API 取（close 时 PostThreadMessageW 唤醒用）
            unsafe { t2.store(GetCurrentThreadId(), Ordering::Relaxed) };
            input_thread_main(r2, s2, t2, password);
        });
        *guard = Some(InputShared {
            result,
            stop,
            thread_id,
            worker: Some(worker),
        });
        0
    }))
    .unwrap_or(-99)
}

/// 轮询输入状态：0=输入中 1=已确认(文本就绪) 2=已取消 3=已结束/未打开
#[no_mangle]
pub unsafe extern "system" fn net_input_poll() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = INPUT.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return 3;
        };
        let Ok(r) = shared.result.lock() else {
            return -99;
        };
        match &*r {
            InputResult::Active => 0,
            InputResult::Ok(_) => 1,
            InputResult::Cancel => 2,
            InputResult::Closed => 3,
        }
    }))
    .unwrap_or(-99)
}

/// 取确认的文本（仅在 poll 返回 1 时有效）。返回拷贝的 UTF-8 字节数。
#[no_mangle]
pub unsafe extern "system" fn net_input_get_text(buf: *mut u8, buf_len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() || buf_len <= 0 {
            return -1;
        }
        let Ok(guard) = INPUT.lock() else {
            return -99;
        };
        let Some(shared) = guard.as_ref() else {
            return -1;
        };
        let Ok(r) = shared.result.lock() else {
            return -99;
        };
        let InputResult::Ok(text) = &*r else {
            return -1; // 非确认状态无文本
        };
        let bytes = text.as_bytes();
        let n = bytes.len().min(buf_len as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        }
        n as i32
    }))
    .unwrap_or(-99)
}

/// 关闭输入条并回收线程（幂等）
#[no_mangle]
pub unsafe extern "system" fn net_input_close() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(mut guard) = INPUT.lock() else {
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
