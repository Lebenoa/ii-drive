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

/// One schemaless row per user, keyed like the other per-user settings.
fn bots_id(owner: i64) -> String {
    format!("setting:bots_{}", owner.to_string().replace('-', "_"))
}

/// Reads a pool out of one `setting` row; empty when the row is absent.
async fn read_pool(db: &surrealdb::Surreal<Conn>, id: &str) -> Result<Vec<BotInfo>, DbError> {
    let mut res = db.query(format!("SELECT bots_json FROM {id}")).await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };
    row.get("bots_json")
        .and_then(|v| v.as_str())
        .map_or_else(
            || Ok(Vec::new()),
            |s| serde_json::from_str(s).map_err(|e| DbError::Shape(format!("bots shape: {e}"))),
        )
}

/// All bots configured by `owner`; empty when none.
pub async fn get_bots(db: &surrealdb::Surreal<Conn>, owner: i64) -> Result<Vec<BotInfo>, DbError> {
    read_pool(db, &bots_id(owner)).await
}

/// Replaces the whole pool of one user.
pub async fn set_bots(
    db: &surrealdb::Surreal<Conn>,
    owner: i64,
    bots: &[BotInfo],
) -> Result<(), DbError> {
    let json =
        serde_json::to_string(bots).map_err(|e| DbError::Shape(format!("bots serialize: {e}")))?;
    let mut res = db
        .query(format!("UPSERT {} SET bots_json = $j", bots_id(owner)))
        .bind(("j", json))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Drops a single bot from a user's pool.
pub async fn remove_bot(db: &surrealdb::Surreal<Conn>, owner: i64, id: i64) -> Result<(), DbError> {
    let mut bots = get_bots(db, owner).await?;
    bots.retain(|b| b.id != id);
    set_bots(db, owner, &bots).await
}
