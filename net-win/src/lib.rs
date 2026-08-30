//! rgss3_rust_net.dll — RGSS3 P2P 联机网络层（Windows 32 位）
//!
//! 由 RGSS3 (Ruby 1.9 / Win32API) 以 stdcall 方式调用。
//! 架构：1 个后台网络线程 + 互斥锁保护的双向队列 + 长度前缀分帧。
//! Ruby 主线程每帧调 net_poll / net_send，与网络线程通过队列交换数据，
//! 粘包/半包全部在 DLL 内消化，Ruby 侧拿到的永远是完整帧。
//!
//! 导出函数返回值约定：
//!   >= 0  成功（net_poll 返回消息字节数，0 = 无消息）
//!   -1    未连接 / 参数非法
//!   -2    缓冲区不足（net_poll：消息比缓冲区大，消息保留在队列中）
//!   -99   内部 panic（理论上不应出现，出现说明 DLL 有 bug）

pub mod auctionui;
mod authui;
mod input;

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use net_core::{Decoder, MAX_PAYLOAD};

/// 连接状态（net_status 返回值）
const ST_DISCONNECTED: i32 = 0;
const ST_CONNECTING: i32 = 1;
const ST_CONNECTED: i32 = 2;

/// 网络线程读缓冲
const READ_BUF_LEN: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 内部状态
// ---------------------------------------------------------------------------

struct Inner {
    status: i32,
    error: String,
    /// 收到的完整帧（payload，不含长度头）
    recv_queue: VecDeque<Vec<u8>>,
    /// 待发送的完整帧（已含长度头）
    send_queue: VecDeque<Vec<u8>>,
}

impl Inner {
    fn connecting() -> Self {
        Inner {
            status: ST_CONNECTING,
            error: String::new(),
            recv_queue: VecDeque::new(),
            send_queue: VecDeque::new(),
        }
    }
}

struct NetState {
    /// 主线程与网络线程共享的上下文（同一个 Arc，两边读写同一份数据）
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NetState {
    fn new() -> Self {
        NetState {
            inner: Arc::new(Mutex::new(Inner::connecting())),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

/// 全局单例。RGSS3 进程内只有一个网络上下文。
static STATE: Mutex<Option<NetState>> = Mutex::new(None);

const LOCK_POISONED: &str = "内部锁中毒（前一调用线程崩溃）";

// ---------------------------------------------------------------------------
// 网络线程
// ---------------------------------------------------------------------------

fn worker_main(host: String, port: u16, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    // 失败时统一走这里：置状态、记错误
    let fail = |msg: String| {
        if let Ok(mut g) = inner.lock() {
            g.status = ST_DISCONNECTED;
            g.error = msg;
        }
    };

    // 1. DNS 解析 + 连接（阻塞，但线程是后台的，不影响游戏帧率）
    let addr = match (host.as_str(), port).to_socket_addrs() {
        Ok(mut it) => it.next(),
        Err(e) => return fail(format!("域名解析失败: {}", e)),
    };
    let Some(addr) = addr else {
        return fail("域名解析失败: 无有效地址".to_string());
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(e) => return fail(format!("连接失败: {}", e)),
    };
    // 禁用 Nagle，小包立刻发出（联机位置同步必需）
    let _ = stream.set_nodelay(true);
    if stream.set_nonblocking(true).is_err() {
        return fail("无法切换非阻塞模式".to_string());
    }
    if let Ok(mut g) = inner.lock() {
        g.status = ST_CONNECTED;
        g.error.clear();
    }

    // 协议协商：连接成功立即发 hello 帧（长度前缀格式）。
    // 服务器以首字节识别协议（0x00=新协议，'{'=旧换行协议），
    // 旧客户端连接后不发数据、由服务器超时按旧协议处理。
    // hello 由服务器忽略，对 Ruby 层完全透明。
    {
        let hello = net_core::encode(br#"{"type":"hello"}"#);
        let mut sent = 0usize;
        while sent < hello.len() {
            match stream.write(&hello[sent..]) {
                Ok(0) => return fail("发送 hello 失败: 对端关闭".to_string()),
                Ok(n) => sent += n,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return fail(format!("发送 hello 失败: {}", e)),
            }
        }
    }

    let mut decoder = Decoder::new();
    let mut read_buf = [0u8; READ_BUF_LEN];
    // 上次没写完的残余帧（部分写回退用）
    let mut write_pending: Option<Vec<u8>> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // ---- 发送阶段 ----
        loop {
            let next = match write_pending.take() {
                Some(rest) => Some(rest),
                None => inner
                    .lock()
                    .ok()
                    .and_then(|mut g| g.send_queue.pop_front()),
            };
            let Some(mut frame) = next else { break };
            match stream.write(&frame) {
                Ok(n) if n < frame.len() => {
                    // 部分写：保留剩余，等下轮
                    frame.drain(..n);
                    write_pending = Some(frame);
                    break;
                }
                Ok(_) => {} // 整帧写出
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    write_pending = Some(frame);
                    break;
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return fail(format!("发送失败: {}", e)),
            }
        }

        // ---- 接收阶段 ----
        loop {
            match stream.read(&mut read_buf) {
                Ok(0) => return fail("连接被对端关闭".to_string()),
                Ok(n) => {
                    match decoder.feed(&read_buf[..n]) {
                        Ok(frames) => {
                            if !frames.is_empty() {
                                if let Ok(mut g) = inner.lock() {
                                    g.recv_queue.extend(frames);
                                }
                            }
                        }
                        Err(_) => {
                            return fail(format!("非法帧头（长度超过 {} 字节上限）", MAX_PAYLOAD));
                        }
                    }
                    if n < READ_BUF_LEN {
                        break; // 内核缓冲读空
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return fail(format!("接收失败: {}", e)),
            }
        }

        // 1ms 轮询：单连接场景下比 epoll/select 更简单且开销可忽略
        thread::sleep(Duration::from_millis(1));
    }
}

/// 停掉旧 worker 并复位状态（重连 / 关闭时用）
fn stop_worker(state: &mut NetState) {
    state.stop.store(true, Ordering::Relaxed);
    if let Some(h) = state.worker.take() {
        let _ = h.join();
    }
    state.stop = Arc::new(AtomicBool::new(false));
    state.inner = Arc::new(Mutex::new(Inner::connecting()));
}

// ---------------------------------------------------------------------------
// FFI 帮助函数
// ---------------------------------------------------------------------------

/// 读 C 字符串（Ruby String 自带 NUL 结尾）
unsafe fn read_cstr(ptr: *const u8) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let bytes = slice::from_raw_parts(ptr, len);
        String::from_utf8(bytes.to_vec()).ok()
    }
}

// ---------------------------------------------------------------------------
// 导出函数（stdcall，供 Win32API 调用）
// ---------------------------------------------------------------------------

/// 初始化网络系统（幂等）。返回 0 成功。
#[no_mangle]
pub unsafe extern "system" fn net_init() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        match STATE.lock() {
            Ok(mut g) => {
                if g.is_none() {
                    *g = Some(NetState::new());
                }
                0
            }
            Err(_) => -99,
        }
    }))
    .unwrap_or(-99)
}

