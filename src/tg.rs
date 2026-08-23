use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::message::InputMessage;
use grammers_client::session::storages::SqliteSession;
use grammers_client::tl;
use grammers_client::{Client, InvocationError, SenderPool};
use tokio::sync::Mutex;

/// Global round-robin counter, shared by upload-target selection and bot
/// rotation so both spread evenly.
static ROTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub use grammers_client::session::types::{PeerAuth, PeerId, PeerRef};

use crate::config::Config;

const FILE_REFERENCE_EXPIRED: &str = "FILE_REFERENCE_EXPIRED";
const FILEREF_UPGRADE_NEEDED: &str = "FILEREF_UPGRADE_NEEDED";

#[derive(Debug, Clone, serde::Serialize)]
pub struct UserInfo {
    pub id: i64,
    pub name: String,
    pub username: Option<String>,
}

/// True when an RPC failure means the session's auth key is not bound to a
/// logged-in user (stale/partial login, revoked or expired session).
fn is_auth_error(err: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "AUTH_KEY_UNREGISTERED",
        "AUTH_KEY_INVALID",
        "SESSION_EXPIRED",
        "SESSION_REVOKED",
        "USER_DEACTIVATED",
    ];
    MARKERS.iter().any(|m| err.contains(m))
}

/// Stable copy for auth-dead errors; the API layer maps this exact string
/// to HTTP 401 so clients can react structurally, not by matching prose.
pub const SESSION_INVALID_MSG: &str = "Telegram session expired or was revoked — sign in again";

