//! p2p_server — Rust P2P 联机服务器 v2（tokio + PostgreSQL）
//!
//! 替代 Python p2p_server.py。功能：玩家 ID 分配、房间管理、
//! Lockstep 战斗消息中转（房间内保序广播）、聊天、ping/pong、UDP 局域网发现。
//!
//! v2 新增（权威服务器演进）：
//!   - PostgreSQL 存储层：账号体系（argon2 密码哈希）、拍卖行、交易流水
//!   - 服务器权威金币：所有加减钱在数据库事务内完成，杜绝刷钱
//!   - 每连接限速器：30 msg/s，连续超限踢连接（防洪水/防刷屏）
//!   - 数据库不可用时降级运行：转发功能不受影响
//!
//! 双协议自动检测（同一端口并存，支持平滑迁移）：
//!   首字节 0x00 -> 长度前缀帧（4 字节大端 u32 + JSON，rgss3_rust_net.dll 客户端）
//!   首字节 '{'  -> 换行分隔 JSON（旧 Python 代理客户端）
//!
//! 架构：每客户端 = 1 读循环 + 1 写 task；写 task 消费 mpsc 队列，
//! 单点顺序写 socket —— Lockstep 消息顺序由架构保证。

mod auction;
mod auth;
mod db;
mod rate_limit;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use net_core::MAX_PAYLOAD;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::db::Db;
use crate::rate_limit::RateLimiter;

// ==================== 配置 ====================

const TCP_PORT: u16 = 12345; // 认证/房间/中转
const UDP_DISCOVERY_PORT: u16 = 12346; // 局域网发现广播
const MAX_PLAYERS_PER_ROOM: usize = 4; // 房间人数上限（显示用，v1 不强制）
const SERVER_NAME: &str = "P2P联机服务器";

/// 单次读取的缓冲上限（换行 JSON 协议的行长保护）
const MAX_LINE: usize = MAX_PAYLOAD;

// ==================== 状态 ====================

/// 单个在线玩家（共享状态中的登记项）
struct Player {
    /// 发送队列：写 task 消费（每条为 JSON 字符串，不含帧头）
    tx: mpsc::UnboundedSender<String>,
    name: String,
    room: Option<String>,
    /// 玩家完整信息（join_room 时上报：name/map/x/y/character_name 等），
    /// room_joined / player_joined 时原样回传给其他客户端
    info: Value,
    /// 登录后绑定的数据库账号 ID（None = 未登录，拍卖行等功能不可用）
    account_id: Option<i32>,
    /// 拍卖行面板是否打开中（打开期间市场变动会推送该玩家，高效自动刷新）
    auction_open: bool,
}

#[derive(Default)]
struct State {
    players: HashMap<i64, Player>,
    next_id: i64,
    /// room_id -> 玩家 id 列表（顺序即加入顺序）
    rooms: HashMap<String, Vec<i64>>,
}

type Shared = Arc<Mutex<State>>;

fn log(msg: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("[服务器 @{}] {}", now, msg);
}

// ==================== 发送帮助 ====================

/// 给指定玩家发一条 JSON（入队，写 task 异步写出）
fn send_to(state: &Shared, player_id: i64, msg: &Value) {
    let text = serde_json::to_string(msg).unwrap_or_default();
    if let Ok(g) = state.lock() {
        if let Some(p) = g.players.get(&player_id) {
            // channel 关闭说明连接已断，忽略即可
            let _ = p.tx.send(text);
        }
    }
}

/// 房间内广播（可选排除发起者）。返回实际送达人数。
fn broadcast_room(state: &Shared, room_id: &str, msg: &Value, exclude: Option<i64>) -> usize {
    let text = serde_json::to_string(msg).unwrap_or_default();
    let mut targets = Vec::new();
    if let Ok(g) = state.lock() {
        if let Some(members) = g.rooms.get(room_id) {
            for pid in members {
                if Some(*pid) == exclude {
                    continue;
                }
                if let Some(p) = g.players.get(pid) {
                    targets.push(p.tx.clone());
                }
            }
        }
    }
    let mut sent = 0;
    for tx in targets {
        if tx.send(text.clone()).is_ok() {
            sent += 1;
        }
    }
    sent
}