/// 异步连接服务器。host 为 UTF-8 C 字符串，如 "127.0.0.1" 或域名。
/// 返回 0 表示已开始连接，用 net_status 轮询结果（重复调用 = 断开重连）。
#[no_mangle]
pub unsafe extern "system" fn net_connect(host: *const u8, port: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(host) = (unsafe { read_cstr(host) }) else {
            return -1;
        };
        let Ok(port) = u16::try_from(port) else {
            return -1;
        };
        let Ok(mut guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_mut() else {
            return -1; // 未 net_init
        };
        stop_worker(state); // 清理旧连接
        // 新连接上下文：主线程与网络线程共享同一个 Arc
        let inner = Arc::new(Mutex::new(Inner::connecting()));
        let h = thread::spawn({
            let inner = inner.clone();
            let stop = state.stop.clone();
            move || worker_main(host, port, inner, stop)
        });
        state.inner = inner;
        state.worker = Some(h);
        0
    }))
    .unwrap_or(-99)
}

/// 发送一条消息（异步排队，网络线程立即写出）。
/// data 指向任意字节（Ruby String），len 为长度。返回 0 成功。
/// 注意：len == 0 被拒绝——net_poll 用 0 表示"队列空"，
/// 空消息会造成歧义，且联机 JSON 消息永远非空。
#[no_mangle]
pub unsafe extern "system" fn net_send(data: *const u8, len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if data.is_null() || len <= 0 {
            return -1;
        }
        let payload = unsafe { slice::from_raw_parts(data, len as usize) }.to_vec();
        let Ok(guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_ref() else {
            return -1;
        };
        let Ok(mut g) = state.inner.lock() else {
            return -99;
        };
        if g.status != ST_CONNECTED {
            g.error = "发送失败: 尚未连接".to_string();
            return -1;
        }
        g.send_queue.push_back(net_core::encode(&payload));
        0
    }))
    .unwrap_or(-99)
}

