mod bots;
mod files;
mod folders;
mod schema;
mod settings;

pub use bots::{BotInfo, get_bots, remove_bot, set_bots};
pub use files::{
    FilePart, FileRow, counts, delete, get, insert, list, set_folder, set_public, set_thumb,
};
pub use folders::{
    FolderRow, create_folder, delete_folder, folder_is_empty, get_folder, list_folders,
};
// Glob so the version plumbing (`pub(super)`) stays reachable from the
// tests below without a second, test-only import list.
pub use schema::*;
pub use settings::{
    BotDraft, ChannelSel, DraftMsg, RouteRule, bump_token_epoch, clear_bot_draft, get_bot_draft,
    get_channels, get_rules, get_split, get_token_epoch, set_bot_draft, set_channels, set_rules,
    set_split,
};

type Conn = surrealdb::engine::local::Db;

/// Owner id on rows written before multi-tenancy, and on rows a fresh
/// account has not claimed yet. Real Telegram user ids are never 0, so no
/// live account can collide with it.
pub const UNOWNED: i64 = 0;

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
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::SurrealKv>(path).await?;
    bootstrap(&db).await?;
    Ok(db)
}

/// Scratch store for tests. Same `local::Db` client as [`open`], so every
/// query behaves identically, with no filesystem to set up or clean out.
/// Measured: the suite runs in ~1.5s on this engine versus ~6.5s
/// file-backed, and a test can never see another's leftover files.
#[cfg(test)]
pub(crate) async fn open_mem() -> Result<surrealdb::Surreal<Conn>, DbError> {
    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(()).await?;
    bootstrap(&db).await?;
    Ok(db)
}

/// Selects the namespace, defines the tables and brings the schema up to
/// date. Shared so a test store is never a different shape from a real one.
async fn bootstrap(db: &surrealdb::Surreal<Conn>) -> Result<(), DbError> {
    db.use_ns("drive").await?;
    db.use_db("drive").await?;
    // SELECTs against a not-yet-created table error out; define up front so
    // a fresh install serves an empty list instead of a 500.
    let mut res = db
        .query(
            "DEFINE TABLE IF NOT EXISTS file; DEFINE TABLE IF NOT EXISTS folder; \
                DEFINE TABLE IF NOT EXISTS setting",
        )
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    let _ = res.take::<surrealdb::types::Value>(1usize)?;
    migrate(db).await?;
    Ok(())
}

/// Test-only serialization for the *file-backed* store.
///
/// Opening several SurrealKv engines at once has crashed a run here, and a
/// store keeps closing on background tasks after its test has dropped the
/// runtime, so the overlap is not visible from the test body. Measured
/// cause unknown; holding this lock keeps one file-backed engine live at a
/// time, which is cheap insurance for the two tests that need a real file.
/// Logic tests sidestep it entirely with [`open_mem`].
#[cfg(test)]
pub(crate) mod harness {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENGINE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Held for the duration of a DB-backed test.
    pub(crate) struct EngineGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

