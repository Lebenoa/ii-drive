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
        Some(s) => serde_json::from_str(s).map_err(|e| DbError::Shape(format!("rules shape: {e}"))),
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

/// Row id for a user's upload settings. Mirrors `setting_id`/`draft_id`.
fn upload_id(user_key: &str) -> String {
    format!("setting:upload_{}", user_key.replace([':', '-'], "_"))
}

/// Upload-split threshold in bytes (0 = never split), per user: several
/// accounts share the process, and one tenant must not resize another
/// tenant's uploads.
pub async fn get_split(db: &surrealdb::Surreal<Conn>, user_key: &str) -> Result<u64, DbError> {
    let mut res = db
        .query(format!("SELECT split_bytes FROM {}", upload_id(user_key)))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| r.get("split_bytes").cloned())
        .and_then(|v| v.as_u64())
        .unwrap_or(0))
}

pub async fn set_split(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
    bytes: u64,
) -> Result<(), DbError> {
    let mut res = db
        .query(format!(
            "UPSERT {} SET split_bytes = $b",
            upload_id(user_key)
        ))
        .bind(("b", bytes as i64))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Row the split threshold lived in before it was per-user.
const LEGACY_UPLOAD_ID: &str = "setting:upload";

/// Moves the pre-multi-tenant global split threshold to `owner`, returning 1
/// when it claimed something.
///
/// Without this, upgrading silently reset a configured threshold to 0 and the
/// next oversized upload would fail outright instead of being chunked. A user
/// who already has their own row keeps it, so the legacy row is merged once.
pub(super) async fn adopt_legacy_split(
    db: &surrealdb::Surreal<Conn>,
    owner: i64,
) -> Result<u64, DbError> {
    let mut res = db
        .query(format!("SELECT split_bytes FROM {LEGACY_UPLOAD_ID}"))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(bytes) = rows
        .into_iter()
        .next()
        .and_then(|r| r.get("split_bytes").cloned())
        .and_then(|v| v.as_u64())
    else {
        return Ok(0);
    };
    let key = owner.to_string();
    // Only adopt into an account that has not set its own threshold since.
    if get_split(db, &key).await? == 0 {
        set_split(db, &key, bytes).await?;
    }
    let mut res = db.query(format!("DELETE {LEGACY_UPLOAD_ID}")).await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(1)
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
    format!("setting:storage_{}", user_key.replace([':', '-'], "_"))
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
        Some(s) => {
            serde_json::from_str(s).map_err(|e| DbError::Shape(format!("channels shape: {e}")))
        }
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
        .query(format!(
            "UPSERT {} SET chats_json = $j",
            setting_id(user_key)
        ))
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
pub async fn clear_bot_draft(db: &surrealdb::Surreal<Conn>, user_key: &str) -> Result<(), DbError> {
    let mut res = db.query(format!("DELETE {}", draft_id(user_key))).await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Row id for a user's token epoch. Same key sanitising as `upload_id`.
fn epoch_id(user_key: &str) -> String {
    format!("setting:epoch_{}", user_key.replace([':', '-'], "_"))
}

/// The account's session-token epoch. Session tokens carry the epoch they
/// were minted under, so bumping it retires every token issued before the
/// bump — that is what makes logout a real revocation rather than a hint.
///
/// Absent means 0: an account that has never logged out is at epoch 0, and
/// its tokens say 0 too.
pub async fn get_token_epoch(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<u64, DbError> {
    let mut res = db
        .query(format!("SELECT epoch FROM {}", epoch_id(user_key)))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| r.get("epoch").cloned())
        .and_then(|v| v.as_u64())
        .unwrap_or(0))
}

/// Retires every token issued for this account and returns the new epoch.
///
/// Read-then-write rather than an atomic increment: two concurrent bumps can
/// only ever land on the same new value, and any value strictly greater than
/// what outstanding tokens carry revokes them just the same. Persisting it
/// is what keeps a logout honoured across a restart.
pub async fn bump_token_epoch(
    db: &surrealdb::Surreal<Conn>,
    user_key: &str,
) -> Result<u64, DbError> {
    let next = get_token_epoch(db, user_key).await? + 1;
    let mut res = db
        .query(format!("UPSERT {} SET epoch = $e", epoch_id(user_key)))
        .bind(("e", next as i64))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account that has never logged out sits at epoch 0, and a bump is
    /// monotonic and per-account — one tenant's logout must not invalidate
    /// another tenant's tokens.
    #[tokio::test]
    async fn token_epoch_defaults_to_zero_and_only_grows() {
        let db = crate::db::open_mem().await.expect("open test db");

        assert_eq!(get_token_epoch(&db, "11").await.expect("read"), 0);

        assert_eq!(bump_token_epoch(&db, "11").await.expect("bump"), 1);
        assert_eq!(get_token_epoch(&db, "11").await.expect("read"), 1);
        assert_eq!(bump_token_epoch(&db, "11").await.expect("bump"), 2);
        assert_eq!(get_token_epoch(&db, "11").await.expect("read"), 2);

        assert_eq!(
            get_token_epoch(&db, "22").await.expect("read"),
            0,
            "another tenant is unaffected"
        );
    }

    /// Upgrading must not silently reset a configured split threshold: an
    /// operator who chunked at 1 GiB would otherwise find oversized uploads
    /// failing outright instead of being split.
    #[tokio::test]
    async fn a_legacy_split_threshold_is_adopted_once() {
        let db = crate::db::open_mem().await.expect("open test db");
        let mut res = db
            .query(format!("UPSERT {LEGACY_UPLOAD_ID} SET split_bytes = $b"))
            .bind(("b", 1_073_741_824i64))
            .await
            .expect("plant the pre-multi-tenant row");
        let _ = res.take::<surrealdb::types::Value>(0usize).expect("upsert");

        assert_eq!(adopt_legacy_split(&db, 7).await.expect("adopt"), 1);
        assert_eq!(get_split(&db, "7").await.expect("read"), 1_073_741_824);

        // Idempotent: the legacy row is gone, so a second boot claims nothing
        // and cannot overwrite a threshold the user has since changed.
        assert_eq!(adopt_legacy_split(&db, 7).await.expect("adopt"), 0);
        set_split(&db, "7", 500).await.expect("retune");
        assert_eq!(adopt_legacy_split(&db, 7).await.expect("adopt"), 0);
        assert_eq!(get_split(&db, "7").await.expect("read"), 500);
    }
}
