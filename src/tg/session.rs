use std::collections::HashMap;
use std::sync::Arc;

use grammers_client::session::storages::SqliteSession;
use grammers_client::tl;
use grammers_client::{Client, SenderPool};
use tokio::sync::Mutex;

use crate::config::Config;

use super::{
    Conn, PeerRef, ROTATION, State, TgManager, TgStatus, friendly, get_me_info, is_auth_error,
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

    /// Opens a session file and spawns its network runner.
    pub(super) async fn open_conn(&self, session_path: &str) -> Result<Conn, String> {
        if let Some(parent) = std::path::Path::new(session_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot create session dir: {e}"))?;
        }
        let session = Arc::new(
            SqliteSession::open(session_path)
                .await
                .map_err(|e| format!("cannot open session {session_path}: {e}"))?,
        );
        let SenderPool {
            runner,
            handle,
            mut updates,
        } = SenderPool::new(session, self.cfg.api_id);
        let dc_id = handle
            .session
            .home_dc_id()
            .map_err(|e| format!("cannot determine home DC: {e}"))?;
        let pool = handle.thin.clone();
        let runner = tokio::spawn(runner.run());
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                tracing::debug!(?update, "telegram update");
            }
        });

        // Open extra connections to the same DC so upload workers can
        // spread across multiple TCP pipes instead of pipelining through
        // one.  Each pool gets its own copy of the session file to
        // avoid SQLite locking conflicts (grammers writes during normal
        // ops — bound file refs, DC bookkeeping — so concurrent access
        // to a single file would wedge).
        let mut aux_pools = Vec::with_capacity(super::AUX_UPLOAD_POOLS);
        let mut aux_runners = Vec::with_capacity(super::AUX_UPLOAD_POOLS);
        let mut aux_session_paths = Vec::with_capacity(super::AUX_UPLOAD_POOLS);
        for idx in 0..super::AUX_UPLOAD_POOLS {
            // Copy the main session so each aux pool has its own
            // SQLite file — same auth key, independent state tracker.
            let aux_path = format!("{session_path}.aux{idx}");
            if let Err(e) = tokio::fs::copy(session_path, &aux_path).await {
                // First connection may not have flushed yet; an empty
                // file is fine — SqliteSession will initialise it.
                if tokio::fs::metadata(session_path).await.is_ok() {
                    return aux_cleanup(
                        &mut aux_pools,
                        &mut aux_runners,
                        &mut aux_session_paths,
                        pool,
                        runner,
                        format!("cannot copy session for aux {idx}: {e}"),
                    );
                }
            }
            let aux_session = match SqliteSession::open(&aux_path).await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    let _ = tokio::fs::remove_file(&aux_path).await;
                    return aux_cleanup(
                        &mut aux_pools,
                        &mut aux_runners,
                        &mut aux_session_paths,
                        pool,
                        runner,
                        format!("cannot open aux session {idx}: {e}"),
                    );
                }
            };
            let SenderPool {
                runner: aux_runner,
                handle: aux_handle,
                mut updates,
            } = SenderPool::new(aux_session, self.cfg.api_id);
            aux_pools.push(aux_handle.thin.clone());
            aux_runners.push(tokio::spawn(aux_runner.run()));
            aux_session_paths.push(aux_path);
            tokio::spawn(async move {
                while let Some(update) = updates.recv().await {
                    tracing::debug!(?update, "telegram update (aux)");
                }
            });
        }
        tracing::info!(
            dc_id,
            connections = 1 + aux_pools.len(),
            "opened upload connections"
        );

        Ok(Conn {
            client: Client::new(handle),
            pool,
            runner,
            dc_id,
            aux_pools,
            aux_runners,
            aux_session_paths,
        })
    }

    /// Connects on first use; caches the client forever after a success.
    /// Missing credentials are remembered as a deterministic error.
    pub async fn ensure(&self) -> Result<Client, String> {
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
        tracing::info!(user_id = self.user_id, "connected to Telegram");
        self.st.lock().await.conn = Some(conn);
        Ok(client)
    }

    /// Stops every connection this manager holds and releases its session
    /// files. Required before the hub may move or delete them: an open
    /// SQLite file cannot be renamed on Windows.
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
        match self.ensure().await {
            Err(e) => TgStatus {
                connected: false,
                authorized: false,
                user: None,
                relogin: is_auth_error(&e),
                error: Some(e),
            },
            Ok(client) => {
                // A stale connection can surface here; treat RPC failure as
                // "not connected" so the UI shows the error instead of
                // hanging. The first call right after boot can fail simply
                // because the MTProto handshake is still in flight, so
                // transient errors get a few short retries first.
                let mut attempt = 0u32;
                loop {
                    match client.is_authorized().await {
                        Ok(true) => {
                            let mut st = self.st.lock().await;
                            if st.me.is_none() {
                                st.me = get_me_info(&client).await;
                            }
                            return TgStatus {
                                connected: true,
                                authorized: true,
                                user: st.me.clone(),
                                relogin: false,
                                error: None,
                            };
                        }
                        Ok(false) => {
                            return TgStatus {
                                connected: true,
                                authorized: false,
                                user: None,
                                // Keyed but signed out — a fresh sign-in is
                                // required.
                                relogin: true,
                                error: None,
                            };
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if !is_auth_error(&msg) && attempt < 2 {
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                                continue;
                            }
                            let auth = is_auth_error(&msg);
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
        let client = self.ensure().await.ok()?;
        let self_ref: PeerRef = tl::types::InputPeerSelf {}.into();
        let mut photos = client.iter_profile_photos(self_ref);
        let photo = photos.next().await.ok()??;
        // Stripped sizes are partial LQ previews; anything else downloads
        // as a real JPEG via its input location.
        let thumb = photo
            .thumbs()
            .into_iter()
            .filter(|t| !matches!(t, grammers_client::media::PhotoSize::Stripped(_)))
            .max_by_key(grammers_client::media::PhotoSize::size)?;
        let mut it = client.iter_download(&thumb).chunk_size(64 * 1024);
        let mut out = Vec::new();
        while let Some(chunk) = it.next().await.ok()? {
            out.extend_from_slice(&chunk);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Picks the next storage target when several are configured.
    #[allow(clippy::unused_self)] // kept as a &self method: it is called from routes/files/upload.rs, which is out of scope
    pub fn next_rotation(&self) -> usize {
        use std::sync::atomic::Ordering;
        ROTATION.fetch_add(1, Ordering::Relaxed)
    }
}

/// Tears down already-spawned runners on aux connection failure,
/// avoiding leaked tasks and held session files.
fn aux_cleanup(
    aux_pools: &mut Vec<grammers_client::sender::SenderPoolHandle>,
    aux_runners: &mut Vec<tokio::task::JoinHandle<()>>,
    aux_session_paths: &mut Vec<String>,
    pool: grammers_client::sender::SenderPoolHandle,
    runner: tokio::task::JoinHandle<()>,
    error: String,
) -> Result<Conn, String> {
    for r in aux_runners.drain(..) {
        r.abort();
    }
    aux_pools.clear();
    for p in aux_session_paths.drain(..) {
        let _ = std::fs::remove_file(p);
    }
    pool.quit();
    runner.abort();
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::UNKNOWN_USER;

    /// Closing a manager must release its session file: the hub moves that
    /// file into place once a login names its account, and an open SQLite
    /// file cannot be renamed on Windows. No network is involved — the
    /// runner only connects when a request is made.
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

        manager.ensure().await.expect("session opens");
        assert!(path.exists());

        manager.close().await;
        let moved = dir.path().join("77.db");
        tokio::fs::rename(&path, &moved)
            .await
            .expect("closed session file can be moved");
        assert!(moved.exists());
    }
}
