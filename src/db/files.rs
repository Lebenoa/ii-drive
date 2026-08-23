use serde::{Deserialize, Serialize};

use super::{Conn, DbError};

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