fn friendly(err: String) -> String {
    if is_auth_error(&err) {
        SESSION_INVALID_MSG.to_string()
    } else {
        err
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TgStatus {
    pub connected: bool,
    pub authorized: bool,
    pub user: Option<UserInfo>,
    pub error: Option<String>,
    /// True when the only way forward is a fresh Telegram sign-in
    /// (revoked/expired/unregistered session key).
    pub relogin: bool,
}

enum LoginFlow {
    None,
    CodeSent {
        phone: String,
        token: Box<LoginToken>,
    },
    PasswordNeeded {
        token: Box<PasswordToken>,
    },
}

pub enum SignInOutcome {
    Done,
    PasswordRequired { hint: Option<String> },
}

/// A chat offered as a storage target by the picker UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelInfo {
    /// Stable key resolvable by `storage_peer`: "me" or a bot-api chat id.
    pub chat: String,
    pub title: String,
}

/// Cheap clone of a bot's live pieces for rotation/invite loops.
struct BotHandle {
    client: Client,
    username: String,
}

struct BotSession {
    client: Client,
    username: String,
    id: i64,
    access_hash: Option<i64>,
}

struct State {
    client: Option<Client>,
    config_error: Option<String>,
    login: LoginFlow,
    peers: HashMap<String, PeerRef>,
    me: Option<UserInfo>,
    bots: HashMap<i64, BotSession>,
    failed_logins: u32,
    blocked_until: Option<std::time::Instant>,
}

/// Failed code/password attempts before login is temporarily blocked.
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// How long the block lasts.
const LOGIN_BLOCK_SECS: u64 = 300;

pub struct TgManager {
    cfg: Arc<Config>,
    st: Mutex<State>,
}


impl TgManager {
    pub fn new(cfg: Arc<Config>) -> Self {
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
    async fn open_client(&self, session_path: &str) -> Result<Client, String> {
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

    fn bot_session_path(&self, token: &str) -> String {
        // Deterministic per bot: numeric prefix of the token.
        let key = token.split(':').next().unwrap_or("bot");
        let base = self.cfg.session_path.trim_end_matches(".db");
        format!("{base}_bot_{key}.db")
    }

    /// Signs a bot in (or restores its persisted session) and adds it to
    /// the download pool.
    pub async fn configure_bot(&self, token: &str) -> Result<(String, i64), String> {
        if !self.cfg.tg_configured() {
            return Err(
                "Telegram is not configured: set api_id and api_hash in config.toml".to_string(),
            );
        }
        let path = self.bot_session_path(token);
        let client = self.open_client(&path).await?;
        let user = client
            .bot_sign_in(token, &self.cfg.api_hash)
            .await
            .map_err(|e| friendly(format!("bot sign-in failed: {e}")))?;
        let (id, access_hash, username) = match &user.raw {
            tl::enums::User::User(u) => (
                u.id,
                u.access_hash,
                u.username.clone().unwrap_or_else(|| format!("bot{}", u.id)),
            ),
            _ => return Err("bot account unavailable".to_string()),
        };
        self.st.lock().await.bots.insert(
            id,
            BotSession {
                client,
                username: username.clone(),
                id,
                access_hash,
            },
        );
        tracing::info!(%id, %username, "bot added to download pool");
        Ok((username, id))
    }

    pub async fn drop_bot(&self, id: i64) {
        self.st.lock().await.bots.remove(&id);
    }

    /// Snapshot of the pool for the settings UI (no tokens).
    pub async fn bot_list(&self) -> Vec<(i64, String)> {
        let st = self.st.lock().await;
        let mut v: Vec<(i64, String)> = st
            .bots
            .values()
            .map(|b| (b.id, b.username.clone()))
            .collect();
        v.sort();
        v
    }

    /// Picks a client for downloads: rotating through the bot pool for
    /// channel-stored files, falling back to the user session (also used
    /// for Saved Messages, which bots cannot read).
    pub async fn download_target(&self, chat: &str) -> Result<(Client, PeerRef), String> {
        let key = chat.trim();
        if let Ok(n) = key.parse::<i64>() {
            let bots = self.st.lock().await.bots.len();
            if bots > 0 {
                let skip = self.next_rotation() % bots;
                let sessions: Vec<BotHandle> = {
                    let st = self.st.lock().await;
                    let mut v: Vec<BotHandle> = st
                        .bots
                        .values()
                        .map(|b| BotHandle {
                            client: b.client.clone(),
                            username: b.username.clone(),
                        })
                        .collect();
                    v.sort_by_key(|b| b.username.clone());
                    v
                };
                let mut last_err = String::new();
                for bs in sessions.iter().cycle().skip(skip).take(bots) {
                    let pid = match PeerId::from_bot_api_dialog_id(n) {
                        Some(p) => p,
                        None => break,
                    };
                    let pref = PeerRef {
                        id: pid,
                        auth: PeerAuth::default(),
                    };
                    match bs.client.resolve_peer(pref).await {
                        Ok(peer) => {
                            let pref = peer
                                .to_ref()
                                .await
                                .map_err(|e| format!("peer ref failed: {e}"))?
                                .ok_or_else(|| format!("chat `{key}` has no usable peer ref"))?;
                            tracing::debug!(bot = %bs.username, "download via bot");
                            return Ok((bs.client.clone(), pref));
                        }
                        Err(e) => {
                            last_err = format!("bot {}: {e}", bs.username);
                        }
                    }
                }
                tracing::warn!("all bots failed for `{chat}` ({last_err}); using user session");
            }
        }
        let client = self.ensure().await?;
        let peer = self.storage_peer(chat).await?;
        Ok((client, peer))
    }

    /// Invites every configured bot into the given storage chat and
    /// promotes it to admin, so downloads work through the pool.
    pub async fn add_bots_to_chat(&self, chat: &str) -> Vec<(String, Result<(), String>)> {
        let sessions: Vec<BotHandle> = {
            let st = self.st.lock().await;
            st.bots
                .values()
                .map(|b| BotHandle {
                    client: b.client.clone(),
                    username: b.username.clone(),
                })
                .collect()
        };
        if sessions.is_empty() {
            return Vec::new();
        }

        let Ok(peer_ref) = self.storage_peer(chat).await else {
            return sessions
                .into_iter()
                .map(|b| (b.username, Err(format!("cannot resolve `{chat}`"))))
                .collect();
        };
        let channel_id = peer_ref.id.bare_id_unchecked();
        let access_hash = peer_ref.auth.hash();
        let input_channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
            channel_id,
            access_hash,
        });

        // The bot's InputUser as seen from the user session.
        let st = self.st.lock().await;
        let input_users: Vec<tl::enums::InputUser> = st
            .bots
            .values()
            .filter_map(|b| {
                b.access_hash
                    .map(|h| {
                        tl::enums::InputUser::User(tl::types::InputUser {
                            user_id: b.id,
                            access_hash: h,
                        })
                    })
            })
            .collect();
        let usernames: Vec<String> = st.bots.values().map(|b| b.username.clone()).collect();
        let user_client = st.client.clone();
        drop(st);

        let Some(user_client) = user_client else {
            return sessions
                .into_iter()
                .map(|b| (b.username, Err("user session not connected".into())))
                .collect();
        };

        let mut results = Vec::new();
        for (username, input_user) in usernames.into_iter().zip(input_users) {
            let res = (|| async {
                match user_client
                    .invoke(&tl::functions::channels::InviteToChannel {
                        channel: input_channel.clone(),
                        users: vec![input_user.clone()],
                    })
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        let s = e.to_string();
                        // Already a member / privacy limits: admin promotion
                        // below is what actually matters for downloads.
                        if s.contains("USER_ALREADY_PARTICIPANT")
                            || s.contains("USER_NOT_MUTUAL_CONTACT")
                            || s.contains("USER_CHANNELS_TOO_MUCH")
                        {
                            tracing::warn!("invite said: {s}; promoting anyway");
                            Ok(())
                        } else {
                            Err(friendly(format!("invite failed: {s}")))
                        }
                    }
                }?;
                user_client
                    .invoke(&tl::functions::channels::EditAdmin {
                        channel: input_channel.clone(),
                        user_id: input_user,
                        admin_rights: tl::enums::ChatAdminRights::Rights(
                            tl::types::ChatAdminRights {
                                change_info: false,
                                post_messages: true,
                                edit_messages: false,
                                delete_messages: false,
                                ban_users: false,
                                invite_users: true,
                                pin_messages: false,
                                add_admins: false,
                                anonymous: false,
                                manage_call: false,
                                other: false,
                                manage_topics: false,
                                post_stories: false,
                                edit_stories: false,
                                delete_stories: false,
                                manage_direct_messages: false,
                                manage_ranks: false,
                            },
                        ),
                        rank: Some("storage".to_string()),
                    })
                    .await
                    .map_err(|e| friendly(format!("promoting failed: {e}")))?;
                Ok(())
            })()
            .await;
            results.push((username, res));
        }
        results
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
    /// Rejects login attempts while a brute-force block is active.
    fn login_gate(st: &mut State) -> Result<(), String> {
        if let Some(until) = st.blocked_until {
            let now = std::time::Instant::now();
            if now < until {
                let secs = (until - now).as_secs() + 1;
                return Err(format!(
                    "too many failed login attempts; try again in {secs}s"
                ));
            }
            // Block has lapsed.
            st.blocked_until = None;
        }
        Ok(())
    }

    fn record_login_failure(st: &mut State) {
        st.failed_logins += 1;
        if st.failed_logins >= MAX_LOGIN_ATTEMPTS {
            st.blocked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_BLOCK_SECS));
            st.failed_logins = 0;
            tracing::warn!("login blocked for {LOGIN_BLOCK_SECS}s after repeated failures");
        }
    }

    fn record_login_success(st: &mut State) {
        st.failed_logins = 0;
        st.blocked_until = None;
    }

    pub async fn send_code(&self, phone: &str) -> Result<(), String> {
        {
            // The flow is deliberately single-user: a new code request
            // replaces any pending one, so a stale browser tab can never
            // complete a login started by another.
            let st = self.st.lock().await;
            if !matches!(st.login, LoginFlow::None) {
                tracing::warn!("replacing a pending login flow with a new code request");
            }
        }
        match self.try_send_code(phone).await {
            // A session that holds an auth key Telegram does not know (partial
            // login, revoked session) rejects even SendCode; rebuild it once.
            Err(e) if is_auth_error(&e) => {
                self.reset_session().await?;
                self.try_send_code(phone).await
            }
            other => other,
        }
    }

    /// Drops the persisted MTProto session and reconnects from scratch.
    async fn reset_session(&self) -> Result<(), String> {
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

    async fn try_send_code(&self, phone: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        let token = client
            .request_login_code(phone, &self.cfg.api_hash)
            .await
            .map_err(|e| friendly(format!("failed to send code: {e}")))?;
        let mut st = self.st.lock().await;
        st.login = LoginFlow::CodeSent {
            phone: phone.to_string(),
            token: Box::new(token),
        };
        Ok(())
    }

    pub async fn sign_in(&self, code: &str) -> Result<SignInOutcome, String> {
        let client = self.ensure().await?;
        {
            let mut st = self.st.lock().await;
            Self::login_gate(&mut st)?;
        }
        let (phone, token) = match std::mem::replace(&mut self.st.lock().await.login, LoginFlow::None)
        {
            LoginFlow::CodeSent { phone, token } => (phone, *token),
            _ => return Err("no code was requested".to_string()),
        };
        match client.sign_in(&token, code).await {
            Ok(_user) => {
                self.finish_auth(&client).await;
                Ok(SignInOutcome::Done)
            }
            Err(grammers_client::SignInError::PasswordRequired(pt)) => {
                let hint = pt.hint().map(|h| h.to_string());
                self.st.lock().await.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Ok(SignInOutcome::PasswordRequired { hint })
            }
            Err(grammers_client::SignInError::InvalidCode) => {
                // Keep the token so the user can retry with a new code entry.
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                st.login = LoginFlow::CodeSent {
                    phone,
                    token: Box::new(token),
                };
                Err("invalid confirmation code".to_string())
            }
            Err(grammers_client::SignInError::InvalidPassword(pt)) => {
                self.st.lock().await.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Err("invalid password".to_string())
            }
            Err(grammers_client::SignInError::SignUpRequired) => {
                Err("this account needs sign-up, which is not supported".to_string())
            }
            Err(grammers_client::SignInError::Other(e)) => {
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                drop(st);
                Err(format!("sign-in failed: {e}"))
            }
        }
    }

    pub async fn check_password(&self, password: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        {
            let mut st = self.st.lock().await;
            Self::login_gate(&mut st)?;
        }
        let pt = match std::mem::replace(&mut self.st.lock().await.login, LoginFlow::None) {
            LoginFlow::PasswordNeeded { token, .. } => *token,
            _ => return Err("no password step pending".to_string()),
        };
        match client.check_password(pt, password).await {
            Ok(_user) => {
                self.finish_auth(&client).await;
                Ok(())
            }
            Err(grammers_client::SignInError::InvalidPassword(pt2)) => {
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                st.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt2),
                };
                Err("invalid password".to_string())
            }
            Err(other) => Err(format!("password check failed: {other:?}")),
        }
    }

    async fn finish_auth(&self, client: &Client) {
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

    /// Resolves (and caches) a chat key — "me", "@username" or "-100<id>" —
    /// to a peer reference.
    pub async fn storage_peer(&self, chat: &str) -> Result<PeerRef, String> {
        let key = chat.trim();
        let cache_key = key.to_ascii_lowercase();

        {
            let st = self.st.lock().await;
            if let Some(peer) = st.peers.get(&cache_key) {
                return Ok(*peer);
            }
        }

        let client = self.ensure().await?;
        let peer_ref = if key.is_empty()
            || key.eq_ignore_ascii_case("me")
            || key.eq_ignore_ascii_case("self")
        {
            client
                .get_me()
                .await
                .map_err(|e| friendly(format!("cannot resolve own peer: {e}")))?
                .to_ref()
                .await
                .map_err(|e| friendly(format!("cannot resolve own peer: {e}")))?
                .ok_or("cannot resolve own peer")?
        } else if let Ok(n) = key.parse::<i64>() {
            let pid = PeerId::from_bot_api_dialog_id(n)
                .ok_or_else(|| format!("chat id `{key}` is not a valid chat"))?;
            let pref = PeerRef {
                id: pid,
                auth: PeerAuth::default(),
            };
            let peer = client
                .resolve_peer(pref)
                .await
                .map_err(|e| friendly(format!("cannot resolve chat {key}: {e}")))?;
            peer.to_ref()
                .await
                .map_err(|e| friendly(format!("cannot resolve chat {key}: {e}")))?
                .ok_or_else(|| format!("chat `{key}` has no usable peer reference"))?
        } else {
            let name = key.trim_start_matches('@');
            let peer = client
                .resolve_username(name)
                .await
                .map_err(|e| friendly(format!("resolve failed: {e}")))?
                .ok_or_else(|| format!("storage chat `{key}` not found or not accessible"))?;
            peer.to_ref()
                .await
                .map_err(|e| format!("storage chat `{key}` peer lookup failed: {e}"))?
                .ok_or_else(|| format!("storage chat `{key}` has no usable peer reference"))?
        };

        self.st
            .lock()
            .await
            .peers
            .insert(cache_key, peer_ref);
        Ok(peer_ref)
    }

    /// Picks the next storage target when several are configured.
    pub fn next_rotation(&self) -> usize {
        use std::sync::atomic::Ordering;
        ROTATION.fetch_add(1, Ordering::Relaxed)
    }

    /// Channels and groups this account could use as storage targets:
    /// both the main dialog list AND the archived folder (storage channels
    /// are typically muted and archived, so they only show up there).
    /// "Saved Messages" is always first.
    pub async fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let client = self.ensure().await?;
        let mut out = vec![ChannelInfo {
            chat: "me".to_string(),
            title: "Saved Messages (my account)".to_string(),
        }];
        let mut seen: HashSet<String> = HashSet::new();
        Self::collect_folder(&client, None, &mut out, &mut seen).await?;
        Self::collect_folder(&client, Some(1), &mut out, &mut seen).await?;
        Ok(out)
    }

