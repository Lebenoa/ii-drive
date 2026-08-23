use std::collections::HashMap;
use std::sync::Arc;

use grammers_client::session::storages::SqliteSession;
use grammers_client::tl;
use grammers_client::{Client, SenderPool};
use tokio::sync::Mutex;

use crate::config::Config;

use super::{
    LoginFlow, PeerRef, ROTATION, State, TgManager, TgStatus, friendly, get_me_info, is_auth_error,
};

impl TgManager {
    pub fn new(cfg: Config) -> Self {
        TgManager {
            cfg,
            st: Mutex::new(State {
                client: None,
                config_error: None,
                login: LoginFlow::None,
                peers: HashMap::new(),
                me: None,
                bots: HashMap::new(),
                failed_logins: 0,
                blocked_until: None,
            }),
        }
    }

    /// Opens a session file and spawns its network runner.
    pub(super) async fn open_client(&self, session_path: &str) -> Result<Client, String> {
        if let Some(parent) = std::path::Path::new(session_path)
            .parent()
            .filter(|p| !p.is_empty())
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
        tokio::spawn(runner.run());
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                tracing::debug!(?update, "telegram update");
            }
        });
        Ok(Client::new(handle))
    }

    /// Connects on first use; caches the client forever after a success.
    /// Missing credentials are remembered as a deterministic error.
    pub async fn ensure(&self) -> Result<Client, String> {
        let mut st = self.st.lock().await;
        if let Some(client) = &st.client {
            return Ok(client.clone());
        }
        if !self.cfg.tg_configured() {
            let msg = "Telegram is not configured: set api_id and api_hash in config.toml"
                .to_string();
            st.config_error = Some(msg.clone());
            return Err(msg);
        }

        let path = self.cfg.session_path.clone();
        drop(st);
        let client = self.open_client(&path).await?;
        tracing::info!("connected to Telegram");
        self.st.lock().await.client = Some(client.clone());
        Ok(client)
    }

    /// Drops the persisted MTProto session and reconnects from scratch.
    pub(super) async fn reset_session(&self) -> Result<(), String> {
        let path = self.cfg.session_path.clone();
        for suffix in ["", "-wal", "-shm"] {
            let p = format!("{path}{suffix}");
            match tokio::fs::remove_file(&p).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot reset session file {p}: {e}")),
            }
        }
        let mut st = self.st.lock().await;
        st.client = None;
        st.peers.clear();
        st.me = None;
        st.failed_logins = 0;
        st.blocked_until = None;
        tracing::warn!("stale telegram session discarded, a fresh one will be created");
        drop(st);
        self.ensure().await.map(|_| ())
    }

    /// Status for /api/me. Never fails — reports degradation in the payload.
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
                            }
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

    pub(super) async fn finish_auth(&self, client: &Client) {
        let info = get_me_info(client).await;
        let mut st = self.st.lock().await;
        st.login = LoginFlow::None;
        st.me = info;
        Self::record_login_success(&mut st);
        tracing::info!("telegram account linked");
    }

    /// Telegram account id of the signed-in user, when known. After a
    /// restart the cache is empty, so this connects and fills it rather
    /// than reporting "nobody" (which made uploads ignore the user's
    /// channel selection until /api/me happened to run).
    pub async fn current_user_id(&self) -> Option<i64> {
        {
            let st = self.st.lock().await;
            if let Some(m) = &st.me {
                return Some(m.id);
            }
        }
        let client = self.ensure().await.ok()?;
        let mut st = self.st.lock().await;
        if st.me.is_none() {
            st.me = get_me_info(&client).await;
        }
        st.me.as_ref().map(|m| m.id)
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
            .max_by_key(|t| t.size())?;
        let mut it = client.iter_download(&thumb).chunk_size(64 * 1024);
        let mut out = Vec::new();
        while let Some(chunk) = it.next().await.ok()? {
            out.extend_from_slice(&chunk);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Picks the next storage target when several are configured.
    pub fn next_rotation(&self) -> usize {
        use std::sync::atomic::Ordering;
        ROTATION.fetch_add(1, Ordering::Relaxed)
    }
}
