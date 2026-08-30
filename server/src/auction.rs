//! auction.rs — 拍卖行（服务器权威）
//!
//! 防作弊核心设计：
//! 1. 金币完全由服务器保管：所有加减钱都发生在数据库事务内，
//!    客户端报上来的余额一律不信任（改内存 gold 无效）
//! 2. 购买走「行锁事务」：SELECT ... FOR UPDATE 锁挂单行，
//!    两个玩家同时抢购同一件商品时只有一人成功，另一人收到失败
//! 3. 数值边界校验：价格/数量/物品ID 在入口处全部检查，
//!    拒绝负数、零、超天价（防溢出与洗钱式挂单）
//! 4. 交易流水全量落库：出现纠纷时可追溯每一笔成交
//!
//! 并发死锁说明：极端情况下（A 买 B 的货、B 同时买 A 的货）事务
//! 可能互锁，PostgreSQL 死锁检测器会中止其中一单并返回错误码，
//! 客户端表现为「购买失败，请重试」——不丢钱，是安全的兜底。

use serde_json::{json, Value};

use crate::db::Db;

/// 价格上限（与 RMVA 金币显示上限一致）
pub const MAX_PRICE: i64 = 99_999_999;
/// 单件挂单数量上限
pub const MAX_QUANTITY: i32 = 99;

// ==================== 通用校验 ====================

/// 校验上架参数（item_id, quantity, price）
fn validate_listing(item_id: i64, quantity: i64, price: i64) -> Result<(), String> {
    // ★ 三类编码：道具=1..99999 武器=100000+ID 防具=200000+ID（客户端撞号偏移方案）
    //   上限 299999 = 200000 + 99999（防具最大编码）
    if !(1..=299_999).contains(&item_id) {
        return Err("物品 ID 非法".into());
    }
    if !(1..=MAX_QUANTITY as i64).contains(&quantity) {
        return Err("数量需在 1-99 之间".into());
    }
    if !(1..=MAX_PRICE).contains(&price) {
        return Err("单价需在 1-99999999 之间".into());
    }
    // 总价溢出保护：99 × 99999999 < i64::MAX，数学上必然安全，显式断言防御式编程
    if price.checked_mul(quantity).is_none() {
        return Err("总价溢出".into());
    }
    Ok(())
}

// ==================== 挂单列表 ====================

/// 拉取在售挂单列表（含卖家名，标记哪些是自己的）
pub async fn list(db: &Db, viewer_id: i32) -> Result<Value, String> {
    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    let rows = conn
        .query(
            "SELECT a.id, a.seller_id, COALESCE(acc.username, '?'), a.item_id, a.quantity, a.price
             FROM auction_items a
             JOIN accounts acc ON acc.id = a.seller_id
             WHERE a.status = 'listed'
             ORDER BY a.created_at DESC
             LIMIT 200",
            &[],
        )
        .await
        .map_err(|e| format!("查询失败: {}", e))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let seller_id: i32 = r.get(1);
            json!({
                "id": r.get::<_, i32>(0),          // 挂单号（购买/下架时引用）
                "seller_id": seller_id,
                "seller_name": r.get::<_, String>(2),
                "item_id": r.get::<_, i32>(3),      // RMVA 物品 ID（客户端按 $data_items 索引）
                "quantity": r.get::<_, i32>(4),
                "price": r.get::<_, i64>(5),        // 单价（总价 = 单价 × 数量）
                "mine": seller_id == viewer_id,     // 前端高亮自己的挂单用
            })
        })
        .collect();

    Ok(json!({ "type": "auction_list_result", "ok": true, "items": items }))
}

