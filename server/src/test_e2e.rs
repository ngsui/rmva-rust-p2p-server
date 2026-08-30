// //! test_e2e.rs — 服务器权威拍卖行端到端测试
// //!
// //! 测试项（全部走真实 TCP + PostgreSQL）：
// //! [T01] 注册两个账号（A 买家 / B 卖家）
// //! [T02] 重复注册同名账号应被拒绝
// //! [T03] 错误密码登录应被拒绝
// //! [T04] 正确登录成功并返回服务器权威金币
// //! [T05] 未登录时调用拍卖接口应被拒绝
// //! [T06] B 上架物品（非法价格应被拒绝）
// //! [T07] 查询挂单列表（应含 B 的挂单，且标记 mine）
// //! [T08] A 购买 B 的挂单（金币转移 + 挂单置 sold + 流水落库）
// //! [T09] 重复购买同一挂单应被拒绝（防双买）
// //! [T10] A 购买自己的挂单应被拒绝
// //! [T11] 下架不存在的挂单应被拒绝
// //!
// //! 运行：先启动 p2p_server.exe，再 cargo run --bin test_e2e

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// 服务器地址
const ADDR: &str = "127.0.0.1:12345";

/// 测试客户端：长度前缀帧协议（与 rgss3_rust_net.dll 相同）
struct Client {
    stream: TcpStream,
    /// 接收缓冲（半包拼接）
    buf: Vec<u8>,
}

impl Client {
    fn connect() -> Result<Client, String> {
        let stream = TcpStream::connect(ADDR).map_err(|e| format!("连接失败: {}", e))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut c = Client { stream, buf: Vec::new() };
        // 先发 hello 帧（首字节 0x00 触发服务器走长度前缀协议，
        // 与真实 rgss3_rust_net.dll 客户端行为一致）
        c.send(&json!({"type": "hello"}))?;
        // 等服务器 init（忽略内容，只是确认连接正常）
        let _ = c.recv()?;
        Ok(c)
    }

