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

/// How an accepted upload reaches Telegram.
///
/// `Stream` relays the client body straight into per-part uploaders — no disk
/// usage, but each part only starts draining once the sequential body feed
/// reaches it. `Spill` buffers the whole body to `spill_dir` first, then
/// drains every part at full aggregate rate: a faster tail on fast pipes,
/// paid for in temporary disk space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadStrategy {
    Stream,
    Spill,
}

/// Instance-wide tunables: the settings an operator actually revisits while
/// the server runs. They live here rather than in `config.toml` so changing
/// one is a request instead of an edit-and-restart — the file is left for
/// values that are set once (credentials, paths, the phone allowlists).
///
/// One row for the whole process, not one per user: an upload cap that each
/// tenant could raise for themselves would not be a cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    /// Largest accepted upload. Files above Telegram's per-document limit
    /// are chunked transparently, so this can exceed it.
    pub max_file_size: u64,
    /// Generate thumbnails for videos/images with ffmpeg when available.
    pub media_thumbs: bool,
    pub upload_strategy: UploadStrategy,
}

impl Default for Instance {
    fn default() -> Self {
        Instance {
            max_file_size: 2 * 1024 * 1024 * 1024,
            media_thumbs: true,
            upload_strategy: UploadStrategy::Stream,
        }
    }
}

/// The one instance row. Global, so it takes no user key.
const INSTANCE_ID: &str = "setting:instance";

/// The stored tunables, or None when nothing has written them yet.
///
/// The caller needs that distinction: absent means a fresh install (or one
/// whose values still sit in `config.toml`), which is the only moment seeding
/// them from the file is right. Fields are read individually and fall back to
/// the default one by one, so a row hand-edited through `/internal-db` into a
/// partial shape degrades to defaults instead of failing every upload.
pub async fn get_instance(db: &surrealdb::Surreal<Conn>) -> Result<Option<Instance>, DbError> {
    let mut res = db
        .query(format!(
            "SELECT max_file_size, media_thumbs, upload_strategy FROM {INSTANCE_ID}"
        ))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let fallback = Instance::default();
    Ok(Some(Instance {
        max_file_size: row
            .get("max_file_size")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(fallback.max_file_size),
        media_thumbs: row
            .get("media_thumbs")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(fallback.media_thumbs),
        upload_strategy: match row.get("upload_strategy").and_then(|v| v.as_str()) {
            Some("spill") => UploadStrategy::Spill,
            Some("stream") => UploadStrategy::Stream,
            _ => fallback.upload_strategy,
        },
    }))
}

pub async fn set_instance(db: &surrealdb::Surreal<Conn>, inst: &Instance) -> Result<(), DbError> {
    let mut res = db
        .query(format!(
            "UPSERT {INSTANCE_ID} SET max_file_size = $s, media_thumbs = $t, \
             upload_strategy = $u"
        ))
        .bind(("s", inst.max_file_size as i64))
        .bind(("t", inst.media_thumbs))
        .bind(("u", inst.upload_strategy.to_string()))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

impl std::fmt::Display for UploadStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UploadStrategy::Stream => "stream",
            UploadStrategy::Spill => "spill",
        })
    }
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

    /// `None` is what tells startup the values still live in `config.toml`,
    /// so an unwritten row must not read back as defaults.
    #[tokio::test]
    async fn instance_settings_round_trip() {
        let db = crate::db::open_mem().await.expect("open test db");

        assert_eq!(get_instance(&db).await.expect("read"), None);

        let want = Instance {
            max_file_size: 500 * 1024 * 1024,
            media_thumbs: false,
            upload_strategy: UploadStrategy::Spill,
        };
        set_instance(&db, &want).await.expect("write");
        assert_eq!(get_instance(&db).await.expect("read"), Some(want));
    }

    /// The row is reachable from `/internal-db`, so a hand-written partial
    /// one must degrade field by field rather than fail every upload.
    #[tokio::test]
    async fn a_partial_instance_row_falls_back_per_field() {
        let db = crate::db::open_mem().await.expect("open test db");
        db.query(format!("UPSERT {INSTANCE_ID} SET media_thumbs = false"))
            .await
            .expect("plant a partial row");

        let got = get_instance(&db).await.expect("read").expect("row exists");
        assert!(!got.media_thumbs, "the written field is honoured");
        assert_eq!(got.max_file_size, Instance::default().max_file_size);
        assert_eq!(got.upload_strategy, UploadStrategy::Stream);
    }
}
