//! db.rs — PostgreSQL 连接池与建表初始化
//!
//! 设计要点：
//! - 连接串优先读环境变量 DATABASE_URL，未设置则用本机默认
//! - 建表语句全部幂等（IF NOT EXISTS），重复启动无副作用
//! - 数据库不可用时降级运行：聊天/战斗转发不受影响，拍卖/登录返回错误
//! - 全部玩家资产（金币）以数据库为唯一权威，杜绝客户端刷钱

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};

/// 默认连接串（本机开发用；部署到服务器时改环境变量或此处）
const DEFAULT_DB_URL: &str = "postgres://postgres:postgres@localhost:5432/rmva_p2p";

/// 应用层共享的数据库句柄
#[derive(Clone)]
pub struct Db {
    pub pool: Pool,
}

impl Db {
    /// 创建连接池并初始化表结构
    /// 返回 None 表示数据库不可用（服务器将降级运行）
    pub async fn connect() -> Option<Db> {
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DB_URL.to_string());

        // 解析连接串为 tokio-postgres 配置
        let pg_config = match url.parse::<tokio_postgres::Config>() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[数据库] 连接串解析失败: {}（DATABASE_URL={})", e, url);
                return None;
            }
        };

        // deadpool 管理器：空闲连接快速回收，稳态保持少量连接
        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let mgr = Manager::from_config(pg_config, tokio_postgres::NoTls, mgr_config);

        // 池大小 8：本游戏规模（几十人在线）绰绰有余
        let pool = Pool::builder(mgr)
            .max_size(8)
            .build()
            .expect("构建连接池失败（配置错误）");

        let db = Db { pool };

        // 建表初始化（失败则视为数据库不可用）
        match db.init_schema().await {
            Ok(()) => {
                println!("[数据库] 已连接并初始化表结构");
                Some(db)
            }
            Err(e) => {
                eprintln!("[数据库] 初始化失败: {}", e);
                eprintln!("[数据库] 提示：请确认 PostgreSQL 已启动、数据库 rmva_p2p 已创建");
                eprintln!("[数据库]   创建数据库命令: createdb rmva_p2p  或  psql -c \"CREATE DATABASE rmva_p2p;\"");
                eprintln!("[数据库] 服务器将以降级模式运行（联机转发正常，拍卖行/账号不可用）");
                None
            }
        }
    }

    /// 建表（幂等）。错误转 String：连接失败等一律走降级，绝不让进程 panic
    async fn init_schema(&self) -> Result<(), String> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(|e| format!("获取连接失败: {}", e))?;

        // ---- 账号表：金币等资产的唯一权威 ----
        // gold 用 BIGINT：RMVA 金币上限 99,999,999，留足余量
        conn.batch_execute(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                id            SERIAL PRIMARY KEY,
                username      TEXT UNIQUE NOT NULL,
                pass_hash     TEXT NOT NULL,
                gold          BIGINT NOT NULL DEFAULT 0,
                created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                last_login_at TIMESTAMPTZ
            );

            -- ---- 拍卖行挂单表 ----
            -- status: listed（在售）/ sold（已售）/ cancelled（已下架）
            CREATE TABLE IF NOT EXISTS auction_items (
                id         SERIAL PRIMARY KEY,
                seller_id  INT NOT NULL REFERENCES accounts(id),
                item_id    INT NOT NULL,
                quantity   INT NOT NULL,
                price      BIGINT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'listed',
                buyer_id   INT REFERENCES accounts(id),
                created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );

            -- 挂单查询索引：列表页按状态扫全表是常态操作
            CREATE INDEX IF NOT EXISTS idx_auction_status ON auction_items(status);

            -- ---- 交易流水（审计追溯，防作弊排查依据）----
            CREATE TABLE IF NOT EXISTS transactions (
                id         SERIAL PRIMARY KEY,
                buyer_id   INT REFERENCES accounts(id),
                seller_id  INT REFERENCES accounts(id),
                item_id    INT NOT NULL,
                quantity   INT NOT NULL,
                price      BIGINT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            "#,
        )
        .await
        .map_err(|e| format!("建表失败: {}", e))?;
        Ok(())
    }
}
