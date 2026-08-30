//! auth.rs — 账号注册/登录（argon2 密码哈希）
//!
//! 防作弊设计：
//! - 密码绝不明文存储：argon2id 哈希（抗 GPU 撞库的现代标准算法）
//! - 用户名/密码长度与字符集校验在最外层拦截，脏数据不进数据库
//! - 登录成功发一次性会话 token（随机 32 字节十六进制），后续拍卖操作凭 token 绑定账号
//!
//! 会话模型：token 存内存 State（服务器重启即失效，玩家重新登录即可，
//! 对游戏场景足够；持久会话属后续可选增强）。

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::RngCore;
use serde_json::{json, Value};

use crate::db::Db;

/// 用户名规则：3~20 字符，字母/数字/下划线/中文
fn valid_username(s: &str) -> bool {
    let n = s.chars().count();
    (3..=20).contains(&n)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || (c as u32) >= 0x4E00)
}

/// 密码规则：6~64 字符，不允许空白字符
fn valid_password(s: &str) -> bool {
    (6..=64).contains(&s.chars().count()) && !s.chars().any(|c| c.is_whitespace())
}

/// 哈希密码（argon2id 默认参数：m=19456KB t=2 p=1）
fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("哈希失败: {}", e))
}

/// 校验密码是否匹配（常量时间比较，由 argon2 内部保证）
fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false, // 库里的哈希格式损坏，一律拒绝
    }
}

/// 生成 64 位十六进制随机 token
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// 注册结果
pub struct AuthedAccount {
    pub account_id: i32,
    pub username: String,
    pub gold: i64,
    pub token: String,
}

/// 处理注册：username + password
pub async fn register(db: &Db, username: &str, password: &str) -> Result<AuthedAccount, String> {
    if !valid_username(username) {
        return Err("用户名需 3-20 位（字母/数字/下划线/中文）".into());
    }
    if !valid_password(password) {
        return Err("密码需 6-64 位且不含空白字符".into());
    }

    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    // 用户名唯一性检查（防并发重名：靠数据库唯一约束兜底）
    let exists: bool = conn
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE username = $1)",
            &[&username],
        )
        .await
        .map_err(|e| format!("数据库错误: {}", e))?
        .get(0);
    if exists {
        return Err("用户名已被占用".into());
    }

    let pass_hash = hash_password(password)?;
    // 注册送 500 金启动资金（可调）
    let row = match conn
        .query_one(
            "INSERT INTO accounts (username, pass_hash, gold, last_login_at)
             VALUES ($1, $2, 500, now())
             RETURNING id, gold",
            &[&username, &pass_hash],
        )
        .await
    {
        Ok(r) => r,
        // 唯一约束冲突 = 并发注册撞名，走友好提示
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => {
            return Err("用户名已被占用".into());
        }
        Err(e) => return Err(format!("注册失败: {}", e)),
    };

    Ok(AuthedAccount {
        account_id: row.get(0),
        username: username.to_string(),
        gold: row.get(1),
        token: new_token(),
    })
}

/// 处理登录：验证密码、更新登录时间、返回账号信息 + 会话 token
pub async fn login(db: &Db, username: &str, password: &str) -> Result<AuthedAccount, String> {
    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    let row = conn
        .query_opt(
            "SELECT id, pass_hash, gold FROM accounts WHERE username = $1",
            &[&username],
        )
        .await
        .map_err(|e| format!("数据库错误: {}", e))?;

    // 用户不存在与密码错误统一提示，避免账号名枚举攻击
    let (account_id, stored_hash, gold) = match row {
        Some(r) => (r.get::<_, i32>(0), r.get::<_, String>(1), r.get::<_, i64>(2)),
        None => return Err("用户名或密码错误".into()),
    };

    if !verify_password(password, &stored_hash) {
        return Err("用户名或密码错误".into());
    }

    // 更新最后登录时间（失败不阻塞登录）
    let _ = conn
        .execute("UPDATE accounts SET last_login_at = now() WHERE id = $1", &[&account_id])
        .await;

    Ok(AuthedAccount {
        account_id,
        username: username.to_string(),
        gold,
        token: new_token(),
    })
}

/// 构造登录/注册成功回给客户端的消息
pub fn auth_ok_msg(result: &str, acc: &AuthedAccount) -> Value {
    json!({
        "type": "auth_result",
        "result": result,
        "ok": true,
        "account_id": acc.account_id,
        "username": acc.username,
        "gold": acc.gold,
        "token": acc.token,
    })
}

/// 构造登录/注册失败消息
pub fn auth_err_msg(result: &str, err: &str) -> Value {
    json!({
        "type": "auth_result",
        "result": result,
        "ok": false,
        "error": err,
    })
}
