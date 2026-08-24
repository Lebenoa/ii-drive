use serde::{Deserialize, Serialize};

use super::{Conn, DbError};

/// A user-created directory; `parent` is a folder id, "" = root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRow {
    /// Owning Telegram user id. `UNOWNED` is the serde fallback for rows
    /// that arrive without one — invisible to every tenant filter.
    #[serde(default, deserialize_with = "super::files::null_as_zero")]
    pub owner: i64,
    pub uid: String,
    pub name: String,
    pub parent: String,
}

pub async fn list_folders(
    db: &surrealdb::Surreal<Conn>,
    owner: i64,
) -> Result<Vec<FolderRow>, DbError> {
    let mut res = db
        .query("SELECT uid, name, parent, owner FROM folder WHERE owner = $owner ORDER BY name")
        .bind(("owner", owner))
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
        .query("SELECT uid, name, parent, owner FROM folder WHERE uid = $uid LIMIT 1")
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
    owner: i64,
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
            "owner": owner,
        }))
        .await?;
    Ok(())
}

/// True when the folder still holds files or subfolders.
pub async fn folder_is_empty(db: &surrealdb::Surreal<Conn>, uid: &str) -> Result<bool, DbError> {
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
