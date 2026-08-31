//! Telegram session rows.
//!
//! `MTProto` session state (mtprsto's [`mtprsto::session::SessionData`])
//! is persisted here as an opaque JSON blob instead of the per-account
//! files the previous storage kept. Sessions live and die with the
//! embedded store, so claiming, deleting and restoring accounts are
//! plain row operations — no files to move, no write-ahead logs to
//! pair, nothing held open that Windows would refuse to rename.

use super::{Conn, DbError};
use surrealdb::types::{RecordId, RecordIdKey};

/// What a session row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A signed-in account; the row key carries the Telegram user id.
    Account,
    /// A download bot owned by one account.
    Bot,
    /// A login in flight; a successful sign-in turns the row into an
    /// account row, and any row still of this kind at boot belongs to a
    /// login interrupted by a restart and is deleted.
    Pending,
}

impl SessionKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Bot => "bot",
            Self::Pending => "pending",
        }
    }
}

/// Creates or overwrites a session blob.
pub async fn write_session(
    db: &surrealdb::Surreal<Conn>,
    key: &str,
    kind: SessionKind,
    owner: i64,
    data: &str,
) -> Result<(), DbError> {
    let payload = serde_json::json!({
        "kind": kind.as_str(),
        "owner": owner,
        "data": data,
    });
    let _: Option<serde_json::Value> =
        db.upsert(("tg_session", key)).content(payload).await?;
    Ok(())
}

/// Reads a session blob. Returns `Ok(None)` when nothing is stored.
pub async fn read_session(
    db: &surrealdb::Surreal<Conn>,
    key: &str,
) -> Result<Option<String>, DbError> {
    let mut res = db
        .query("SELECT data FROM type::record(\"tg_session\", $key)")
        .bind(("key", key.to_string()))
        .await?;
    let rows: Vec<serde_json::Value> = res.take(0usize)?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|row| row.get("data").and_then(|d| d.as_str().map(String::from))))
}

/// Deletes a session blob. Deleting an absent row is fine.
pub async fn delete_session(db: &surrealdb::Surreal<Conn>, key: &str) -> Result<(), DbError> {
    let _: Option<serde_json::Value> = db.delete(("tg_session", key)).await?;
    Ok(())
}

/// Row keys of every session of `kind`, sorted for deterministic logs.
pub async fn list_keys(
    db: &surrealdb::Surreal<Conn>,
    kind: SessionKind,
) -> Result<Vec<String>, DbError> {
    let mut res = db
        .query("SELECT value id FROM tg_session WHERE kind = $kind ORDER BY id")
        .bind(("kind", kind.as_str()))
        .await?;
    let ids: Vec<RecordId> = res.take(0usize)?;
    Ok(ids
        .into_iter()
        .filter_map(|id| match id.key {
            RecordIdKey::String(s) => Some(s),
            _ => None,
        })
        .collect())
}

/// Deletes every session of `kind` owned by `owner`, returning the count.
pub async fn delete_sessions_of(
    db: &surrealdb::Surreal<Conn>,
    kind: SessionKind,
    owner: i64,
) -> Result<usize, DbError> {
    let mut res = db
        .query("DELETE tg_session WHERE kind = $kind AND owner = $owner RETURN BEFORE")
        .bind(("kind", kind.as_str()))
        .bind(("owner", owner))
        .await?;
    let removed: Vec<serde_json::Value> = res.take(0usize)?;
    Ok(removed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> surrealdb::Surreal<Conn> {
        let db = surrealdb::Surreal::init();
        super::super::connect_mem(&db).await.expect("scratch store");
        db
    }

    /// A blob round-trips verbatim, whatever the library serialized into
    /// it, and reads of absent rows stay `None`.
    #[tokio::test]
    async fn blobs_round_trip_by_kind_and_owner() {
        let db = db().await;
        write_session(&db, "user-7", SessionKind::Account, 7, "{\"a\":1}")
            .await
            .unwrap();
        write_session(&db, "user-7-bot-9", SessionKind::Bot, 7, "{\"b\":2}")
            .await
            .unwrap();

        assert_eq!(
            read_session(&db, "user-7").await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(read_session(&db, "nope").await.unwrap(), None);

        assert_eq!(
            list_keys(&db, SessionKind::Account).await.unwrap(),
            vec!["user-7".to_string()]
        );
        assert_eq!(
            list_keys(&db, SessionKind::Bot).await.unwrap(),
            vec!["user-7-bot-9".to_string()]
        );

        // Deleting by owner+kind touches only the bot row.
        assert_eq!(
            delete_sessions_of(&db, SessionKind::Bot, 7).await.unwrap(),
            1
        );
        assert_eq!(
            delete_sessions_of(&db, SessionKind::Bot, 7).await.unwrap(),
            0
        );
        assert!(read_session(&db, "user-7").await.unwrap().is_some());

        delete_session(&db, "user-7").await.unwrap();
        assert!(read_session(&db, "user-7").await.unwrap().is_none());
        delete_session(&db, "user-7").await.unwrap(); // idempotent
    }

    /// A claimed row overwrites the pending one in place: same key, new
    /// kind — the pending listing must not resurrect it.
    #[tokio::test]
    async fn claiming_rewrites_kind_in_place() {
        let db = db().await;
        write_session(&db, "pending-abc", SessionKind::Pending, 0, "x")
            .await
            .unwrap();
        write_session(&db, "pending-abc", SessionKind::Account, 77, "y")
            .await
            .unwrap();

        assert_eq!(list_keys(&db, SessionKind::Pending).await.unwrap(), Vec::<String>::new());
        assert_eq!(
            list_keys(&db, SessionKind::Account).await.unwrap(),
            vec!["pending-abc".to_string()]
        );
        assert_eq!(
            read_session(&db, "pending-abc").await.unwrap().as_deref(),
            Some("y")
        );
    }
}