/// 取一条完整消息（不含长度头）拷入 buf。
/// 返回字节数；0 = 队列空；-2 = 缓冲区不足（消息保留，可用更大缓冲重试）。
#[no_mangle]
pub unsafe extern "system" fn net_poll(buf: *mut u8, buf_len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() || buf_len <= 0 {
            return -1;
        }
        let Ok(guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_ref() else {
            return -1;
        };
        let Ok(mut g) = state.inner.lock() else {
            return -99;
        };
        let Some(front) = g.recv_queue.front() else {
            return 0;
        };
        if front.len() > buf_len as usize {
            return -2;
        }
        let n = front.len();
        unsafe {
            std::ptr::copy_nonoverlapping(front.as_ptr(), buf, n);
        }
        g.recv_queue.pop_front();
        n as i32
    }))
    .unwrap_or(-99)
}

/// 连接状态：0=断开 1=连接中 2=已连接
#[no_mangle]
pub unsafe extern "system" fn net_status() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_ref() else {
            return ST_DISCONNECTED;
        };
        state.inner.lock().map(|g| g.status).unwrap_or(-99)
    }))
    .unwrap_or(-99)
}

/// 队列中待收取的消息条数（诊断用）
#[no_mangle]
pub unsafe extern "system" fn net_pending() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        state
            .inner
            .lock()
            .map(|g| g.recv_queue.len() as i32)
            .unwrap_or(-99)
    }))
    .unwrap_or(-99)
}

/// 取最近一次错误文本（UTF-8），返回拷贝的字节数。
#[no_mangle]
pub unsafe extern "system" fn net_last_error(buf: *mut u8, buf_len: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if buf.is_null() || buf_len <= 0 {
            return -1;
        }
        let Ok(guard) = STATE.lock() else {
            return -99;
        };
        let Some(state) = guard.as_ref() else {
            return 0;
        };
        let msg = state
            .inner
            .lock()
            .map(|g| g.error.clone())
            .unwrap_or_else(|_| LOCK_POISONED.to_string());
        let bytes = msg.as_bytes();
        let n = bytes.len().min(buf_len as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        }
        n as i32
    }))
    .unwrap_or(-99)
}

/// 关闭连接并释放线程（幂等）。游戏退出时必须调用。
///   一并回收聊天输入条 / 登录面板 / 拍卖行面板线程，防止游戏退出时挂线程。
#[no_mangle]
pub unsafe extern "system" fn net_shutdown() -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let _ = auctionui::aui_internal_close();
        let _ = authui::internal_close();
        match STATE.lock() {
            Ok(mut g) => {
                if let Some(mut state) = g.take() {
                    stop_worker(&mut state);
                }
                0
            }
            Err(_) => -99,
        }
    }))
    .unwrap_or(-99)
}

// ---------------------------------------------------------------------------
// 服务器列表延迟探测（异步 TCP 握手计时，不占用主连接）
// ---------------------------------------------------------------------------
// 0 = 从未探测；-2 = 探测进行中；-3 = 失败/不可达；>0 = RTT 毫秒
static PROBE_MS: AtomicI32 = AtomicI32::new(0);

/// 发起一次延迟探测（TCP 握手往返 = RTT）。后台线程执行，立即返回。
/// 返回 0 已开始；-1 参数非法或已有探测在进行。
#[no_mangle]
pub unsafe extern "system" fn net_probe(host: *const u8, port: i32) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(host) = (unsafe { read_cstr(host) }) else {
            return -1;
        };
        let Ok(port) = u16::try_from(port) else {
            return -1;
        };
        // 上一次探测仍未结束则拒绝，防止堆线程
        if PROBE_MS.load(Ordering::Relaxed) == -2 {
            return -1;
        }
        PROBE_MS.store(-2, Ordering::Relaxed);
        thread::spawn(move || {
            PROBE_MS.store(probe_once(&host, port), Ordering::Relaxed);
        });
        0
    }))
    .unwrap_or(-99)
}

fn probe_once(host: &str, port: u16) -> i32 {
    let target = format!("{}:{}", host, port);
    let Ok(mut addrs) = target.to_socket_addrs() else {
        return -3;
    };
    let Some(addr) = addrs.next() else {
        return -3;
    };
    let t = Instant::now();
    match TcpStream::connect_timeout(&addr, Duration::from_millis(2000)) {
        Ok(_) => t.elapsed().as_millis().min(60000) as i32,
        Err(_) => -3,
    }
}

/// 取探测结果：>0 = RTT 毫秒；-2 = 进行中；-3 = 失败；0 = 从未探测。
/// 结果会一直保留到下次 net_probe 覆盖。
#[no_mangle]
pub unsafe extern "system" fn net_probe_result() -> i32 {
    PROBE_MS.load(Ordering::Relaxed)
}
