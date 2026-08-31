use std::collections::HashMap;
use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::serialize::{TLReader, TLWriter};
use mtprsto::session::{SessionStore, SessionStorage};
use mtprsto::types;
use tokio::sync::Mutex;

use crate::config::Config;

use super::{
    Conn, ROTATION, State, TgManager, TgStatus, UserInfo, friendly, is_auth_error,
    is_auth_error_str,
};

impl TgManager {
    /// Builds a manager for one account. `session_path` is that account's
    /// own session file; `user_id` is [`super::UNKNOWN_USER`] while a login
    /// is still in flight and the account is therefore unknown.
    pub fn new(cfg: Config, session_path: String, user_id: i64) -> Self {
        Self {
            cfg,
            session_path,
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

    /// Session file this manager owns. Only the hub needs it, to move or
    /// delete the file once no connection holds it open.
    pub(super) fn session_path(&self) -> &str {
        &self.session_path
    }

    /// Builds (without connecting) the client over `session_path`.
    /// mtprsto sessions are JSON files written atomically, so unlike the
    /// previous SQLite store nothing has to be opened or kept warm here —
    /// the actual `connect` happens on first use.
    pub(super) async fn open_conn(&self, session_path: &str) -> Result<Conn, String> {
        if let Some(parent) = std::path::Path::new(session_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot create session dir: {e}"))?;
        }
        // Sessions written by the previous grammers backend are SQLite
        // databases and cannot be parsed as JSON. Move them aside instead
        // of failing on every request — the account simply signs in again.
        if std::path::Path::new(session_path).exists() {
            let mut probe = SessionStore::new(session_path);
            if let Err(e) = SessionStorage::load(&mut probe) {
                let backup = format!("{session_path}.grammers");
                match tokio::fs::rename(session_path, &backup).await {
                    Ok(()) => tracing::warn!(
                        %session_path,
                        %backup,
                        "unreadable old session moved aside; sign in again ({e})"
                    ),
                    Err(rename_err) => {
                        return Err(format!(
                            "cannot open session {session_path}: {e} (and moving it aside failed: {rename_err})"
                        ));
                    }
                }
            }
        }
        let client = Client::builder()
            .api_id(self.cfg.api_id)
            .api_hash(self.cfg.api_hash.clone())
            .session(session_path)
            .build()
            .map_err(|e| format!("cannot build telegram client: {e}"))?;
        Ok(Conn {
            client: Arc::new(Mutex::new(client)),
        })
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
        let conn = self.open_conn(&self.session_path).await?;
        let client = conn.client.clone();
        self.st.lock().await.conn = Some(conn);
        Ok(client)
    }

    /// Client with a live MTProto connection: connects on first use (auth
    /// key handshake + connection pool), then only checks the flag.
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
                relogin: is_auth_error_str(&e),
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
                                name: super::full_name(&user),
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
                            if !is_auth_error(&e) && attempt < 2 {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                                continue;
                            }
                            let auth = is_auth_error(&e);
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
                i64::from(w) * i64::from(h.max(1))
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
        w.write_u32(0x401584e0);
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
            _ => None,
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

    /// Closing a manager must release its session file: the hub moves that
    /// file into place once a login names its account, and a file still
    /// held open cannot be moved on Windows. No network is involved —
    /// mtprsto sessions are JSON files written atomically, so a written
    /// file stands in for one a live connection saved.
    #[tokio::test]
    async fn closing_releases_the_session_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("pending-test.db");
        let cfg = Config {
            api_id: 1,
            api_hash: "test".to_string(),
            ..Config::default()
        };
        let manager = TgManager::new(cfg, path.to_string_lossy().into_owned(), UNKNOWN_USER);

        let mut store = SessionStore::new(&path);
        store
            .save(&mtprsto::session::SessionData::from_auth_key(
                &[7u8; 256],
                0,
                2,
            ))
            .expect("session file written");
        assert!(path.exists());

        manager.close().await;
        let moved = dir.path().join("77.db");
        tokio::fs::rename(&path, &moved)
            .await
            .expect("closed session file can be moved");
        assert!(moved.exists());
    }

    /// A session file the old grammers backend wrote (SQLite) cannot be
    /// parsed; `open_conn` must move it aside so the account can sign in
    /// again instead of erroring forever.
    #[tokio::test]
    async fn legacy_sessions_are_moved_aside() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("legacy.db");
        // SQLite magic header — grammers-session wrote these.
        tokio::fs::write(&path, b"SQLite format 3\0garbage")
            .await
            .expect("legacy file written");
        let cfg = Config {
            api_id: 1,
            api_hash: "test".to_string(),
            ..Config::default()
        };
        let manager = TgManager::new(cfg, path.to_string_lossy().into_owned(), UNKNOWN_USER);

        let conn = manager
            .open_conn(path.to_string_lossy().as_ref())
            .await
            .expect("session opens despite the legacy file");

        assert!(!path.exists(), "legacy file moved");
        assert!(
            dir.path().join("legacy.db.grammers").exists(),
            "moved next to the original"
        );
        drop(conn);
    }
}