/// Harvests storage-capable chats (channels, supergroups, groups) from one
/// dialogs folder via raw `messages.getDialogs`, paginating until exhausted.
async fn collect_folder(
    client: &Client,
    folder_id: Option<i32>,
    out: &mut Vec<ChannelInfo>,
    seen: &mut HashSet<String>,
) -> Result<(), String> {
    let mut offset_date: i32 = 0;
    let mut offset_id: i32 = 0;
    let mut offset_peer = tl::enums::InputPeer::Empty;

    for _page in 0..20 {
        let resp = client
            .invoke(&tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id,
                offset_date,
                offset_id,
                offset_peer: offset_peer.clone(),
                limit: 100,
                hash: 0,
            })
            .await
            .map_err(|e| friendly(format!("listing dialogs failed: {e}")))?;

        let (dialogs, messages, chats) = match resp {
            tl::enums::messages::Dialogs::Dialogs(d) => (d.dialogs, d.messages, d.chats),
            tl::enums::messages::Dialogs::Slice(s) => (s.dialogs, s.messages, s.chats),
            tl::enums::messages::Dialogs::NotModified(_) => return Ok(()),
        };

        Self::harvest_chats(&chats, out, seen);
        if dialogs.len() < 100 {
            return Ok(());
        }

        // Advance the cursor to the last dialog of this page. The offset
        // date comes from that dialog's top message (the Dialog struct no
        // longer carries its own date).
        let Some(tl::enums::Dialog::Dialog(dd)) = dialogs.last() else {
            return Ok(());
        };
        let raw_id = match &dd.peer {
            tl::enums::Peer::Channel(p) => p.channel_id,
            tl::enums::Peer::Chat(p) => p.chat_id,
            tl::enums::Peer::User(p) => p.user_id,
        };
        let mut msg_date = 0i32;
        for m in &messages {
            if let tl::enums::Message::Message(mm) = m {
                if mm.id != dd.top_message {
                    continue;
                }
                let mid = match &mm.peer_id {
                    tl::enums::Peer::Channel(p) => p.channel_id,
                    tl::enums::Peer::Chat(p) => p.chat_id,
                    tl::enums::Peer::User(p) => p.user_id,
                };
                if mid == raw_id {
                    msg_date = mm.date;
                    break;
                }
            }
        }
        offset_date = msg_date;
        offset_id = dd.top_message;
        offset_peer = Self::input_peer_for(raw_id, &chats);
    }
    Ok(())
}

