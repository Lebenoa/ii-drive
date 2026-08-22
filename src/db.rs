use serde::{Deserialize, Serialize};

type Conn = surrealdb::engine::local::Db;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sur(#[from] surrealdb::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Shape(String),
}
pub async fn open(path: &str) -> Result<surrealdb::Surreal<Conn>, DbError> {
    if let Some(parent) = std::path::Path::new(path).parent().filter(|p| !p.is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::SurrealKv>(path).await?;
    db.use_ns("drive").await?;
    db.use_db("drive").await?;
    // SELECTs against a not-yet-created table error out; define up front so
    // a fresh install serves an empty list instead of a 500.
    let mut res = db
        .query("DEFINE TABLE IF NOT EXISTS file; DEFINE TABLE IF NOT EXISTS folder; \
                DEFINE TABLE IF NOT EXISTS setting")
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    let _ = res.take::<surrealdb::types::Value>(1usize)?;
    migrate(&db).await?;
    Ok(db)
}

/// Latest schema version; a database without a recorded version is v1.
const SCHEMA_LATEST: u64 = 3;

async fn schema_version(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    Ok(schema_version_recorded(db).await?.unwrap_or(1))
}

async fn set_schema_version(db: &surrealdb::Surreal<Conn>, v: u64) -> Result<(), DbError> {
    let mut res = db
        .query("UPSERT setting:schema SET version = $v")
        .bind(("v", v as i64))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Some(version) when `setting:schema` exists, None on a database that has
/// never been through migrate().
async fn schema_version_recorded(
    db: &surrealdb::Surreal<Conn>,
) -> Result<Option<u64>, DbError> {
    let mut res = db.query("SELECT version FROM setting:schema").await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    Ok(rows
        .first()
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_u64()))
}

/// True when the store holds no user data at all (fresh install). An old
/// pre-versioning database with rows is NOT virgin and must migrate.
async fn store_is_virgin(db: &surrealdb::Surreal<Conn>) -> Result<bool, DbError> {
    let mut res = db
        .query("SELECT count() AS n FROM file GROUP ALL; SELECT count() AS n FROM folder GROUP ALL")
        .await?;
    let files: Vec<serde_json::Value> = res.take(0)?;
    let folders: Vec<serde_json::Value> = res.take(1)?;
    let n = |rows: &[serde_json::Value]| {
        rows.first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    Ok(n(&files) + n(&folders) == 0)
}

/// Runs every migration above the stored version, in order, recording each
/// one as it lands. Append a new `v == N` arm (and bump SCHEMA_LATEST)
/// to add a migration; never renumber or edit shipped steps.
///
/// A brand-new store (no schema record and no rows at all) is stamped at
/// the latest version directly — migrations only exist for old data.
pub async fn migrate(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    if schema_version_recorded(db).await?.is_none() && store_is_virgin(db).await? {
        set_schema_version(db, SCHEMA_LATEST).await?;
        tracing::info!("fresh database initialized at schema v{SCHEMA_LATEST}");
        return Ok(SCHEMA_LATEST);
    }
    let mut v = schema_version(db).await?;
    while v < SCHEMA_LATEST {
        v += 1;
        match v {
            2 => {
                // folder becomes non-optional: drop rows from before the
                // feature. Their Telegram messages are NOT deleted.
                let dropped = purge_legacy_rows(db).await?;
                if dropped > 0 {
                    tracing::warn!(
                        "migration v2: dropped {dropped} pre-folder file rows \
                         (messages stay in Telegram)"
                    );
                }
            }
            3 => {
                // files become private by default: every row without an
                // explicit visibility is locked down.
                let mut res = db
                    .query("UPDATE file SET public = false WHERE public IS NONE RETURN AFTER")
                    .await?;
                let updated: Vec<serde_json::Value> = res.take(0)?;
                if !updated.is_empty() {
                    tracing::warn!(
                        "migration v3: {} files defaulted to private",
                        updated.len()
                    );
                }
            }
            other => return Err(DbError::Shape(format!("no migration to version {other}"))),
        }
        set_schema_version(db, v).await?;
        tracing::info!("database migrated to schema v{v}");
    }
    Ok(v)
}

/// Deletes rows written before the folder feature existed (no `folder`
/// field). Their Telegram messages are NOT deleted, so the files remain
/// in the storage channel even though the drive forgets them.
pub async fn purge_legacy_rows(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    let mut res = db
        .query("DELETE FROM file WHERE folder IS NONE RETURN BEFORE")
        .await?;
    let deleted: Vec<serde_json::Value> = res.take(0)?;
    Ok(deleted.len() as u64)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePart {
    pub message_id: i32,
    /// Storage chat key holding this part's message.
    pub chat: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    pub uid: String,
    pub name: String,
    pub mime: String,
    /// Total size across all parts.
    pub size: i64,
    /// First part's message id — kept for compatibility with old rows/tools.
    pub message_id: i32,
    /// First part's storage chat key.
    pub chat: String,
    pub created_at: i64,
    /// Parent folder id, "" = root. Legacy rows default to root.
    #[serde(default)]
    pub folder: String,
    /// One entry per uploaded message; single-part files have exactly one.
    /// Legacy rows without `parts_json` synthesize one part from the columns.
    #[serde(default)]
    pub parts: Vec<FilePart>,
    /// Private by default: the raw endpoint requires a session token
    /// (header or ?token=) unless the user marks the file public.
    #[serde(default)]
    pub public: bool,
    /// Telegram stripped thumbnail, base64 JPEG; None for non-images.
    #[serde(default)]
    pub thumb: Option<String>,
}

const TABLE: &str = "file";

const ROW_COLS: &str =
    "uid, name, mime, size, message_id, chat, created_at, parts_json, folder, public, thumb";

/// Deserializes a `String` that may arrive as JSON null (SurrealDB projects
/// unset fields as null) into "" instead of failing.
fn null_as_empty<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Deserializes a `bool` that may arrive as JSON null into false.
fn null_as_false<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

/// Query results are taken as raw `serde_json::Value` (which implements
/// `SurrealValue`) and converted with serde; no custom trait impls needed.
fn to_row(v: serde_json::Value) -> Result<FileRow, DbError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        uid: String,
        name: String,
        mime: String,
        size: i64,
        message_id: i32,
        chat: String,
        created_at: i64,
        #[serde(default)]
        parts_json: Option<String>,
        // Older rows have no folder at all; SurrealDB may also project an
        // unset field as null, so map both to "" (root).
        #[serde(default, deserialize_with = "null_as_empty")]
        folder: String,
        #[serde(default, deserialize_with = "null_as_false")]
        public: bool,
        #[serde(default)]
        thumb: Option<String>,
    }
    let raw: Raw = serde_json::from_value(v)
        .map_err(|e| DbError::Shape(format!("file row shape mismatch: {e}")))?;
    let parts = match &raw.parts_json {
        Some(s) => serde_json::from_str(s)
            .map_err(|e| DbError::Shape(format!("parts shape mismatch: {e}")))?,
        // Pre-split rows: the whole file is one message.
        None => vec![FilePart {
            message_id: raw.message_id,
            chat: raw.chat.clone(),
            size: raw.size,
        }],
    };
    Ok(FileRow {
        uid: raw.uid,
        name: raw.name,
        mime: raw.mime,
        size: raw.size,
        message_id: raw.message_id,
        chat: raw.chat,
        created_at: raw.created_at,
        folder: raw.folder,
        parts,
        public: raw.public,
        thumb: raw.thumb,
    })
}

pub async fn insert(db: &surrealdb::Surreal<Conn>, row: &FileRow) -> Result<(), DbError> {
    let parts_json = serde_json::to_string(&row.parts)
        .map_err(|e| DbError::Shape(format!("parts serialize: {e}")))?;
    let _: Option<serde_json::Value> = db
        .create(TABLE)
        .content(serde_json::json!({
            "uid": row.uid,
            "name": row.name,
            "mime": row.mime,
            "size": row.size,
            "message_id": row.message_id,
            "chat": row.chat,
            "created_at": row.created_at,
            "parts_json": parts_json,
            "folder": row.folder,
            "public": row.public,
            "thumb": row.thumb,
        }))
        .await?;
    Ok(())
}

pub async fn get(db: &surrealdb::Surreal<Conn>, uid: &str) -> Result<Option<FileRow>, DbError> {
    let mut res = db
        .query(format!("SELECT {ROW_COLS} FROM file WHERE uid = $uid LIMIT 1"))
        .bind(("uid", uid.to_string()))
        .await?;
    let mut rows: Vec<serde_json::Value> = res.take(0)?;
    match rows.len() {
        0 => Ok(None),
        _ => Ok(Some(to_row(rows.swap_remove(0))?)),
    }
}

pub async fn list(
    db: &surrealdb::Surreal<Conn>,
    q: &str,
    folder: &str,
    limit: u64,
    offset: u64,
) -> Result<Vec<FileRow>, DbError> {
    let mut res = db
        .query(format!(
            // CONTAINS "" is true for every name, so one query serves both cases.
            "SELECT {ROW_COLS} FROM file \
             WHERE string::lowercase(name) CONTAINS $q AND folder = $folder \
             ORDER BY created_at DESC \
             LIMIT $limit START $offset"
        ))
        .bind(("q", q.to_lowercase()))
        .bind(("folder", folder.to_string()))
        .bind(("limit", limit.min(500) as i64))
        .bind(("offset", offset as i64))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    rows.into_iter().map(to_row).collect()
}

/// Moves a file to another folder ("" = root); false when the uid does
/// not exist.
pub async fn set_folder(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
    folder: &str,
) -> Result<bool, DbError> {
    let mut res = db
        .query("UPDATE file SET folder = $f WHERE uid = $uid RETURN AFTER")
        .bind(("uid", uid.to_string()))
        .bind(("f", folder.to_string()))
        .await?;
    let updated: Vec<serde_json::Value> = res.take(0)?;
    Ok(!updated.is_empty())
}

/// Stores a thumbnail after the fact (video first-frame extraction runs
/// in the background). false when the uid does not exist.
pub async fn set_thumb(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
    thumb_b64: &str,
) -> Result<bool, DbError> {
    let mut res = db
        .query("UPDATE file SET thumb = $t WHERE uid = $uid RETURN AFTER")
        .bind(("uid", uid.to_string()))
        .bind(("t", thumb_b64.to_string()))
        .await?;
    let updated: Vec<serde_json::Value> = res.take(0)?;
    Ok(!updated.is_empty())
}

/// Flips a file's visibility; false when the uid does not exist.
pub async fn set_public(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
    public: bool,
) -> Result<bool, DbError> {
    let mut res = db
        .query("UPDATE file SET public = $p WHERE uid = $uid RETURN AFTER")
        .bind(("uid", uid.to_string()))
        .bind(("p", public))
        .await?;
    let updated: Vec<serde_json::Value> = res.take(0)?;
    Ok(!updated.is_empty())
}

pub async fn delete(db: &surrealdb::Surreal<Conn>, uid: &str) -> Result<u64, DbError> {
    let mut res = db
        .query("DELETE FROM file WHERE uid = $uid RETURN BEFORE")
        .bind(("uid", uid.to_string()))
        .await?;
    let deleted: Vec<serde_json::Value> = res.take(0)?;
    Ok(deleted.len() as u64)
}

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

/// A user-created directory; `parent` is a folder id, "" = root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRow {
    pub uid: String,
    pub name: String,
    pub parent: String,
}