    pub(crate) fn acquire() -> EngineGuard {
        // Poisoning just means an earlier DB test panicked. This lock
        // orders engine startup, not shared data, so carry on.
        EngineGuard(ENGINE.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing to keep alive for an in-memory store. Kept so tests read
    /// the same as they did when the store was a temp directory.
    struct TestEnv;

    /// Scratch store for a single test: real schema, no filesystem.
    async fn temp_db() -> (surrealdb::Surreal<Conn>, TestEnv) {
        (open_mem().await.expect("open test db"), TestEnv)
    }

    /// Tenant used by tests that do not care about ownership.
    pub(super) const OWNER: i64 = 4242;

    pub(super) fn row(uid: &str, name: &str, created_at: i64) -> FileRow {
        FileRow {
            owner: OWNER,
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

        let all = list(&db, OWNER, "", "", 100, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].uid, "01B", "newest first");

        let hits = list(&db, OWNER, "hello", "", 100, 0).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].uid, "01A");

        let paged = list(&db, OWNER, "", "", 1, 1).await.unwrap();
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
            ChannelSel {
                chat: "@mychannel".into(),
                title: "My Channel".into(),
            },
            ChannelSel {
                chat: "-1001234567890".into(),
                title: "Archive".into(),
            },
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
                DraftMsg {
                    who: "me".into(),
                    text: "/newbot".into(),
                },
                DraftMsg {
                    who: "bf".into(),
                    text: "Alright, a new bot. How are we going to call it?".into(),
                },
            ],
            token: String::new(),
            updated_at: 1_700_000_000,
        };
        set_bot_draft(&db, "12345", &draft).await.unwrap();

        let got = get_bot_draft(&db, "12345")
            .await
            .unwrap()
            .expect("draft exists");
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
            get_bot_draft(&db, "12345")
                .await
                .unwrap()
                .expect("draft")
                .token,
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
                RouteRule {
                    mime: "image/".into(),
                    folder: "F1".into(),
                },
                RouteRule {
                    mime: "application/pdf".into(),
                    folder: "F2".into(),
                },
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
        assert_eq!(get_split(&db, "111").await.unwrap(), 0, "default is off");

        set_split(&db, "111", 250 * 1024 * 1024).await.unwrap();
        assert_eq!(get_split(&db, "111").await.unwrap(), 250 * 1024 * 1024);

        // Per-user isolation: another account keeps the default.
        assert_eq!(get_split(&db, "222").await.unwrap(), 0);

        set_split(&db, "111", 0).await.unwrap();
        assert_eq!(get_split(&db, "111").await.unwrap(), 0);
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
            FilePart {
                message_id: 11,
                chat: "-1001".into(),
                size: 150,
            },
            FilePart {
                message_id: 12,
                chat: "-1002".into(),
                size: 150,
            },
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

        create_folder(&db, OWNER, "F1", "Docs", "").await.unwrap();
        create_folder(&db, OWNER, "F2", "Invoices", "F1")
            .await
            .unwrap();
        assert!(get_folder(&db, "F2").await.unwrap().unwrap().parent == "F1");

        let mut r = row("01F", "tax.pdf", 10);
        r.folder = "F1".into();
        insert(&db, &r).await.unwrap();
        insert(&db, &row("01R", "root.txt", 20)).await.unwrap();

        let in_f1 = list(&db, OWNER, "", "F1", 100, 0).await.unwrap();
        assert_eq!(in_f1.len(), 1);
        assert_eq!(in_f1[0].uid, "01F");
        let in_root = list(&db, OWNER, "", "", 100, 0).await.unwrap();
        assert_eq!(in_root.len(), 1);
        assert_eq!(in_root[0].uid, "01R");

        // Non-empty folders refuse deletion; empty ones go through.
        assert!(!folder_is_empty(&db, "F1").await.unwrap());
        assert!(folder_is_empty(&db, "F2").await.unwrap());
        assert_eq!(delete_folder(&db, "F2").await.unwrap(), 1);
        assert_eq!(delete_folder(&db, "F2").await.unwrap(), 0);

        let names: Vec<String> = list_folders(&db, OWNER)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["Docs"]);
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
        assert_eq!(migrate(&db).await.unwrap(), SCHEMA_LATEST);
        assert_eq!(migrate(&db).await.unwrap(), SCHEMA_LATEST);
    }

    #[tokio::test]
    async fn owners_cannot_see_each_others_rows() {
        let (db, _dir) = temp_db().await;
        const A: i64 = 111;
        const B: i64 = 222;

        let mut a = row("01A", "a.txt", 10);
        a.owner = A;
        insert(&db, &a).await.unwrap();
        let mut b = row("01B", "b.txt", 20);
        b.owner = B;
        insert(&db, &b).await.unwrap();
        create_folder(&db, A, "FA", "Alice", "").await.unwrap();
        create_folder(&db, B, "FB", "Bob", "").await.unwrap();

        let mine = list(&db, A, "", "", 100, 0).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].uid, "01A");
        assert_eq!(mine[0].owner, A);
        // A search matching the other tenant's file still finds nothing.
        assert!(list(&db, A, "b.txt", "", 100, 0).await.unwrap().is_empty());

        let theirs = list(&db, B, "", "", 100, 0).await.unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].uid, "01B");

        let a_folders = list_folders(&db, A).await.unwrap();
        assert_eq!(a_folders.len(), 1);
        assert_eq!(a_folders[0].name, "Alice");
        assert_eq!(a_folders[0].owner, A);
        let b_folders = list_folders(&db, B).await.unwrap();
        assert_eq!(b_folders.len(), 1);
        assert_eq!(b_folders[0].name, "Bob");

        // uid-addressed reads stay global; callers authorize on `owner`.
        assert_eq!(get(&db, "01B").await.unwrap().unwrap().owner, B);
        assert_eq!(get_folder(&db, "FB").await.unwrap().unwrap().owner, B);
    }

    #[tokio::test]
    async fn bot_pools_are_per_owner() {
        let (db, _dir) = temp_db().await;
        const A: i64 = 111;
        const B: i64 = 222;
        assert!(get_bots(&db, A).await.unwrap().is_empty());

        let bot = |id: i64| BotInfo {
            token: format!("{id}:tok"),
            username: format!("b{id}"),
            id,
        };
        set_bots(&db, A, &[bot(1), bot(2)]).await.unwrap();
        set_bots(&db, B, &[bot(9)]).await.unwrap();

        assert_eq!(get_bots(&db, A).await.unwrap().len(), 2);
        let b_pool = get_bots(&db, B).await.unwrap();
        assert_eq!(b_pool.len(), 1);
        assert_eq!(b_pool[0].id, 9);

        // Removing from one pool leaves the other untouched.
        remove_bot(&db, A, 1).await.unwrap();
        let a_pool = get_bots(&db, A).await.unwrap();
        assert_eq!(a_pool.len(), 1);
        assert_eq!(a_pool[0].id, 2);
        assert_eq!(get_bots(&db, B).await.unwrap().len(), 1);
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
            insert(&db, &row("01P", "persist.bin", 5))
                .await
                .expect("insert");
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