/// 查询玩家当前所在房间（未加入房间返回 None）
fn current_room(state: &Shared, player_id: i64) -> Option<String> {
    if let Ok(g) = state.lock() {
        if let Some(p) = g.players.get(&player_id) {
            return p.room.clone();
        }
    }
    None
}

/// 市场变动推送：发给所有打开了拍卖行面板的玩家（排除发起者）。
/// 只发一条小通知（不含列表数据），客户端收到后自行拉取最新列表——
/// 相当于数据效率极高的自动刷新：无变动时零流量，有变动时一条消息。
fn broadcast_auction_change(state: &Shared, event: &str, exclude: i64) {
    let msg = json!({ "type": "auction_changed", "event": event });
    let text = serde_json::to_string(&msg).unwrap_or_default();
    if let Ok(g) = state.lock() {
        for (pid, p) in g.players.iter() {
            if *pid == exclude || !p.auction_open {
                continue;
            }
            // channel 关闭说明连接已断，忽略即可
            let _ = p.tx.send(text.clone());
        }
    }
}

// ==================== 消息处理 ====================

/// 处理一条客户端消息（已解析的 JSON）
///
/// 锁纪律：std::Mutex 的临界区内严禁 .await（guard 跨 await 会阻塞
/// executor 线程）。所有数据库调用前先把需要的数据拷出锁，锁外 await，
/// 完成后再短暂拿锁写回结果。
async fn handle_message(state: &Shared, db: Option<&Db>, player_id: i64, msg: &Value) {
    let msg_type = match msg.get("type").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return,
    };

    match msg_type.as_str() {
        // ---------- 账号注册（username + password -> argon2 哈希入库）----------
        "auth_register" | "auth_login" => {
            let username = msg.get("username").and_then(|v| v.as_str()).unwrap_or("");
            let password = msg.get("password").and_then(|v| v.as_str()).unwrap_or("");

            let db = match db {
                Some(d) => d,
                None => {
                    send_to(
                        state,
                        player_id,
                        &auth::auth_err_msg(&msg_type, "数据库不可用（服务器降级模式）"),
                    );
                    return;
                }
            };

            let result = if msg_type == "auth_register" {
                auth::register(db, username, password).await
            } else {
                auth::login(db, username, password).await
            };

            match result {
                Ok(acc) => {
                    // 绑定账号到当前连接（锁内短临界区写回）
                    {
                        let mut g = state.lock().unwrap();
                        if let Some(p) = g.players.get_mut(&player_id) {
                            p.account_id = Some(acc.account_id);
                        }
                    }
                    send_to(state, player_id, &auth::auth_ok_msg(&msg_type, &acc));
                    log(&format!(
                        "玩家{}{}成功：账号{}(id={})",
                        player_id,
                        if msg_type == "auth_register" { "注册" } else { "登录" },
                        acc.username,
                        acc.account_id
                    ));
                }
                Err(e) => {
                    send_to(state, player_id, &auth::auth_err_msg(&msg_type, &e));
                }
            }
        }

        // ---------- 拍卖行（需登录，服务器权威金币）----------
        "auction_list" | "auction_my" | "auction_sell" | "auction_buy" | "auction_cancel" => {
            // 前置：数据库可用 + 已登录
            let db = match db {
                Some(d) => d,
                None => {
                    send_to(
                        state,
                        player_id,
                        &json!({"type": format!("{}_result", msg_type), "ok": false,
                                "error": "数据库不可用（服务器降级模式）"}),
                    );
                    return;
                }
            };
            // 锁内读出账号绑定后立即放锁，数据库操作全程锁外
            let account_id = {
                let g = state.lock().unwrap();
                g.players.get(&player_id).and_then(|p| p.account_id)
            };
            let account_id = match account_id {
                Some(a) => a,
                None => {
                    send_to(
                        state,
                        player_id,
                        &json!({"type": format!("{}_result", msg_type), "ok": false,
                                "error": "请先登录账号"}),
                    );
                    return;
                }
            };

            let result = match msg_type.as_str() {
                "auction_list" => auction::list(db, account_id).await.map(|v| (v, None)),
                "auction_my" => auction::my(db, account_id).await.map(|v| (v, None)),
                "auction_sell" => {
                    let item_id = msg.get("item_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let quantity = msg.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0);
                    let price = msg.get("price").and_then(|v| v.as_i64()).unwrap_or(0);
                    auction::sell(db, account_id, item_id, quantity, price)
                        .await
                        .map(|v| (v, None))
                }
                "auction_buy" => {
                    let listing_id = msg.get("listing_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    auction::buy(db, account_id, listing_id).await.map(|outcome| {
                        // 解构出买家回包与卖家通知（通知由本层负责投递）
                        let auction::BuyOutcome {
                            buyer_msg,
                            seller_notify,
                        } = outcome;
                        (buyer_msg, seller_notify)
                    })
                }
                "auction_cancel" => {
                    let listing_id = msg.get("listing_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    auction::cancel(db, account_id, listing_id)
                        .await
                        .map(|v| (v, None))
                }
                _ => unreachable!(),
            };

            match result {
                Ok((reply, seller_notify)) => {
                    send_to(state, player_id, &reply);
                    // 成交推送：给在线卖家补发通知（附最新余额）
                    if let Some((seller_account, mut notify)) = seller_notify {
                        let seller_gold = auction::get_gold(db, seller_account).await;
                        notify["gold"] = json!(seller_gold);
                        // 锁内找出卖家所在的连接（account_id -> player_id）
                        let seller_pid = {
                            let g = state.lock().unwrap();
                            g.players
                                .iter()
                                .find(|(_, p)| p.account_id == Some(seller_account))
                                .map(|(pid, _)| *pid)
                        };
                        if let Some(pid) = seller_pid {
                            send_to(state, pid, &notify);
                        }
                    }
                    // 市场变动推送：上架/购买/下架成功 → 所有打开面板的玩家自动刷新
                    // （发起者自己会收到上方 reply 并刷新，故排除）
                    let event = match msg_type.as_str() {
                        "auction_sell" => Some("sell"),
                        "auction_buy" => Some("buy"),
                        "auction_cancel" => Some("cancel"),
                        _ => None,
                    };
                    if let Some(ev) = event {
                        broadcast_auction_change(state, ev, player_id);
                    }
                    log(&format!("玩家{}拍卖操作: {}", player_id, msg_type));
                }
                Err(e) => {
                    send_to(
                        state,
                        player_id,
                        &json!({"type": format!("{}_result", msg_type), "ok": false, "error": e}),
                    );
                }
            }
        }

        // ---------- 拍卖行面板订阅（打开期间接收市场变动推送）----------
        "auction_subscribe" | "auction_unsubscribe" => {
            let open = msg_type == "auction_subscribe";
            let mut g = state.lock().unwrap();
            if let Some(p) = g.players.get_mut(&player_id) {
                p.auction_open = open;
            }
        }

        // ---------- 加入房间 ----------
        "join_room" => {
            let room_id = msg
                .get("room_id")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            // 整理玩家信息。兼容两种格式：
            //   1. 嵌套式：{type, room_id, player_info:{...}}（Python 代理客户端）
            //   2. 扁平式：{type, room_id, name, map, x, y, character_name, ...}（550 脚本直发）
            let mut info = if let Some(pi) = msg.get("player_info") {
                pi.clone()
            } else {
                // 扁平式：把除 type/room_id 外的顶层字段全部当作玩家信息
                let mut obj = serde_json::Map::new();
                if let Some(m) = msg.as_object() {
                    for (k, v) in m {
                        if k != "type" && k != "room_id" {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                }
                Value::Object(obj)
            };
            if !info.is_object() {
                info = json!({});
            }
            let obj = info.as_object_mut().unwrap();
            obj.insert("id".into(), json!(player_id));
            obj.insert("p2p_port".into(), json!(0i64)); // 旧架构字段占位
            let name = obj
                .get("name")
                .or_else(|| obj.get("playername"))
                .and_then(|v| v.as_str())
                .unwrap_or("玩家")
                .to_string();

            // 登记进房间，收集房间现有成员（仅在线者）
            let existing: Vec<Value> = {
                let mut g = state.lock().unwrap();
                // 1. 更新玩家登记（对 g.players 的可变借用，语句即结束）
                if let Some(p) = g.players.get_mut(&player_id) {
                    p.room = Some(room_id.clone());
                    p.name = name.clone();
                    p.info = info.clone();
                }
                // 2. 房间成员登记（对 g.rooms 的可变借用）
                let members = g.rooms.entry(room_id.clone()).or_default();
                if !members.contains(&player_id) {
                    members.push(player_id);
                }
                // 3. 克隆成员列表，结束可变借用后再做不可变读取
                let member_ids: Vec<i64> = members.clone();
                member_ids
                    .iter()
                    .filter(|pid| **pid != player_id)
                    .filter_map(|pid| g.players.get(pid).map(|p| p.info.clone()))
                    .collect()
            };

            // 告知加入者房间现状
            send_to(
                state,
                player_id,
                &json!({
                    "type": "room_joined",
                    "room_id": room_id,
                    "players": existing,
                }),
            );
            // 通知房间其他人
            broadcast_room(
                state,
                &room_id,
                &json!({
                    "type": "player_joined",
                    "player_id": player_id,
                    "player_info": info,
                }),
                Some(player_id),
            );
            log(&format!("玩家{}({})加入房间{}", player_id, name, room_id));
        }

        // ---------- Lockstep 战斗消息中转（房间内保序广播）----------
        // 同房间所有接收者看到相同顺序：广播入队顺序一致 + 写 task 单点顺序写。
        // 原架构 Python 代理转发时补 room_id；RustNet 直连的消息不带，
        // 统一回退按玩家登记的房间路由。
        "battle_action" | "battle_setup" | "actor_snapshot" | "snapshot_ack"
        | "action_input" | "turn_hash" | "desync_recover" | "battle_end"
        | "sync_globals" | "sync_rng" | "battle_reject" | "qte_input"
        | "state" | "player_state" | "chat" => {
            let room = msg
                .get("room_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| current_room(state, player_id));
            if let Some(room_id) = room {
                broadcast_room(state, &room_id, msg, Some(player_id));
            }
        }

        // ---------- ping/pong（RTT 测量）----------
        "ping" => {
            let ts = msg.get("ts").cloned().unwrap_or(Value::Null);
            send_to(
                state,
                player_id,
                &json!({
                    "type": "pong",
                    "ts": ts,
                    "from_ping": true,
                }),
            );
        }

        // ---------- 协议协商 hello（新 DLL 客户端连接后自动发送）----------
        "hello" => {}

        // ---------- open_chat_window（原 Python 代理的本地指令，直连后静默忽略）----------
        "open_chat_window" => {}

        // ---------- 心跳 ----------
        "heartbeat" => {
            // v1：TCP 存活即在线；超时踢出属于阶段4（安全加固）
        }

        _ => {
            log(&format!("未知消息类型: {}（玩家{}）", msg_type, player_id));
        }
    }
}

// ==================== 连接生命周期 ====================

/// 客户端断开：退房广播 + 移除登记
fn cleanup(state: &Shared, player_id: i64) {
    let mut left_room: Option<String> = None;
    {
        let mut g = state.lock().unwrap();
        if let Some(p) = g.players.get(&player_id) {
            left_room = p.room.clone();
        }
        if let Some(ref room_id) = left_room {
            if let Some(members) = g.rooms.get_mut(room_id) {
                members.retain(|pid| *pid != player_id);
                if members.is_empty() {
                    g.rooms.remove(room_id);
                }
            }
        }
        g.players.remove(&player_id);
    }
    // 广播退房（锁外执行，避免与 broadcast_room 死锁）
    if let Some(room_id) = left_room {
        broadcast_room(
            state,
            &room_id,
            &json!({"type": "player_left", "player_id": player_id}),
            None,
        );
    }
    log(&format!("玩家{}断开", player_id));
}

/// 每客户端主逻辑：协议检测 -> 分配 ID -> 读写双 task -> 读循环分发
async fn handle_client(state: Shared, db: Option<Db>, stream: TcpStream) {
    let stream = stream;

    // 每连接独立限速器（属于读循环，单线程访问，无需加锁）
    let mut limiter = RateLimiter::new();

    // ---- 协议检测：窥视首字节（最多等 500ms）----
    // 0x00 -> 长度前缀帧（新 DLL 客户端连接后会立即发 hello 帧）
    // '{'  -> 换行 JSON（旧客户端发来的任意首条消息）
    // 超时 -> 旧客户端（连接后不说话、等服务器先发 init，行为同 Python 服务器）
    let mut peek = [0u8; 1];
    let peeked = tokio::time::timeout(Duration::from_millis(500), stream.peek(&mut peek)).await;
    let proto = match peeked {
        Ok(Ok(_)) if peek[0] == 0x00 => Proto::LengthPrefix,
        Ok(Ok(_)) if peek[0] == b'{' => Proto::NewlineJson,
        Ok(Ok(_)) => {
            log(&format!(
                "协议检测失败（首字节 0x{:02X} 非法），断开",
                peek[0]
            ));
            return;
        }
        Ok(Err(e)) => {
            log(&format!("协议检测读取错误: {}，断开", e));
            return;
        }
        Err(_) => Proto::NewlineJson, // 500ms 无数据：按旧客户端处理
    };

    // ---- 分配玩家 ID 并登记 ----
    let (player_id, tx, mut rx) = {
        let mut g = state.lock().unwrap();
        g.next_id += 1;
        let id = g.next_id;
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        g.players.insert(
            id,
            Player {
                tx: tx.clone(),
                name: format!("玩家{}", id),
                room: None,
                info: json!({"id": id, "p2p_port": 0i64}),
                account_id: None,
                auction_open: false,
            },
        );
        (id, tx, rx)
    };
    log(&format!(
        "玩家{}连接（协议: {}）",
        player_id,
        match proto {
            Proto::LengthPrefix => "长度前缀帧",
            Proto::NewlineJson => "换行JSON",
        }
    ));

    // ---- 读写分离（owned 半边，满足写 task 的 'static 要求）----
    let (read_half, mut write_half) = stream.into_split();

    let write_task = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let payload = text.as_bytes();
            let res = match proto {
                Proto::LengthPrefix => {
                    let frame = net_core::encode(payload);
                    write_half.write_all(&frame).await
                }
                Proto::NewlineJson => {
                    let mut buf = payload.to_vec();
                    buf.push(b'\n');
                    write_half.write_all(&buf).await
                }
            };
            if res.is_err() {
                break; // 连接已断
            }
        }
        // channel 关闭（玩家下线）：关闭写半边，让客户端收到 EOF
        let _ = write_half.shutdown().await;
    });

    // ---- 发送 init（与旧协议保持字段形状）----
    send_to(
        &state,
        player_id,
        &json!({
            "type": "init",
            "id": player_id,
            "p2p_port": 0,
            "server": "0.0.0.0",
            "tcp_port": TCP_PORT,
        }),
    );

    // ---- 读循环：按协议解帧，逐条分发 ----
    let mut read_half = read_half;
    let read_result: Result<(), String> = async {
        match proto {
            Proto::LengthPrefix => {
                loop {
                    // 4 字节大端长度头
                    let mut header = [0u8; 4];
                    if read_half.read_exact(&mut header).await.is_err() {
                        return Ok(()); // 对端关闭
                    }
                    let len = u32::from_be_bytes(header) as usize;
                    if len == 0 || len > MAX_PAYLOAD {
                        return Err(format!("非法帧长 {}", len));
                    }
                    let mut payload = vec![0u8; len];
                    if read_half.read_exact(&mut payload).await.is_err() {
                        return Err("帧体不完整".into());
                    }
                    let text = match String::from_utf8(payload) {
                        Ok(t) => t,
                        Err(_) => {
                            log(&format!("玩家{}发来非 UTF-8 帧，忽略", player_id));
                            continue;
                        }
                    };
                    match serde_json::from_str::<Value>(&text) {
                        Ok(msg) => {
                            // 限速闸门：超限丢消息，连续违规踢连接
                            match limiter.check() {
                                Ok(()) => {
                                    handle_message(&state, db.as_ref(), player_id, &msg).await
                                }
                                Err(true) => {
                                    log(&format!("玩家{}触发限速踢出（洪水攻击？）", player_id));
                                    return Err("触发限速保护".into());
                                }
                                Err(false) => {
                                    if limiter.should_warn() {
                                        send_to(&state, player_id, &json!({"type": "rate_limited"}));
                                    }
                                }
                            }
                        }
                        Err(e) => log(&format!("玩家{}消息 JSON 错误: {}", player_id, e)),
                    }
                }
            }
            Proto::NewlineJson => {
                let mut buf: Vec<u8> = Vec::with_capacity(4096);
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match read_half.read(&mut chunk).await {
                        Ok(0) => return Ok(()), // 对端关闭
                        Ok(n) => n,
                        Err(e) => return Err(format!("读取错误: {}", e)),
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    // 逐行切出处理
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        let text = String::from_utf8_lossy(&line).trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if text.len() > MAX_LINE {
                            log(&format!("玩家{}超长行（{}B），忽略", player_id, text.len()));
                            continue;
                        }
                        match serde_json::from_str::<Value>(&text) {
                            Ok(msg) => {
                                // 限速闸门：超限丢消息，连续违规踢连接
                                match limiter.check() {
                                    Ok(()) => {
                                        handle_message(&state, db.as_ref(), player_id, &msg).await
                                    }
                                    Err(true) => {
                                        log(&format!("玩家{}触发限速踢出（洪水攻击？）", player_id));
                                        return Err("触发限速保护".into());
                                    }
                                    Err(false) => {
                                        if limiter.should_warn() {
                                            send_to(&state, player_id, &json!({"type": "rate_limited"}));
                                        }
                                    }
                                }
                            }
                            Err(e) => log(&format!("玩家{}消息 JSON 错误: {}", player_id, e)),
                        }
                    }
                    if buf.len() > MAX_LINE {
                        return Err("行缓冲超限（无换行）".into());
                    }
                }
            }
        }
    }
    .await;

    if let Err(e) = read_result {
        log(&format!("玩家{}读循环异常: {}", player_id, e));
    }

    // ---- 收尾：先清理登记，再等写 task ----
    // 注意顺序：State.players 里还持有 tx 的一份 clone，
    // 直接 drop 本地 tx 不会关闭 channel（写 task 的 recv 会永远等下去）。
    // 必须先 cleanup 移除 players 条目 -> channel 全部关闭 -> 写 task 自然退出。
    drop(tx);
    cleanup(&state, player_id);
    let _ = write_task.await;
}