pub async fn list_folders(db: &surrealdb::Surreal<Conn>) -> Result<Vec<FolderRow>, DbError> {
    let mut res = db
        .query("SELECT uid, name, parent FROM folder ORDER BY name")
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    rows.into_iter()
        .map(|v| {
            serde_json::from_value(v)
                .map_err(|e| DbError::Shape(format!("folder row shape mismatch: {e}")))
        })
        .collect()
}

pub async fn get_folder(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
) -> Result<Option<FolderRow>, DbError> {
    let mut res = db
        .query("SELECT uid, name, parent FROM folder WHERE uid = $uid LIMIT 1")
        .bind(("uid", uid.to_string()))
        .await?;
    let mut rows: Vec<serde_json::Value> = res.take(0)?;
    match rows.len() {
        0 => Ok(None),
        _ => serde_json::from_value(rows.swap_remove(0))
            .map(Some)
            .map_err(|e| DbError::Shape(format!("folder row shape mismatch: {e}"))),
    }
}

pub async fn create_folder(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
    name: &str,
    parent: &str,
) -> Result<(), DbError> {
    let _: Option<serde_json::Value> = db
        .create("folder")
        .content(serde_json::json!({
            "uid": uid,
            "name": name,
            "parent": parent,
        }))
        .await?;
    Ok(())
}

