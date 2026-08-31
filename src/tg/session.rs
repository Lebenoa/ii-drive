use std::collections::HashMap;
use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::serialize::{TLReader, TLWriter};
use mtprsto::session::{SessionData, SessionStorage};
use mtprsto::types;
use tokio::sync::Mutex;

use crate::config::Config;

use super::{Conn, ROTATION, State, TgManager, TgStatus, UserInfo, friendly};

type Db = surrealdb::Surreal<surrealdb::engine::local::Db>;

/// mtprsto session persistence over the embedded store: one
/// [`SessionData`] JSON blob per row of the `tg_session` table.
///
/// mtprsto drives persistence through its synchronous
/// [`SessionStorage`] trait, while the Surreal client is async-only —
/// and the handle is bound to the runtime that connected it, so a naive
/// `block_on` on the calling thread deadlocks: under `#[tokio::test]`
/// that runtime is single-threaded and is exactly the thread being
/// parked. The bridge is the documented one instead —
/// `block_in_place` + `Handle::block_on` — which lets the runtime's
/// other workers keep the engine's tasks moving while this one waits.
/// It requires a multi-threaded runtime: what `#[tokio::main]` builds,
/// and what the tests that reach in here declare. Blobs are tiny (a few
/// hundred bytes) and written on session change, never in a hot loop.
pub(super) struct DbSessions {
    db: Db,
    key: String,
    kind: crate::db::SessionKind,
    owner: i64,
}

/// Runs one async DB call from the synchronous trait surface.
fn block_on_db<T>(
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> mtprsto::Result<T> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(fut)
            .map_err(mtprsto::error::Error::Other)
    })
}

impl DbSessions {
    pub(super) const fn new(
        db: Db,
        key: String,
        kind: crate::db::SessionKind,
        owner: i64,
    ) -> Self {
        Self { db, key, kind, owner }
    }

    /// Persists `data` as this key's session blob, keeping the row's
    /// kind and owner so saves never re-key a pending login or a bot
    /// session into an account row.
    async fn save_data(&self, data: &SessionData) -> Result<(), String> {
        let blob = serde_json::to_string(data)
            .map_err(|e| format!("cannot serialize session {}: {e}", self.key))?;
        crate::db::write_session(&self.db, &self.key, self.kind, self.owner, &blob)
            .await
            .map_err(|e| format!("cannot persist session {}: {e}", self.key))
    }
}

impl SessionStorage for DbSessions {
    fn load(&mut self) -> mtprsto::Result<Option<SessionData>> {
        let key = self.key.clone();
        block_on_db(async {
            let blob = crate::db::read_session(&self.db, &key)
                .await
                .map_err(|e| format!("cannot read session {key}: {e}"))?;
            let Some(blob) = blob else {
                return Ok(None);
            };
            serde_json::from_str(&blob)
                .map(Some)
                .map_err(|e| format!("cannot parse session {key}: {e}"))
        })
    }

    fn save(&mut self, data: &SessionData) -> mtprsto::Result<()> {
        block_on_db(self.save_data(data))
    }

    fn delete(&mut self) -> mtprsto::Result<()> {
        let key = self.key.clone();
        block_on_db(async {
            crate::db::delete_session(&self.db, &key)
                .await
                .map_err(|e| format!("cannot delete session {key}: {e}"))
        })
    }

    fn describe(&self) -> String {
        format!("embedded store row tg_session:{}", self.key)
    }
}
impl TgManager {
    /// Builds a manager for one account. `session_key` names this
    /// account's session row in the embedded store; `user_id` is
    /// [`super::UNKNOWN_USER`] while a login is still in flight and the
    /// account is therefore unknown.
    pub fn new(cfg: Config, db: Db, session_key: String, user_id: i64) -> Self {
        Self {
            cfg,
            db,
            session_key,
            user_id,
            st: Mutex::new(State {
                conn: None,
                config_error: None,
                peers: HashMap::new(),
                me: None,
                bots: HashMap::new(),
            }),
        }
    }

    /// Session row key this manager owns. Only the hub needs it, to
    /// re-key the row once a login names its account.
    pub(super) fn session_key(&self) -> &str {
        &self.session_key
    }

