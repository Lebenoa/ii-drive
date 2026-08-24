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

/// The single pool every account shared before multi-tenancy. Left in place
/// until `adopt_unowned` hands it to the account that owns the old session.
const LEGACY_ID: &str = "setting:bots";

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
    match row.get("bots_json").and_then(|v| v.as_str()) {
        Some(s) => serde_json::from_str(s).map_err(|e| DbError::Shape(format!("bots shape: {e}"))),
        None => Ok(Vec::new()),
    }
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

/// Moves the pre-multi-tenant global pool to `owner`, returning 1 when it
/// claimed something. A user who already has a pool keeps it, so the legacy
/// row is only ever merged once; the second call finds nothing to move.
pub(super) async fn adopt_legacy_pool(
    db: &surrealdb::Surreal<Conn>,
    owner: i64,
) -> Result<u64, DbError> {
    let legacy = read_pool(db, LEGACY_ID).await?;
    if legacy.is_empty() {
        return Ok(0);
    }
    let mut mine = get_bots(db, owner).await?;
    // Re-adding a bot the user already has would double its share of
    // download traffic.
    let held: Vec<i64> = mine.iter().map(|b| b.id).collect();
    mine.extend(legacy.into_iter().filter(|l| !held.contains(&l.id)));
    set_bots(db, owner, &mine).await?;
    let mut res = db.query(format!("DELETE {LEGACY_ID}")).await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(1)
}
