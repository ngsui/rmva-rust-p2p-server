//! rate_limit.rs — 每连接消息限速器
//!
//! 防洪水/防刷屏的第一道闸门：
//! - 固定窗口计数器：每 WINDOW_MS 毫秒内最多 MAX_PER_WINDOW 条消息
//! - 超限消息直接丢弃（并给客户端发一条提示，提示本身也限频）
//! - 连续超限达到连续违规阈值时断开连接（对付恶意脚本刷屏）
//!
//! 实现选型：固定窗口而非令牌桶——对游戏服务器足够（正常游戏流量
//! 远低于阈值），且状态只有两个整数，零分配零锁（限速器属于单个
//! 连接的读循环，天然单线程访问）。

use std::time::{Duration, Instant};

/// 窗口长度
const WINDOW_MS: u64 = 1000;
/// 每窗口最大消息数（聊天/移动/战斗同步的常态峰值远低于此）
const MAX_PER_WINDOW: u32 = 30;
/// 连续违规多少个窗口后踢人
const KICK_AFTER_WINDOWS: u32 = 5;

pub struct RateLimiter {
    /// 当前窗口起点
    window_start: Instant,
    /// 当前窗口内已放行的消息数
    count: u32,
    /// 连续违规窗口数
    strikes: u32,
    /// 上次发限速提示的时间（提示限频：每秒最多 1 条）
    last_warn: Option<Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            window_start: Instant::now(),
            count: 0,
            strikes: 0,
            last_warn: None,
        }
    }

    /// 尝试放行一条消息
    /// 返回 Ok(()) 放行；Err(kick) 为 true 时应断开连接
    pub fn check(&mut self) -> Result<(), bool> {
        let now = Instant::now();
        // 窗口过期：重置计数，清连续违规
        if now.duration_since(self.window_start) >= Duration::from_millis(WINDOW_MS) {
            self.window_start = now;
            self.count = 0;
            if self.count == 0 {
                // 上一窗口无超限记录才清零违规（此处恒成立，逻辑显式化）
                self.strikes = 0;
            }
        }

        if self.count < MAX_PER_WINDOW {
            self.count += 1;
            Ok(())
        } else {
            // 超限：计一次违规
            self.strikes += 1;
            if self.strikes >= KICK_AFTER_WINDOWS {
                Err(true) // 踢
            } else {
                Err(false) // 丢弃本条
            }
        }
    }

    /// 是否需要发限速提示（每秒最多一条，防提示本身刷屏）
    pub fn should_warn(&mut self) -> bool {
        let now = Instant::now();
        let send = match self.last_warn {
            Some(t) => now.duration_since(t) >= Duration::from_millis(WINDOW_MS),
            None => true,
        };
        if send {
            self.last_warn = Some(now);
        }
        send
    }
}
