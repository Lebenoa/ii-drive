mod bots;
mod files;
mod folders;
mod schema;
mod settings;

pub use bots::{get_bots, remove_bot, set_bots, BotInfo};
pub use files::{
    counts, delete, get, insert, list, set_folder, set_public, set_thumb, FilePart, FileRow,
};
pub use folders::{
    create_folder, delete_folder, folder_is_empty, get_folder, list_folders, FolderRow,
};
// Glob so the version plumbing (`pub(super)`) stays reachable from the
// tests below without a second, test-only import list.
pub use schema::*;
pub use settings::{
    clear_bot_draft, get_bot_draft, get_channels, get_rules, get_split, set_bot_draft,
    set_channels, set_rules, set_split, BotDraft, ChannelSel, DraftMsg, RouteRule,
};

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

/// Test-only serialization for the embedded store.
///
/// Every `open()` builds a SurrealKv engine that commits roughly a
/// gigabyte up front and releases it only once its store finishes closing
/// on background tasks. `cargo test`'s default parallelism happily opens
/// one per thread, so a dozen live arenas exhaust the machine and the
/// process dies on a 1 GiB allocation — regardless of `--test-threads`,
/// because each `#[tokio::test]` drops its own runtime before that close
/// completes. Holding this lock keeps one engine live at a time.
#[cfg(test)]
pub(crate) mod harness {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENGINE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Held for the duration of a DB-backed test.
    pub(crate) struct EngineGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    pub(crate) fn acquire() -> EngineGuard {
        // Poisoning just means an earlier DB test panicked. This lock
        // guards memory headroom, not shared data, so carry on.
        let held = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        // The previous test released this lock as it unwound; wait for its
        // engine to hand the arena back before opening the next one.
        std::thread::sleep(std::time::Duration::from_millis(250));
        EngineGuard(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeps one test's scratch directory and its engine slot alive.
    struct TestEnv {
        _dir: tempfile::TempDir,
        _engine: super::harness::EngineGuard,
    }

    /// Scratch store for a single test. Hold the returned guard for the
    /// whole test: dropping it frees the engine slot for the next one.
    async fn temp_db() -> (surrealdb::Surreal<Conn>, TestEnv) {
        let engine = super::harness::acquire();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.surrealkv");
        let db = open(path.to_str().expect("utf8 path"))
            .await
            .expect("open test db");
        (db, TestEnv { _dir: dir, _engine: engine })
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
    async fn bot_draft_roundtrip() {
        let (db, _dir) = temp_db().await;

        // Idle wizard: nothing stored, so nothing to resume.
        assert!(get_bot_draft(&db, "12345").await.unwrap().is_none());

        let draft = BotDraft {
            log: vec![
                DraftMsg { who: "me".into(), text: "/newbot".into() },
                DraftMsg { who: "bf".into(), text: "Alright, a new bot. How are we going to call it?".into() },
            ],
            token: String::new(),
            updated_at: 1_700_000_000,
        };
        set_bot_draft(&db, "12345", &draft).await.unwrap();

        let got = get_bot_draft(&db, "12345").await.unwrap().expect("draft exists");
        assert_eq!(got.log.len(), 2);
        assert_eq!(got.log[1].who, "bf");
        assert!(got.token.is_empty(), "no token issued yet");
        assert_eq!(got.updated_at, 1_700_000_000);

        // Per-user isolation: another account has its own wizard.
        assert!(get_bot_draft(&db, "99999").await.unwrap().is_none());

        // A token survives the round-trip, so an interrupted wizard can
        // still hand the created bot to the pool.
        let issued = BotDraft {
            token: "123456:AAtoken".into(),
            ..draft
        };
        set_bot_draft(&db, "12345", &issued).await.unwrap();
        assert_eq!(
            get_bot_draft(&db, "12345").await.unwrap().expect("draft").token,
            "123456:AAtoken"
        );

        clear_bot_draft(&db, "12345").await.unwrap();
        assert!(get_bot_draft(&db, "12345").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn routing_rules_roundtrip() {
        let (db, _dir) = temp_db().await;
        assert!(get_rules(&db, "111").await.unwrap().is_empty());

        set_rules(
            &db,
            "111",
            &[
                RouteRule { mime: "image/".into(), folder: "F1".into() },
                RouteRule { mime: "application/pdf".into(), folder: "F2".into() },
            ],
        )
        .await
        .unwrap();
        let got = get_rules(&db, "111").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].mime, "image/");
        assert_eq!(got[1].folder, "F2");

        // Per-user isolation.
        assert!(get_rules(&db, "222").await.unwrap().is_empty());

        set_rules(&db, "111", &[]).await.unwrap();
        assert!(get_rules(&db, "111").await.unwrap().is_empty());
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

    #[tokio::test]
    async fn public_backfill_v3() {
        let (db, _dir) = temp_db().await;
        // Pre-v3 row: no public field at all.
        set_schema_version(&db, 2).await.unwrap();
        db.query(
            "CREATE file SET uid = 'v3', name = 'x', mime = 'm', size = 1,              message_id = 1, chat = 'me', created_at = 1, folder = ''",
        )
        .await
        .unwrap();

        assert_eq!(migrate(&db).await.unwrap(), 3);
        let row = get(&db, "v3").await.unwrap().unwrap();
        assert!(!row.public, "backfilled rows must be private");
        // Idempotent second run.
        assert_eq!(migrate(&db).await.unwrap(), 3);
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;
    use crate::db::tests::row;

    #[tokio::test]
    async fn rows_survive_reopen() {
        // Same engine budget as temp_db(): this test opens two in a row.
        let _engine = super::harness::acquire();
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