    /// Reads this account's persisted session from the embedded store.
    /// The login flow needs the freshly negotiated auth key of a
    /// throwaway session to drive the one-shot code exchange.
    pub(super) async fn session_data(
        &self,
        key: &str,
    ) -> Result<Option<mtprsto::session::SessionData>, String> {
        let blob = crate::db::read_session(&self.db, key)
            .await
            .map_err(|e| friendly(format!("cannot read session {key}: {e}")))?;
        let Some(blob) = blob else {
            return Ok(None);
        };
        serde_json::from_str(&blob)
            .map(Some)
            .map_err(|e| friendly(format!("cannot parse session {key}: {e}")))
    }

    /// Builds (without connecting) the client over this account's session
    /// row. Session persistence rides the embedded store, so nothing has
    /// to be opened or kept warm here — the actual `connect` happens on
    /// first use.
    pub(super) async fn open_conn(
        &self,
        session_key: &str,
        kind: crate::db::SessionKind,
    ) -> Result<Conn, String> {
        // This handle is its own session (every Surreal clone is), so it
        // needs the namespace before its first query. Idempotent, and the
        // manager connects rarely.
        crate::db::attach_session(&self.db)
            .await
            .map_err(|e| format!("cannot attach session store: {e}"))?;
        // A row mtprsto cannot parse (a half-written blob from a crash,
        // or format drift) would fail every connect forever. Drop it so
        // the account simply signs in again, like the file storage's
        // move-aside did.
        let mut storage =
            DbSessions::new(self.db.clone(), session_key.to_string(), kind, self.user_id);
        if let Err(e) = SessionStorage::load(&mut storage) {
            crate::db::delete_session(&self.db, session_key)
                .await
                .map_err(|e| format!("cannot clear unreadable session {session_key}: {e}"))?;
            tracing::warn!(
                %session_key,
                "unreadable session row dropped; sign in again ({e})"
            );
        }
        let client = Client::builder()
            .api_id(self.cfg.api_id)
            .api_hash(self.cfg.api_hash.clone())
            .session_storage(Box::new(storage))
            .build()
            .map_err(|e| format!("cannot build telegram client: {e}"))?;
        Ok(Conn {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// The session-row kind this manager's own session carries: a
    /// manager for a login in flight owns a pending row, a signed-in
    /// account owns an account row.
    const fn own_kind(&self) -> crate::db::SessionKind {
        if self.user_id == super::UNKNOWN_USER {
            crate::db::SessionKind::Pending
        } else {
            crate::db::SessionKind::Account
        }
    }

    /// Client for this account; built on first use and cached forever.
    /// Missing credentials are remembered as a deterministic error.
    pub async fn ensure(&self) -> Result<Arc<Mutex<Client>>, String> {
        let mut st = self.st.lock().await;
        if let Some(conn) = &st.conn {
            return Ok(conn.client.clone());
        }
        if !self.cfg.tg_configured() {
            let msg =
                "Telegram is not configured: set api_id and api_hash in config.toml".to_string();
            st.config_error = Some(msg.clone());
            return Err(msg);
        }

        drop(st);
        let conn = self.open_conn(&self.session_key, self.own_kind()).await?;
        let client = conn.client.clone();
        self.st.lock().await.conn = Some(conn);
        Ok(client)
    }

    /// Client with a live `MTProto` connection: connects on first use (auth
    /// key handshake + connection pool), then only checks the flag.
    #[allow(clippy::significant_drop_tightening)] // the guard must span check+connect: two tasks must not double-connect one client
    pub(super) async fn ensure_connected(&self) -> Result<Arc<Mutex<Client>>, String> {
        let client = self.ensure().await?;
        {
            let mut c = client.lock().await;
            if !c.is_connected() {
                c.connect()
                    .await
                    .map_err(|e| friendly(format!("cannot connect to Telegram: {e}")))?;
                tracing::info!(user_id = self.user_id, "connected to Telegram");
            }
        }
        Ok(client)
    }

    /// Stops every connection this manager holds. Kept for call sites: the
    /// bots go first, then the owner connection.
    pub(super) async fn close(&self) {
        let (conn, bots) = {
            let mut st = self.st.lock().await;
            st.peers.clear();
            st.me = None;
            (st.conn.take(), std::mem::take(&mut st.bots))
        };
        for (_, bot) in bots {
            bot.conn.close().await;
        }
        if let Some(conn) = conn {
            conn.close().await;
        }
    }

    /// Status for /api/me. Never fails — reports degradation in the payload.
    #[allow(clippy::arithmetic_side_effects)] // retry counter is bounded at < 2, so `attempt += 1` cannot overflow
    pub async fn status(&self) -> TgStatus {
        match self.ensure_connected().await {
            Err(e) => TgStatus {
                connected: false,
                authorized: false,
                user: None,
                relogin: mtprsto::error::is_auth_error_message(&e),
                error: Some(e),
            },
            Ok(client) => {
                // A stale connection can surface here; treat auth failure as
                // "needs relogin" and anything else as unreachability after a
                // few short retries (the first call right after boot can fail
                // while the handshake is still in flight).
                let mut attempt = 0u32;
                loop {
                    let result = client.lock().await.get_me().await;
                    match result {
                        Ok(user) => {
                            let info = UserInfo {
                                id: user.id().0,
                                name: user.full_name(),
                                username: user.username().map(ToString::to_string),
                                phone: user.phone().map(ToString::to_string),
                            };
                            let mut st = self.st.lock().await;
                            if st.me.is_none() {
                                st.me = Some(info);
                            }
                            return TgStatus {
                                connected: true,
                                authorized: true,
                                user: st.me.clone(),
                                relogin: false,
                                error: None,
                            };
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if !e.is_session_dead() && attempt < 2 {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                                continue;
                            }
                            let auth = e.is_session_dead();
                            return TgStatus {
                                connected: false,
                                authorized: false,
                                user: None,
                                relogin: auth,
                                error: Some(if auth {
                                    friendly(msg)
                                } else {
                                    format!("Telegram unreachable: {msg}")
                                }),
                            };
                        }
                    }
                }
            }
        }
    }

    /// Current profile photo of the signed-in user as image bytes (largest
    /// server-side thumbnail), for the navbar avatar. `None` when the user
    /// has no photo or Telegram is unreachable — callers fall back to the
    /// initial-letter avatar.
    // The client guard deliberately spans the whole fetch: the photo
    // list and the chosen size's download must ride the same client.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn avatar(&self) -> Option<Vec<u8>> {
        let client = self.ensure_connected().await.ok()?;
        let c = client.lock().await;
        // The newest profile photo is the current one.
        let raw = c
            .get_user_photos(&types::InputUser::Self_, 0, 0, 1)
            .await
            .ok()?;
        let photo = first_photo(&raw)?;
        let (id, access_hash, reference, _dc_id, sizes) = match &photo {
            types::Photo::Photo {
                id,
                access_hash,
                file_reference,
                dc_id,
                sizes,
                ..
            } => (id.0, access_hash.0, file_reference.clone(), *dc_id, sizes),
            types::Photo::Empty { .. } => return None,
        };
        // Stripped sizes are partial LQ previews and path sizes are SVG
        // outlines; among the rest the largest one is the avatar.
        let best = sizes
            .iter()
            .filter(|s| {
                !matches!(
                    s,
                    types::PhotoSize::PhotoStrippedSize { .. }
                        | types::PhotoSize::PhotoSizeEmpty { .. }
                        | types::PhotoSize::PhotoPathSize { .. }
                )
            })
            .max_by_key(|s| {
                let (w, h) = s.dimensions();
                // Dimensions are server-issued preview sizes, far below any
                // multiply-overflow range for i64.
                i64::from(w).saturating_mul(i64::from(h.max(1)))
            })?;
        if let types::PhotoSize::PhotoCachedSize { bytes, .. } = best {
            return (!bytes.is_empty()).then(|| bytes.clone());
        }
        let thumb_size = match best {
            types::PhotoSize::PhotoSize { r#type, .. }
            | types::PhotoSize::PhotoSizeProgressive { r#type, .. } => r#type.clone(),
            _ => return None,
        };
        // Photos download through inputPhotoFileLocation#401584e0 keyed by
        // the photo's id/access hash plus the chosen size's type char —
        // mtprsto's `build_get_file` only knows volume/document locations.
        let mut w = TLWriter::new();
        w.write_u32(types::UPLOAD_GET_FILE);
        w.write_i32(0); // flags: no precise, no cdn_supported
        w.write_u32(0x4015_84e0);
        w.write_i64(id);
        w.write_i64(access_hash);
        w.write_bytes(&reference);
        w.write_bytes(thumb_size.as_bytes());
        w.write_i64(0); // offset
        w.write_i32(1024 * 1024); // limit
        let raw = c.invoke_raw(w.into_bytes()).await.ok()?;
        match mtprsto::file::parse_get_file(&raw).ok()? {
            mtprsto::file::GetFile::File { bytes, .. } => {
                (!bytes.is_empty()).then_some(bytes)
            }
            // Thumbnails are never served from a CDN DC.
            mtprsto::file::GetFile::CdnRedirect { .. } => None,
        }
    }

    /// Picks the next storage target when several are configured.
    #[allow(clippy::unused_self)] // kept as a &self method: it is called from routes/files/upload.rs, which is out of scope
    pub fn next_rotation(&self) -> usize {
        use std::sync::atomic::Ordering;
        ROTATION.fetch_add(1, Ordering::Relaxed)
    }
}

/// First `Photo` of a `photos.Photos`/`photos.PhotosSlice` payload.
fn first_photo(data: &[u8]) -> Option<types::Photo> {
    let mut r = TLReader::new(data);
    let ctor = r.read_u32().ok()?;
    let count = match ctor {
        types::PHOTOS_PHOTOS => r.read_vector_header().ok()?,
        types::PHOTOS_PHOTOS_SLICE => {
            let _total = r.read_i32().ok()?;
            r.read_vector_header().ok()?
        }
        _ => return None,
    };
    (0..count).find_map(|_| types::Photo::read_from(&mut r).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::UNKNOWN_USER;

    async fn scratch_db() -> Db {
        let db = surrealdb::Surreal::init();
        crate::db::connect_mem(&db).await.expect("scratch store");
        db
    }

    fn cfg() -> Config {
        Config {
            api_id: 1,
            api_hash: "test".to_string(),
            ..Config::default()
        }
    }

    /// A manager persists its session through the embedded store: mtprsto
    /// saves through the DbSessions backend and a fresh manager over the
    /// same key reads the very same auth key back. No network involved —
    /// building the client does not connect. Multi-thread flavor: the
    /// sync storage bridge parks this task while the runtime's other
    /// workers drive the embedded engine.
    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_round_trip_through_the_store() {
        let db = scratch_db().await;
        let manager = TgManager::new(cfg(), db.clone(), "pending-test".into(), UNKNOWN_USER);

        let conn = manager
            .open_conn("pending-test", crate::db::SessionKind::Pending)
            .await
            .expect("client builds over the row");
        // A save through the live client's storage, standing in for one a
        // connection makes during the handshake.
        let mut storage = DbSessions::new(
            db.clone(),
            "pending-test".into(),
            crate::db::SessionKind::Pending,
            UNKNOWN_USER,
        );
        SessionStorage::save(
            &mut storage,
            &mtprsto::session::SessionData::from_auth_key(&[7u8; 256], 0, 2),
        )
        .expect("session blob written");
        drop(conn);

        // The row carries the pending kind — it belongs to a login in
        // flight, not to a signed-in account.
        assert_eq!(
            crate::db::read_session(&db, "pending-test")
                .await
                .unwrap()
                .is_some(),
            true
        );
        let data = manager
            .session_data("pending-test")
            .await
            .expect("row parses")
            .expect("row present");
        assert_eq!(data.dc_id, 2);
        assert_eq!(data.auth_key, base64_of(&[7u8; 256]));
    }

    /// A session row mtprsto cannot parse would fail every connect
    /// forever; open_conn drops it instead, so the account can simply
    /// sign in again — the row-era twin of the file storage's move-aside.
    #[tokio::test(flavor = "multi_thread")]
    async fn unreadable_session_rows_are_dropped() {
        let db = scratch_db().await;
        crate::db::write_session(
            &db,
            "user-9",
            crate::db::SessionKind::Account,
            9,
            "definitely not a session blob",
        )
        .await
        .unwrap();
        let manager = TgManager::new(cfg(), db.clone(), "user-9".into(), 9);

        let conn = manager
            .open_conn("user-9", crate::db::SessionKind::Account)
            .await
            .expect("client builds despite the unreadable row");

        assert_eq!(
            crate::db::read_session(&db, "user-9").await.unwrap(),
            None,
            "the unreadable row is gone"
        );
        drop(conn);
    }

    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}