/// True when the folder still holds files or subfolders.
pub async fn folder_is_empty(
    db: &surrealdb::Surreal<Conn>,
    uid: &str,
) -> Result<bool, DbError> {
    // count() yields a {count: 0} row even for empty sets, so read the
    // numbers instead of checking for absent rows.
    let mut res = db
        .query(
            "SELECT count() AS n FROM file WHERE folder = $uid GROUP ALL; \
             SELECT count() AS n FROM folder WHERE parent = $uid GROUP ALL",
        )
        .bind(("uid", uid.to_string()))
        .await?;
    let files: Vec<serde_json::Value> = res.take(0)?;
    let subs: Vec<serde_json::Value> = res.take(1)?;
    let total = |rows: &[serde_json::Value]| {
        rows.first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    Ok(total(&files) + total(&subs) == 0)
}

pub async fn delete_folder(db: &surrealdb::Surreal<Conn>, uid: &str) -> Result<u64, DbError> {
    let mut res = db
        .query("DELETE FROM folder WHERE uid = $uid RETURN BEFORE")
        .bind(("uid", uid.to_string()))
        .await?;
    let deleted: Vec<serde_json::Value> = res.take(0)?;
    Ok(deleted.len() as u64)
}

/// (files, folders) row counts for the startup log.
pub async fn counts(db: &surrealdb::Surreal<Conn>) -> Result<(u64, u64), DbError> {
    let mut res = db
        .query(
            "SELECT count() AS n FROM file GROUP ALL;              SELECT count() AS n FROM folder GROUP ALL",
        )
        .await?;
    let files: Vec<serde_json::Value> = res.take(0)?;
    let folders: Vec<serde_json::Value> = res.take(1)?;
    let n = |rows: &[serde_json::Value]| {
        rows.first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    Ok((n(&files), n(&folders)))
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_db() -> (surrealdb::Surreal<Conn>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.surrealkv");
        let db = open(path.to_str().expect("utf8 path"))
            .await
            .expect("open test db");
        (db, dir)
    }

    pub(super) fn row(uid: &str, name: &str, created_at: i64) -> FileRow {
        FileRow {
            uid: uid.to_string(),
            name: name.to_string(),
            mime: "application/octet-stream".to_string(),
            size: 42,
            message_id: 7,
            chat: "me".to_string(),
            created_at,
            folder: String::new(),
            parts: vec![FilePart {
                message_id: 7,
                chat: "me".to_string(),
                size: 42,
            }],
            public: false,
            thumb: None,
        }
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let (db, _dir) = temp_db().await;

        insert(&db, &row("01A", "hello.txt", 100)).await.unwrap();
        insert(&db, &row("01B", "world.bin", 200)).await.unwrap();

        let got = get(&db, "01A").await.unwrap().expect("row exists");
        assert_eq!(got.name, "hello.txt");
        assert_eq!(got.size, 42);

        assert!(get(&db, "missing").await.unwrap().is_none());

        let all = list(&db, "", "", 100, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].uid, "01B", "newest first");

        let hits = list(&db, "hello", "", 100, 0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].uid, "01A");

        let paged = list(&db, "", "", 1, 1).await.unwrap();
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].uid, "01A");

        assert_eq!(delete(&db, "01A").await.unwrap(), 1);
        assert_eq!(delete(&db, "01A").await.unwrap(), 0);
        assert!(get(&db, "01A").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn channel_selection_roundtrip() {
        let (db, _dir) = temp_db().await;

        assert!(get_channels(&db, "12345").await.unwrap().is_empty());

        let sel = vec![
            ChannelSel { chat: "@mychannel".into(), title: "My Channel".into() },
            ChannelSel { chat: "-1001234567890".into(), title: "Archive".into() },
        ];
        set_channels(&db, "12345", sel.clone()).await.unwrap();
        let got = get_channels(&db, "12345").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].chat, "@mychannel");

        // Per-user isolation.
        assert!(get_channels(&db, "99999").await.unwrap().is_empty());

        set_channels(&db, "12345", vec![]).await.unwrap();
        assert!(get_channels(&db, "12345").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn split_setting_roundtrip() {
        let (db, _dir) = temp_db().await;
        assert_eq!(get_split(&db).await.unwrap(), 0, "default is off");

        set_split(&db, 250 * 1024 * 1024).await.unwrap();
        assert_eq!(get_split(&db).await.unwrap(), 250 * 1024 * 1024);

        set_split(&db, 0).await.unwrap();
        assert_eq!(get_split(&db).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn multi_part_row_and_legacy_compat() {
        let (db, _dir) = temp_db().await;

        // A split file: two parts across two chats.
        let mut row = row("01S", "big.bin", 300);
        row.size = 300;
        row.message_id = 11;
        row.chat = "-1001".into();
        row.parts = vec![
            FilePart { message_id: 11, chat: "-1001".into(), size: 150 },
            FilePart { message_id: 12, chat: "-1002".into(), size: 150 },
        ];
        insert(&db, &row).await.unwrap();
        let got = get(&db, "01S").await.unwrap().unwrap();
        assert_eq!(got.parts.len(), 2);
        assert_eq!(got.parts[1].chat, "-1002");
        assert_eq!(got.message_id, 11);

        // A pre-split row (no parts_json) reads back as a single part.
        db.query(
            "CREATE file SET uid = 'old', name = 'legacy', mime = 'm', size = 5, \
             message_id = 9, chat = 'me', created_at = 1",
        )
        .await
        .unwrap();
        let old = get(&db, "old").await.unwrap().unwrap();
        assert_eq!(old.parts.len(), 1);
        assert_eq!(old.parts[0].message_id, 9);
        assert_eq!(old.parts[0].size, 5);
        // Legacy rows also land in the root folder.
        assert_eq!(old.folder, "");
    }

    #[tokio::test]
    async fn folders_crud_and_file_filtering() {
        let (db, _dir) = temp_db().await;

        create_folder(&db, "F1", "Docs", "").await.unwrap();
        create_folder(&db, "F2", "Invoices", "F1").await.unwrap();
        assert!(get_folder(&db, "F2").await.unwrap().unwrap().parent == "F1");

        let mut r = row("01F", "tax.pdf", 10);
        r.folder = "F1".into();
        insert(&db, &r).await.unwrap();
        insert(&db, &row("01R", "root.txt", 20)).await.unwrap();

        let in_f1 = list(&db, "", "F1", 100, 0).await.unwrap();
        assert_eq!(in_f1.len(), 1);
        assert_eq!(in_f1[0].uid, "01F");
        let in_root = list(&db, "", "", 100, 0).await.unwrap();
        assert_eq!(in_root.len(), 1);
        assert_eq!(in_root[0].uid, "01R");

        // Non-empty folders refuse deletion; empty ones go through.
        assert!(!folder_is_empty(&db, "F1").await.unwrap());
        assert!(folder_is_empty(&db, "F2").await.unwrap());
        assert_eq!(delete_folder(&db, "F2").await.unwrap(), 1);
        assert_eq!(delete_folder(&db, "F2").await.unwrap(), 0);

        let names: Vec<String> = list_folders(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["Docs"]);
    }

    #[tokio::test]
    async fn legacy_rows_without_folder_are_purged() {
        let (db, _dir) = temp_db().await;

        // A row written before the folder feature existed: no folder field.
        db.query(
            "CREATE file SET uid = 'leg', name = 'legacy.bin', mime = 'm', size = 9, \
             message_id = 3, chat = 'me', created_at = 1",
        )
        .await
        .unwrap();
        insert(&db, &row("01N", "new.bin", 20)).await.unwrap();

        // The startup purge drops only rows without a folder.
        assert_eq!(purge_legacy_rows(&db).await.unwrap(), 1);
        let root = list(&db, "", "", 100, 0).await.unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].uid, "01N");
        assert!(get(&db, "leg").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn visibility_roundtrip() {
        let (db, _dir) = temp_db().await;
        insert(&db, &row("01V", "secret.bin", 1)).await.unwrap();

        // New uploads are private by default.
        assert!(!get(&db, "01V").await.unwrap().unwrap().public);

        assert!(set_public(&db, "01V", true).await.unwrap());
        assert!(get(&db, "01V").await.unwrap().unwrap().public);

        assert!(set_public(&db, "01V", false).await.unwrap());
        assert!(!get(&db, "01V").await.unwrap().unwrap().public);

        assert!(!set_public(&db, "missing", true).await.unwrap());
    }

    #[tokio::test]
    async fn fresh_store_stamps_latest_directly() {
        let (db, _dir) = temp_db().await;
        // temp_db already ran open(); a virgin store must land on the
        // latest version without any migration noise.
        assert_eq!(schema_version(&db).await.unwrap(), SCHEMA_LATEST);
        assert_eq!(migrate(&db).await.unwrap(), SCHEMA_LATEST);
    }

    #[tokio::test]
    async fn migrations_run_once_and_record_version() {
        let (db, _dir) = temp_db().await;
        // temp_db opened (and migrated) at the latest version.
        assert_eq!(schema_version(&db).await.unwrap(), SCHEMA_LATEST);

        // Rewind to v1 with a legacy row, as an old database would look.
        set_schema_version(&db, 1).await.unwrap();
        db.query(
            "CREATE file SET uid = 'leg', name = 'legacy.bin', mime = 'm', size = 9, \
             message_id = 3, chat = 'me', created_at = 1",
        )
        .await
        .unwrap();

        assert_eq!(migrate(&db).await.unwrap(), SCHEMA_LATEST);
        assert_eq!(schema_version(&db).await.unwrap(), SCHEMA_LATEST);
        assert!(get(&db, "leg").await.unwrap().is_none(), "v2 purged it");

        // A second run is a no-op.
        assert_eq!(migrate(&db).await.unwrap(), SCHEMA_LATEST);
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;
    use crate::db::tests::row;

    #[tokio::test]
    async fn rows_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.surrealkv");
        let path_str = path.to_str().expect("utf8").to_string();

        {
            let db = open(&path_str).await.expect("open");
            insert(&db, &row("01P", "persist.bin", 5)).await.expect("insert");
        }
        // Dropping Surreal closes the store asynchronously; give the
        // background tasks time to release the file lock.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // SurrealDB flushes on drop/close; simulate a server restart.
        let db = open(&path_str).await.expect("reopen");
        let got = get(&db, "01P").await.expect("get");
        assert!(got.is_some(), "row must survive reopen");
    }
}