/// 客户端协议类型
#[derive(Clone, Copy, PartialEq)]
enum Proto {
    LengthPrefix,
    NewlineJson,
}

// ==================== UDP 发现服务 ====================

async fn udp_discovery_loop(state: Shared) {
    let sock = match tokio::net::UdpSocket::bind(("0.0.0.0", UDP_DISCOVERY_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[服务器] UDP 绑定失败: {}", e);
            return;
        }
    };
    let mut buf = [0u8; 4096];
    loop {
        if let Ok((n, addr)) = sock.recv_from(&mut buf).await {
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let msg: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.get("type").and_then(|v| v.as_str()) == Some("discovery") {
                let response = build_discovery_response(&state);
                if let Ok(bytes) = serde_json::to_vec(&response) {
                    let _ = sock.send_to(&bytes, addr).await;
                }
            }
        }
    }
}

fn build_discovery_response(state: &Shared) -> Value {
    let mut rooms_info: Vec<Value> = Vec::new();
    if let Ok(g) = state.lock() {
        for (room_id, members) in g.rooms.iter() {
            let players: Vec<Value> = members
                .iter()
                .filter_map(|pid| g.players.get(pid))
                .map(|p| json!({"name": p.name}))
                .collect();
            rooms_info.push(json!({
                "room_id": room_id,
                "room_name": format!("房间 {}", room_id),
                "player_count": players.len(),
                "max_players": MAX_PLAYERS_PER_ROOM,
                "players": players,
            }));
        }
    }
    if rooms_info.is_empty() {
        rooms_info.push(json!({
            "room_id": "default",
            "room_name": "房间 default",
            "player_count": 0,
            "max_players": MAX_PLAYERS_PER_ROOM,
            "players": [],
        }));
    }
    json!({
        "type": "discovery_response",
        "server": "0.0.0.0",
        "server_name": SERVER_NAME,
        "tcp_port": TCP_PORT,
        "udp_port": UDP_DISCOVERY_PORT,
        "p2p_base_port": 0,
        "rooms": rooms_info,
    })
}

