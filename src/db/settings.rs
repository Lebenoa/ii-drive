use serde::{Deserialize, Serialize};

use super::{Conn, DbError};

/// One auto-upload routing rule: files whose mime starts with `mime` land
/// in `folder`. Order matters — first match wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub mime: String,
    pub folder: String,
}

/// Per-user routing rules, ordered; empty when none configured.
pub async fn get_rules(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<Vec<RouteRule>, DbError> {
    let id = format!("setting:rules_{}", user_key.replace([':', '-'], "_"));
    let mut res = db.query(format!("SELECT rules_json FROM {id}")).await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };
    match row.get("rules_json").and_then(|v| v.as_str()) {
        Some(s) => {
            serde_json::from_str(s).map_err(|e| DbError::Shape(format!("rules shape: {e}")))
        }
        None => Ok(Vec::new()),
    }
}

pub async fn set_rules(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
    rules: &[RouteRule],
) -> Result<(), DbError> {
    let json = serde_json::to_string(rules)
        .map_err(|e| DbError::Shape(format!("rules serialize: {e}")))?;
    let id = format!("setting:rules_{}", user_key.replace([':', '-'], "_"));
    let mut res = db
        .query(format!("UPSERT {id} SET rules_json = $j"))
        .bind(("j", json))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Upload-split threshold in bytes (0 = never split); global setting.
pub async fn get_split(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    let mut res = db.query("SELECT split_bytes FROM setting:upload").await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| r.get("split_bytes").cloned())
        .and_then(|v| v.as_u64())
        .unwrap_or(0))
}

pub async fn set_split(db: &surrealdb::Surreal<Conn>, bytes: u64) -> Result<(), DbError> {
    let mut res = db
        .query("UPSERT setting:upload SET split_bytes = $b")
        .bind(("b", bytes as i64))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// One selected storage channel for a user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSel {
    /// Chat key used for peer resolution: "me", "@username" or "-100<id>".
    pub chat: String,
    pub title: String,
}

/// Channels are stored on one schemaless row per user; the JSON payload keeps
/// the shape flexible without schema migrations.
fn setting_id(user_key: &str) -> String {
    format!(
        "setting:storage_{}",
        user_key.replace([':', '-'], "_")
    )
}

/// Channels the user picked as upload targets; empty when none chosen yet.
pub async fn get_channels(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<Vec<ChannelSel>, DbError> {
    let mut res = db
        .query(format!("SELECT chats_json FROM {}", setting_id(user_key)))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(Vec::new());
    };
    match row.get("chats_json").and_then(|v| v.as_str()) {
        Some(s) => serde_json::from_str(s)
            .map_err(|e| DbError::Shape(format!("channels shape: {e}"))),
        None => Ok(Vec::new()),
    }
}

pub async fn set_channels(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
    chats: Vec<ChannelSel>,
) -> Result<(), DbError> {
    let json = serde_json::to_string(&chats)
        .map_err(|e| DbError::Shape(format!("channels serialize: {e}")))?;
    let mut res = db
        .query(format!("UPSERT {} SET chats_json = $j", setting_id(user_key)))
        .bind(("j", json))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// A half-finished @BotFather `/newbot` conversation. BotFather keeps its
/// own pending question, so abandoning the wizard leaves it waiting for an
/// answer forever. Persisting the transcript lets the wizard resume that
/// same conversation instead of firing a second `/newbot` at it — and keeps
/// an issued token from being lost before the bot joins the pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BotDraft {
    /// Transcript, oldest first.
    pub log: Vec<DraftMsg>,
    /// Token once BotFather issued one; empty until then.
    pub token: String,
    /// Unix seconds of the last exchange, for staleness display.
    pub updated_at: i64,
}

/// One line of a draft transcript. `me` is what we sent, `bf` the reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftMsg {
    pub who: String,
    pub text: String,
}

fn draft_id(user_key: &str) -> String {
    format!("setting:botdraft_{}", user_key.replace([':', '-'], "_"))
}

/// The user's pending bot-creation draft, or None when the wizard is idle.
pub async fn get_bot_draft(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<Option<BotDraft>, DbError> {
    let mut res = db
        .query(format!("SELECT draft_json FROM {}", draft_id(user_key)))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    match row.get("draft_json").and_then(|v| v.as_str()) {
        Some(s) => serde_json::from_str(s)
            .map(Some)
            .map_err(|e| DbError::Shape(format!("bot draft shape: {e}"))),
        None => Ok(None),
    }
}

pub async fn set_bot_draft(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
    draft: &BotDraft,
) -> Result<(), DbError> {
    let json = serde_json::to_string(draft)
        .map_err(|e| DbError::Shape(format!("bot draft serialize: {e}")))?;
    let mut res = db
        .query(format!("UPSERT {} SET draft_json = $j", draft_id(user_key)))
        .bind(("j", json))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Forgets the draft. Used once the bot reaches the pool, and when the user
/// cancels the conversation outright.
pub async fn clear_bot_draft(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<(), DbError> {
    let mut res = db
        .query(format!("DELETE {}", draft_id(user_key)))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}