/// 拉取我的金币和在售挂单
pub async fn my(db: &Db, account_id: i32) -> Result<Value, String> {
    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    let gold: i64 = conn
        .query_one("SELECT gold FROM accounts WHERE id = $1", &[&account_id])
        .await
        .map_err(|e| format!("查询失败: {}", e))?
        .get(0);

    let rows = conn
        .query(
            "SELECT id, item_id, quantity, price, status
             FROM auction_items WHERE seller_id = $1 AND status = 'listed'
             ORDER BY created_at DESC",
            &[&account_id],
        )
        .await
        .map_err(|e| format!("查询失败: {}", e))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<_, i32>(0),
                "item_id": r.get::<_, i32>(1),
                "quantity": r.get::<_, i32>(2),
                "price": r.get::<_, i64>(3),
                "status": r.get::<_, String>(4),
            })
        })
        .collect();

    Ok(json!({
        "type": "auction_my_result",
        "ok": true,
        "gold": gold,
        "items": items,
    }))
}

// ==================== 上架 ====================

/// 上架：只登记挂单（物品仍在客户端背包里，成交后通知扣除）
pub async fn sell(
    db: &Db,
    account_id: i32,
    item_id: i64,
    quantity: i64,
    price: i64,
) -> Result<Value, String> {
    validate_listing(item_id, quantity, price)?;

    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    let row = conn
        .query_one(
            "INSERT INTO auction_items (seller_id, item_id, quantity, price)
             VALUES ($1, $2, $3, $4) RETURNING id",
            &[&account_id, &(item_id as i32), &(quantity as i32), &price],
        )
        .await
        .map_err(|e| format!("上架失败: {}", e))?;

    Ok(json!({
        "type": "auction_sell_result",
        "ok": true,
        "listing_id": row.get::<_, i32>(0),
        "item_id": item_id,
        "quantity": quantity,
        "price": price,
    }))
}

// ==================== 购买（核心：事务 + 行锁）====================

/// 购买结果：买家消息 + 可能的卖家通知（卖家在线时推送）
pub struct BuyOutcome {
    pub buyer_msg: Value,
    pub seller_notify: Option<(i32, Value)>,
}

/// 购买挂单：一口价，整单成交
///
/// 事务流程（防双买/防刷钱的全部要点）：
///   1. FOR UPDATE 锁挂单行 -> 校验在售
///   2. FOR UPDATE 锁买家账号行 -> 校验余额
///   3. 同事务内：买家扣钱、卖家加钱、挂单置 sold、写流水
pub async fn buy(db: &Db, buyer_id: i32, listing_id: i64) -> Result<BuyOutcome, String> {
    if !(1..=i64::from(i32::MAX)).contains(&listing_id) {
        return Err("挂单号非法".into());
    }
    let listing_id = listing_id as i32;

    let mut conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    // ---- 事务开始 ----
    let tx = conn
        .transaction()
        .await
        .map_err(|e| format!("开启事务失败: {}", e))?;

    // 1. 锁挂单行并校验状态（并发的第二个买家会阻塞在这里）
    let listing = match tx
        .query_opt(
            "SELECT seller_id, item_id, quantity, price, status
             FROM auction_items WHERE id = $1 FOR UPDATE",
            &[&listing_id],
        )
        .await
        .map_err(|e| format!("查询挂单失败: {}", e))?
    {
        Some(r) => r,
        None => return Err("挂单不存在".into()),
    };

    let seller_id: i32 = listing.get(0);
    let item_id: i32 = listing.get(1);
    let quantity: i32 = listing.get(2);
    let price: i64 = listing.get(3);
    let status: String = listing.get(4);

    if status != "listed" {
        return Err("手慢了，该商品已被购买或下架".into());
    }
    if seller_id == buyer_id {
        return Err("不能购买自己上架的物品".into());
    }
    let total = price * (quantity as i64); // 上架时已做过溢出校验，此处安全

    // 2. 锁买家行并校验余额（FOR UPDATE 防止并发消费同一笔钱）
    let buyer_gold: i64 = tx
        .query_one("SELECT gold FROM accounts WHERE id = $1 FOR UPDATE", &[&buyer_id])
        .await
        .map_err(|e| format!("查询余额失败: {}", e))?
        .get(0);

    if buyer_gold < total {
        return Err(format!("金币不足（需要 {}，持有 {}）", total, buyer_gold));
    }

    // 3. 四步写入，任何一步失败整体回滚（钱物一致性的保证）
    //    扣钱带 WHERE gold >= $x 双保险，即使逻辑有 bug 也不可能出现负余额
    let n = tx
        .execute(
            "UPDATE accounts SET gold = gold - $1 WHERE id = $2 AND gold >= $1",
            &[&total, &buyer_id],
        )
        .await
        .map_err(|e| format!("扣款失败: {}", e))?;
    if n != 1 {
        return Err("扣款失败（余额变动异常）".into());
    }

    tx.execute(
        "UPDATE accounts SET gold = gold + $1 WHERE id = $2",
        &[&total, &seller_id],
    )
    .await
    .map_err(|e| format!("打款失败: {}", e))?;

    tx.execute(
        "UPDATE auction_items SET status = 'sold', buyer_id = $1, updated_at = now()
         WHERE id = $2",
        &[&buyer_id, &listing_id],
    )
    .await
    .map_err(|e| format!("更新挂单失败: {}", e))?;

    tx.execute(
        "INSERT INTO transactions (buyer_id, seller_id, item_id, quantity, price)
         VALUES ($1, $2, $3, $4, $5)",
        &[&buyer_id, &seller_id, &item_id, &quantity, &total],
    )
    .await
    .map_err(|e| format!("记录流水失败: {}", e))?;

    tx.commit()
        .await
        .map_err(|e| format!("提交事务失败: {}", e))?;

    let new_gold = buyer_gold - total;

    // ---- 卖家在线通知（卖家不在线则金币静静躺在数据库里，下次登录可见）----
    let seller_notify = (
        seller_id,
        json!({
            "type": "auction_sold",
            "item_id": item_id,
            "quantity": quantity,
            "gold_earned": total,
            "gold": 0i64, // 占位，main 层投递时会补上卖家当前余额
        }),
    );

    Ok(BuyOutcome {
        buyer_msg: json!({
            "type": "auction_buy_result",
            "ok": true,
            "listing_id": listing_id,
            "item_id": item_id,       // 客户端凭此给玩家背包加物品
            "quantity": quantity,
            "cost": total,
            "gold": new_gold,         // 服务器权威余额（客户端以它为准校准）
        }),
        seller_notify: Some(seller_notify),
    })
}