/// Adds every joined channel/group from a chats array to the output.
fn harvest_chats(
    chats: &[tl::enums::Chat],
    out: &mut Vec<ChannelInfo>,
    seen: &mut HashSet<String>,
) {
    for c in chats {
        match c {
            tl::enums::Chat::Channel(ch) => {
                if ch.left && !ch.creator {
                    continue; // we left it — cannot post
                }
                let key = format!("-100{}", ch.id);
                if seen.insert(key.clone()) {
                    out.push(ChannelInfo {
                        chat: key,
                        title: ch.title.clone(),
                    });
                }
            }
            tl::enums::Chat::Chat(g) => {
                if g.left && !g.creator {
                    continue;
                }
                let key = format!("-{}", g.id);
                if seen.insert(key.clone()) {
                    out.push(ChannelInfo {
                        chat: key,
                        title: g.title.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Builds an InputPeer for pagination offsets from the same response's
/// chats array (which carries the access hashes).
fn input_peer_for(raw_id: i64, chats: &[tl::enums::Chat]) -> tl::enums::InputPeer {
    for c in chats {
        match c {
            tl::enums::Chat::Channel(ch) if ch.id == raw_id => {
                return tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: ch.id,
                    access_hash: ch.access_hash.unwrap_or(0),
                });
            }
            tl::enums::Chat::Chat(g) if g.id == raw_id => {
                return tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: g.id });
            }
            _ => {}
        }
    }
    tl::enums::InputPeer::Empty
}

    /// Creates a new broadcast channel owned by the signed-in account and
    /// returns it as a storage target.
    pub async fn create_channel(&self, title: &str, about: &str) -> Result<ChannelInfo, String> {
        use grammers_client::tl;

        let client = self.ensure().await?;
        let updates = client
            .invoke(&tl::functions::channels::CreateChannel {
                broadcast: true,
                megagroup: false,
                for_import: false,
                forum: false,
                title: title.to_string(),
                about: about.to_string(),
                geo_point: None,
                address: None,
                ttl_period: None,
            })
            .await
            .map_err(|e| friendly(format!("creating channel failed: {e}")))?;

        // The new channel comes back inside the updates' chat list; convert
        // its raw id into the bot-api form ("-100…") used as our chat key.
        let chats = match updates {
            tl::enums::Updates::Updates(u) => u.chats,
            tl::enums::Updates::Combined(u) => u.chats,
            _ => Vec::new(),
        };
        for chat in chats {
            if let tl::enums::Chat::Channel(c) = chat {
                tracing::info!("created storage channel `{}`", c.title);
                return Ok(ChannelInfo {
                    chat: format!("-100{}", c.id),
                    title: c.title,
                });
            }
        }
        Err("channel was created but its identifier could not be read".to_string())
    }

    /// Streams `reader` up to Telegram and posts it as a document message in
    /// the given storage chat. Returns `(message id, name, mime, thumb)` —
    /// thumb is the tiny JPEG Telegram generates, when it made one.
    pub async fn upload<S>(
        &self,
        reader: &mut S,
        size: u64,
        name: &str,
        mime: &str,
        chat: &str,
    ) -> Result<(i32, String, String, Option<Vec<u8>>), String>
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        let client = self.ensure().await?;
        let peer = self.storage_peer(chat).await?;

        let uploaded = client
            .upload_stream(reader, size as usize, name.to_string())
            .await
            .map_err(|e| friendly(format!("upload to telegram failed: {e}")))?;

        let msg = client
            .send_message(
                peer,
                InputMessage::new()
                    .text("")
                    .document(uploaded)
                    .mime_type(mime),
            )
            .await
            .map_err(|e| friendly(format!("sending message failed: {e}")))?;

        let thumb = msg.media().and_then(|m| match m {
            grammers_client::media::Media::Document(doc) => doc
                .thumbs()
                .into_iter()
                .find_map(|t| match t {
                    grammers_client::media::PhotoSize::Stripped(s) => {
                        let jpeg = stripped_thumb_jpeg(&s.bytes);
                        (!jpeg.is_empty()).then_some(jpeg)
                    }
                    _ => None,
                }),
            _ => None,
        });
        Ok((msg.id(), name.to_string(), mime.to_string(), thumb))
    }

    pub async fn delete_message(&self, message_id: i32, chat: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        let peer = self.storage_peer(chat).await?;
        client
            .delete_messages(peer, &[message_id])
            .await
            .map_err(|e| format!("deleting telegram message failed: {e}"))?;
        Ok(())
    }

    /// Sends `text` to @BotFather and returns its reply text. The
    /// conversation state lives on BotFather's side, so the wizard on the
    /// web side only needs this one relay primitive.
    pub async fn botfather_send(&self, text: &str) -> Result<String, String> {
        let client = self.ensure().await?;
        let peer = self.storage_peer("botfather").await?;
        let sent = client
            .send_message(peer, InputMessage::new().text(text))
            .await
            .map_err(|e| friendly(format!("botfather send failed: {e}")))?;

        // BotFather usually answers within a second; poll the dialog for a
        // newer incoming message. Give up after ~8s so the HTTP request
        // cannot hang.
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(
                if attempt < 4 { 300 } else { 600 },
            ))
            .await;
            let mut it = client.iter_messages(peer).limit(1);
            if let Ok(Some(msg)) = it.next().await {
                if !msg.outgoing() && msg.id() > sent.id() {
                    return Ok(msg.text().to_string());
                }
            }
        }
        Err("BotFather did not answer — try again shortly".to_string())
    }
}


