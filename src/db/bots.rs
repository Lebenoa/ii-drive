use serde::{Deserialize, Serialize};

use super::{Conn, DbError};

/// A Telegram bot used for downloads. Several can be configured so
/// download traffic spreads across accounts instead of hammering one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotInfo {
    pub token: String,
    pub username: String,
    pub id: i64,
}

/// All configured bots; empty when none.
pub async fn get_bots(db: &surrealdb::Surreal<Conn>) -> Result<Vec<BotInfo>, DbError> {
    let mut res = db.query("SELECT bots_json FROM setting:bots").await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };
    match row.get("bots_json").and_then(|v| v.as_str()) {
        Some(s) => serde_json::from_str(s)
            .map_err(|e| DbError::Shape(format!("bots shape: {e}"))),
        None => Ok(Vec::new()),
    }
}

/// Replaces the whole bot pool.
pub async fn set_bots(
    db: &surrealdb::Surreal<Conn>,
    bots: &[BotInfo],
) -> Result<(), DbError> {
    let json =
        serde_json::to_string(bots).map_err(|e| DbError::Shape(format!("bots serialize: {e}")))?;
    let mut res = db
        .query("UPSERT setting:bots SET bots_json = $j")
        .bind(("j", json))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Drops a single bot from the pool.
pub async fn remove_bot(db: &surrealdb::Surreal<Conn>, id: i64) -> Result<(), DbError> {
    let mut bots = get_bots(db).await?;
    bots.retain(|b| b.id != id);
    set_bots(db, &bots).await
}