// ==================== 查询工具 ====================

/// 查询账号当前金币（服务器权威余额，用于成交推送等场景）
/// 查询失败返回 -1（调用方视为"未知"，不下发错误值）
pub async fn get_gold(db: &Db, account_id: i32) -> i64 {
    match db.pool.get().await {
        Ok(conn) => match conn
            .query_opt("SELECT gold FROM accounts WHERE id = $1", &[&account_id])
            .await
        {
            Ok(Some(row)) => row.get(0),
            _ => -1,
        },
        Err(_) => -1,
    }
}

// ==================== 下架 ====================

/// 下架自己的挂单（已售挂单不可下架）
pub async fn cancel(db: &Db, account_id: i32, listing_id: i64) -> Result<Value, String> {
    if !(1..=i64::from(i32::MAX)).contains(&listing_id) {
        return Err("挂单号非法".into());
    }
    let listing_id = listing_id as i32;

    let conn = db.pool.get().await.map_err(|e| format!("数据库繁忙: {}", e))?;

    // WHERE 带归属校验：只能下架自己的单子，且只能下架在售的
    // RETURNING item_id/quantity：客户端据此把物品退回背包
    let row = conn
        .query_opt(
            "UPDATE auction_items SET status = 'cancelled', updated_at = now()
             WHERE id = $1 AND seller_id = $2 AND status = 'listed'
             RETURNING item_id, quantity",
            &[&listing_id, &account_id],
        )
        .await
        .map_err(|e| format!("下架失败: {}", e))?;

    let row = match row {
        Some(r) => r,
        None => return Err("挂单不存在、不归属你或已成交".into()),
    };

    Ok(json!({
        "type": "auction_cancel_result",
        "ok": true,
        "listing_id": listing_id,
        "item_id": row.get::<_, i32>(0),
        "quantity": row.get::<_, i32>(1),
    }))
}