/// Rebuilds a displayable JPEG from Telegram's stripped thumbnail bytes
/// (https://core.tlgr.org/api/files#stripped-thumbnails). Mirrors
/// grammers' private StrippedSize::data — same header, dimensions from
/// bytes[1..3], scan data from bytes[3..], JPEG EOI footer.
pub fn stripped_thumb_jpeg(bytes: &[u8]) -> Vec<u8> {
    const HEADER: [u8; 623] = [
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
    0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x28, 0x1c, 0x1e,
    0x23, 0x1e, 0x19, 0x28, 0x23, 0x21, 0x23, 0x2d, 0x2b, 0x28, 0x30, 0x3c, 0x64, 0x41,
    0x3c, 0x37, 0x37, 0x3c, 0x7b, 0x58, 0x5d, 0x49, 0x64, 0x91, 0x80, 0x99, 0x96, 0x8f,
    0x80, 0x8c, 0x8a, 0xa0, 0xb4, 0xe6, 0xc3, 0xa0, 0xaa, 0xda, 0xad, 0x8a, 0x8c, 0xc8,
    0xff, 0xcb, 0xda, 0xee, 0xf5, 0xff, 0xff, 0xff, 0x9b, 0xc1, 0xff, 0xff, 0xff, 0xfa,
    0xff, 0xe6, 0xfd, 0xff, 0xf8, 0xff, 0xdb, 0x00, 0x43, 0x01, 0x2b, 0x2d, 0x2d, 0x3c,
    0x35, 0x3c, 0x76, 0x41, 0x41, 0x76, 0xf8, 0xa5, 0x8c, 0xa5, 0xf8, 0xf8, 0xf8, 0xf8,
    0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
    0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
    0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
    0xf8, 0xf8, 0xf8, 0xf8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x00, 0x00, 0x00, 0x03,
    0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00, 0x1f, 0x00,
    0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
    0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05,
    0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05,
    0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81,
    0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62,
    0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29,
    0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
    0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84,
    0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
    0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5,
    0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5,
    0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9,
    0xfa, 0xff, 0xc4, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
    0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01,
    0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00,
    0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
    0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23,
    0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1,
    0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57,
    0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a,
    0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6,
    0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2,
    0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7,
    0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3,
    0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00,
    0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
    ];
    if bytes.len() < 4 || bytes[0] != 0x01 {
        return Vec::new();
    }
    let mut out = HEADER.to_vec();
    out[164] = bytes[1];
    out[166] = bytes[2];
    out.extend_from_slice(&bytes[3..]);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

pub(crate) fn is_file_reference_error(err: &InvocationError) -> bool {
    if let InvocationError::Rpc(rpc) = err {
        rpc.is(FILE_REFERENCE_EXPIRED) || rpc.is(FILEREF_UPGRADE_NEEDED)
    } else {
        false
    }
}

async fn get_me_info(client: &Client) -> Option<UserInfo> {
    match client.get_me().await {
        Ok(u) => Some(UserInfo {
            id: u.id().bare_id_unchecked(),
            name: u.full_name(),
            username: u.username().map(|s| s.to_string()),
        }),
        Err(e) => {
            tracing::warn!("get_me failed: {e}");
            None
        }
    }
}