// ==================== 主程序 ====================

#[tokio::main]
async fn main() {
    let state: Shared = Arc::new(Mutex::new(State::default()));

    // ---- PostgreSQL 存储层（失败降级：转发功能不受影响）----
    // 连接串：环境变量 DATABASE_URL，默认 postgres://postgres:postgres@localhost:5432/rmva_p2p
    let db = db::Db::connect().await;
    let db_enabled = db.is_some();

    // UDP 发现服务
    let udp_state = state.clone();
    tokio::spawn(async move {
        udp_discovery_loop(udp_state).await;
    });

    // TCP 监听
    let listener = match TcpListener::bind(("0.0.0.0", TCP_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[服务器] TCP 绑定失败: {}（端口被占用？）", e);
            std::process::exit(1);
        }
    };
    println!("==================================================");
    println!("[服务器] P2P联机服务器 v2（Rust + PostgreSQL）启动");
    println!("[服务器] TCP: 0.0.0.0:{}", TCP_PORT);
    println!("[服务器] UDP发现: 0.0.0.0:{}", UDP_DISCOVERY_PORT);
    println!("[服务器] 双协议: 长度前缀帧(新DLL) / 换行JSON(旧客户端)");
    println!(
        "[服务器] 数据库: {}",
        if db_enabled { "已连接（账号/拍卖行可用）" } else { "不可用（降级模式：仅联机转发）" }
    );
    println!("[服务器] 防作弊: 每连接限速 30 msg/s + 服务器权威金币");
    println!("==================================================");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let st = state.clone();
                let db_clone = db.clone();
                tokio::spawn(async move {
                    handle_client(st, db_clone, stream).await;
                });
            }
            Err(e) => {
                log(&format!("接受连接错误: {}", e));
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
