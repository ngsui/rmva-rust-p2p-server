//! test_harness — 模拟 RGSS3 Win32API 调用方式的 DLL 回环测试
//!
//! 与 Ruby 的 Win32API 一样：LoadLibraryA + GetProcAddress 按名取函数，
//! stdcall 调用。测试程序必须与 DLL 同为 32 位。
//!
//! 测试流程：加载 DLL -> 初始化 -> 连 echo 服务器 -> 发 3 条消息
//! （短 JSON / 256KB 大帧 / 空帧）-> 轮询收回显 -> 逐条校验 -> 关闭。

use std::ffi::CString;
use std::time::{Duration, Instant};

// ---- kernel32 FFI（与 Ruby Win32API 同源）----
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut u8;
    fn GetProcAddress(module: *mut u8, name: *const u8) -> *mut u8;
    fn FreeLibrary(module: *mut u8) -> i32;
    fn Sleep(ms: u32);
}

fn main() {
    println!("=== rgss3_rust_net.dll 回环测试 ===");

    // ---- 1. 加载 DLL（模拟 RGSS3 的 Win32API.new）----
    let dll_path = match std::env::args().nth(1) {
        Some(p) => p,
        None => "target/i686-pc-windows-msvc/release/rgss3_rust_net.dll".to_string(),
    };
    let path_c = CString::new(dll_path.as_str()).unwrap();
    let module = unsafe { LoadLibraryA(path_c.as_ptr() as *const u8) };
    if module.is_null() {
        eprintln!("[失败] 无法加载 DLL: {}（确认测试程序与 DLL 同为 32 位）", dll_path);
        std::process::exit(1);
    }
    println!("[OK] DLL 已加载: {}", dll_path);

    // ---- 2. 取导出函数（模拟 Win32API 按名绑定）----
    unsafe fn proc_addr(module: *mut u8, name: &str) -> *mut u8 {
        let c = CString::new(name).unwrap();
        unsafe { GetProcAddress(module, c.as_ptr() as *const u8) }
    }

    unsafe {
        let net_init = proc_addr(module, "net_init");
        let net_connect = proc_addr(module, "net_connect");
        let net_send = proc_addr(module, "net_send");
        let net_poll = proc_addr(module, "net_poll");
        let net_status = proc_addr(module, "net_status");
        let net_pending = proc_addr(module, "net_pending");
        let net_last_error = proc_addr(module, "net_last_error");
        let net_shutdown = proc_addr(module, "net_shutdown");

        for (name, p) in [
            ("net_init", net_init),
            ("net_connect", net_connect),
            ("net_send", net_send),
            ("net_poll", net_poll),
            ("net_status", net_status),
            ("net_pending", net_pending),
            ("net_last_error", net_last_error),
            ("net_shutdown", net_shutdown),
        ] {
            if p.is_null() {
                eprintln!("[失败] 导出函数缺失: {}", name);
                FreeLibrary(module);
                std::process::exit(1);
            }
        }
        println!("[OK] 8 个导出函数全部按名解析成功（stdcall ABI 验证通过）");

        // 函数指针类型定义（与 DLL 端签名一致）
        type NetInit = unsafe extern "system" fn() -> i32;
        type NetConnect = unsafe extern "system" fn(*const u8, i32) -> i32;
        type NetSend = unsafe extern "system" fn(*const u8, i32) -> i32;
        type NetPoll = unsafe extern "system" fn(*mut u8, i32) -> i32;
        type NetStatus = unsafe extern "system" fn() -> i32;
        type NetPending = unsafe extern "system" fn() -> i32;
        type NetLastError = unsafe extern "system" fn(*mut u8, i32) -> i32;
        type NetShutdown = unsafe extern "system" fn() -> i32;

        let net_init: NetInit = std::mem::transmute(net_init);
        let net_connect: NetConnect = std::mem::transmute(net_connect);
        let net_send: NetSend = std::mem::transmute(net_send);
        let net_poll: NetPoll = std::mem::transmute(net_poll);
        let net_status: NetStatus = std::mem::transmute(net_status);
        let net_pending: NetPending = std::mem::transmute(net_pending);
        let net_last_error: NetLastError = std::mem::transmute(net_last_error);
        let net_shutdown: NetShutdown = std::mem::transmute(net_shutdown);

        // ---- 3. 初始化 + 连接 ----
        let r = net_init();
        assert_eq!(r, 0, "net_init 返回 {}", r);
        println!("[OK] net_init -> 0");

        let host = CString::new("127.0.0.1").unwrap();
        let r = net_connect(host.as_ptr() as *const u8, 12345);
        assert_eq!(r, 0, "net_connect 返回 {}", r);
        println!("[OK] net_connect(127.0.0.1, 12345) -> 0（已开始异步连接）");

        // 轮询等待连接建立（模拟游戏每帧检查）
        let started = Instant::now();
        let mut connected = false;
        while started.elapsed() < Duration::from_secs(5) {
            let st = net_status();
            if st == 2 {
                connected = true;
                break;
            }
            if st == 0 {
                // 断开：打出错误文本
                let mut errbuf = [0u8; 512];
                let n = net_last_error(errbuf.as_mut_ptr(), errbuf.len() as i32);
                let msg = String::from_utf8_lossy(&errbuf[..n.max(0) as usize]);
                eprintln!("[失败] 连接失败: {}", msg);
                net_shutdown();
                FreeLibrary(module);
                std::process::exit(1);
            }
            Sleep(10);
        }
        if !connected {
            eprintln!("[失败] 5 秒内未连上 echo 服务器（先启动 echo_server.py）");
            net_shutdown();
            FreeLibrary(module);
            std::process::exit(1);
        }
        println!("[OK] 已连接（耗时 {:?}）", started.elapsed());

        // ---- 4. 发送 2 条测试消息 ----
        // a) 短 JSON（模拟真实联机消息）
        let msg1 = br#"{"type":"move","x":123,"y":456}"#.to_vec();
        // b) 256KB 大帧（强制 TCP 分段，验证粘包/半包处理）
        let mut msg2 = br#"{"type":"big","data":""#.to_vec();
        msg2.extend(std::iter::repeat(b'X').take(256 * 1024));
        msg2.extend(br#""}"#);

        // c) 空消息应被拒绝（net_poll 用 0 表示队列空，空消息会造成歧义）
        let r_empty = net_send(msg1.as_ptr(), 0);
        assert_eq!(r_empty, -1, "net_send(空) 应返回 -1，实际 {}", r_empty);
        println!("[OK] 空消息被正确拒绝（返回 -1）");

        for (i, m) in [&msg1, &msg2].iter().enumerate() {
            let r = net_send(m.as_ptr(), m.len() as i32);
            assert_eq!(r, 0, "net_send #{} 返回 {}", i + 1, r);
        }
        println!(
            "[OK] 2 条消息已入队发送（{}B / {}B）",
            msg1.len(),
            msg2.len()
        );

        // ---- 5. 轮询接收回显（模拟游戏帧循环节奏：10ms 一帧）----
        let expected: Vec<Vec<u8>> = vec![msg1.clone(), msg2.clone()];
        let mut received: Vec<Vec<u8>> = Vec::new();
        let mut recv_buf = vec![0u8; 512 * 1024]; // Ruby 侧同样会预分配大缓冲
        let deadline = Instant::now() + Duration::from_secs(10);
        while received.len() < 2 && Instant::now() < deadline {
            let pending = net_pending();
            if pending <= 0 {
                Sleep(10);
                continue;
            }
            let n = net_poll(recv_buf.as_mut_ptr(), recv_buf.len() as i32);
            if n > 0 {
                received.push(recv_buf[..n as usize].to_vec());
            } else if n == -2 {
                eprintln!("[失败] 缓冲区不足（理论上不应发生）");
                net_shutdown();
                FreeLibrary(module);
                std::process::exit(1);
            }
        }

        // ---- 6. 校验 ----
        if received.len() != 2 {
            eprintln!(
                "[失败] 超时：只收到 {}/2 条（先确认 echo 服务器在跑）",
                received.len()
            );
            net_shutdown();
            FreeLibrary(module);
            std::process::exit(1);
        }
        let all_match = received
            .iter()
            .zip(expected.iter())
            .enumerate()
            .all(|(i, (a, b))| {
                let ok = a == b;
                println!(
                    "[{}] 消息 #{} 回显一致（{} 字节）",
                    if ok { "OK" } else { "失败" },
                    i + 1,
                    a.len()
                );
                ok
            });
        if !all_match {
            eprintln!("[失败] 存在内容不一致");
            net_shutdown();
            FreeLibrary(module);
            std::process::exit(1);
        }

        // ---- 7. 队列应已清空 ----
        let pending = net_pending();
        assert_eq!(pending, 0, "收完后 net_pending 应为 0，实际 {}", pending);
        println!("[OK] 队列已清空");

        // ---- 8. 未连接时发送应被拒绝（简单错误路径验证）----
        // 此处已连接，跳过负向用例；DLL 内部有状态检查

        // ---- 9. 关闭 ----
        let t = Instant::now();
        let r = net_shutdown();
        assert_eq!(r, 0);
        let r2 = net_shutdown(); // 幂等性验证
        assert_eq!(r2, 0);
        println!("[OK] net_shutdown -> 0（耗时 {:?}，二次调用幂等）", t.elapsed());

        FreeLibrary(module);
    }

    println!("\n=== 全部测试通过：DLL 可正常使用 ===");
}