    /// 发送一条 JSON 消息（4 字节大端长度 + UTF-8 JSON）
    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let payload = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        let frame = net_core::encode(&payload);
        self.stream.write_all(&frame).map_err(|e| e.to_string())
    }

    /// 接收一条 JSON 消息（阻塞直到收到完整帧）
    fn recv(&mut self) -> Result<Value, String> {
        loop {
            // 尝试从缓冲切出完整帧
            if self.buf.len() >= 4 {
                let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
                    as usize;
                if self.buf.len() >= 4 + len {
                    let payload: Vec<u8> = self.buf.drain(..4 + len).collect();
                    let text = String::from_utf8_lossy(&payload[4..]).to_string();
                    return serde_json::from_str(&text).map_err(|e| format!("JSON 错误: {}", e));
                }
            }
            // 缓冲不足，继续读网络
            let mut chunk = [0u8; 8192];
            let n = self
                .stream
                .read(&mut chunk)
                .map_err(|e| format!("读取失败: {}", e))?;
            if n == 0 {
                return Err("连接被服务器关闭".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// 发送后等待一条指定 type 的回包（跳过中途收到的其他消息）
    fn call(&mut self, msg: &Value, want_type: &str) -> Result<Value, String> {
        self.send(msg)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() > deadline {
                return Err(format!("等待 {} 超时", want_type));
            }
            let reply = self.recv()?;
            if reply.get("type").and_then(|v| v.as_str()) == Some(want_type) {
                return Ok(reply);
            }
            // 其他消息（如 init/广播）跳过继续等
        }
    }
}

fn main() {
    let mut pass = 0;
    let mut fail = 0;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
                println!("[OK] {}", $name);
            } else {
                fail += 1;
                println!("[FAIL] {}", $name);
            }
        };
    }

    println!("======= 拍卖行端到端测试（服务器: {}）=======", ADDR);

    // ---------- T01 注册两个账号 ----------
    let mut alice = Client::connect().expect("A 连接失败");
    let mut bob = Client::connect().expect("B 连接失败");

    // 用时间戳保证重复运行不撞名
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let name_a = format!("alice_{}", ts);
    let name_b = format!("bob_{}", ts);

    let r = alice
        .call(&json!({"type": "auth_register", "username": name_a, "password": "pass123"}), "auth_result")
        .unwrap();
    check!(
        "T01a A 注册成功",
        r["ok"] == json!(true) && r["gold"] == json!(500)
    );

    let r = bob
        .call(&json!({"type": "auth_register", "username": name_b, "password": "pass123"}), "auth_result")
        .unwrap();
    check!("T01b B 注册成功", r["ok"] == json!(true));

    // ---------- T02 重复注册被拒 ----------
    let r = alice
        .call(&json!({"type": "auth_register", "username": name_a, "password": "xxx"}), "auth_result")
        .unwrap();
    check!("T02 重复注册被拒", r["ok"] == json!(false));

    // ---------- T03 错误密码登录被拒 ----------
    let mut mallory = Client::connect().expect("M 连接失败");
    let r = mallory
        .call(&json!({"type": "auth_login", "username": name_a, "password": "wrong"}), "auth_result")
        .unwrap();
    check!("T03 错误密码登录被拒", r["ok"] == json!(false));

    // ---------- T04 正确登录成功 ----------
    let r = mallory
        .call(&json!({"type": "auth_login", "username": name_b, "password": "pass123"}), "auth_result")
        .unwrap();
    check!("T04 登录成功返回金币", r["ok"] == json!(true) && r["gold"] == json!(500));

    // ---------- T05 未登录调用拍卖被拒 ----------
    let mut guest = Client::connect().expect("G 连接失败");
    let r = guest
        .call(&json!({"type": "auction_list"}), "auction_list_result")
        .unwrap();
    check!("T05 未登录被拒", r["ok"] == json!(false));

    // ---------- T06 上架（含非法参数拒绝） ----------
    // 非法价格
    let r = bob
        .call(&json!({"type": "auction_sell", "item_id": 1, "quantity": 1, "price": -5}), "auction_sell_result")
        .unwrap();
    check!("T06a 非法价格被拒", r["ok"] == json!(false));

    // 合法上架（B 卖：item 7 药水 ×3 单价 100）
    let r = bob
        .call(&json!({"type": "auction_sell", "item_id": 7, "quantity": 3, "price": 100}), "auction_sell_result")
        .unwrap();
    check!("T06b B 上架成功", r["ok"] == json!(true));
    let listing_id = r["listing_id"].as_i64().unwrap_or(0);

    // ---------- T07 列表 ----------
    let r = alice
        .call(&json!({"type": "auction_list"}), "auction_list_result")
        .unwrap();
    let items = r["items"].as_array().cloned().unwrap_or_default();
    let found = items.iter().find(|it| it["id"].as_i64() == Some(listing_id));
    check!(
        "T07 列表含新挂单（mine 标记正确）",
        found.is_some() && found.unwrap()["mine"] == json!(false)
    );

    // ---------- T08 A 购买 ----------
    // A 初始 500，总价 300，买后应剩 200
    let r = alice
        .call(&json!({"type": "auction_buy", "listing_id": listing_id}), "auction_buy_result")
        .unwrap();
    check!(
        "T08a A 购买成功（金币扣减正确）",
        r["ok"] == json!(true) && r["gold"] == json!(200) && r["item_id"] == json!(7)
    );

    // B 应收到成交推送（金币 +300 = 800）
    let r = bob.recv().unwrap();
    check!(
        "T08b B 收到成交推送（金币入账）",
        r["type"] == json!("auction_sold") && r["gold_earned"] == json!(300) && r["gold"] == json!(800)
    );

    // ---------- T09 重复购买被拒（防双买） ----------
    let r = alice
        .call(&json!({"type": "auction_buy", "listing_id": listing_id}), "auction_buy_result")
        .unwrap();
    check!("T09 重复购买被拒", r["ok"] == json!(false));

    // ---------- T10 购买自己挂单被拒 ----------
    let r = bob
        .call(&json!({"type": "auction_sell", "item_id": 8, "quantity": 1, "price": 50}), "auction_sell_result")
        .unwrap();
    let own_listing = r["listing_id"].as_i64().unwrap_or(0);
    let r = bob
        .call(&json!({"type": "auction_buy", "listing_id": own_listing}), "auction_buy_result")
        .unwrap();
    check!("T10 购买自己挂单被拒", r["ok"] == json!(false));

    // ---------- T11 下架 ----------
    // 下架别人的单（A 尝试下架 B 的挂单）
    let r = alice
        .call(&json!({"type": "auction_cancel", "listing_id": own_listing}), "auction_cancel_result")
        .unwrap();
    check!("T11a 下架他人挂单被拒", r["ok"] == json!(false));

    // B 下架自己的
    let r = bob
        .call(&json!({"type": "auction_cancel", "listing_id": own_listing}), "auction_cancel_result")
        .unwrap();
    check!("T11b B 下架自己的挂单成功", r["ok"] == json!(true));

    // ---------- 余额终局核对 ----------
    let r = alice
        .call(&json!({"type": "auction_my"}), "auction_my_result")
        .unwrap();
    check!("T12a A 余额 = 200", r["gold"] == json!(200));

    let r = bob
        .call(&json!({"type": "auction_my"}), "auction_my_result")
        .unwrap();
    check!("T12b B 余额 = 800（500初始+300成交-0）", r["gold"] == json!(800));

    println!("==========================================");
    println!("结果: {} 通过 / {} 失败", pass, fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
