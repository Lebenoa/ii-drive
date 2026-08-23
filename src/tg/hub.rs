use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;

use super::TgManager;
use super::login::{CodeStep, Pending};

/// Failed code/password attempts before login is temporarily blocked.
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// How long the block lasts.
const LOGIN_BLOCK_SECS: u64 = 300;
/// A login nobody finished (browser closed mid-flow) is dropped after this
/// long, together with its throwaway session file.
const LOGIN_TTL_SECS: u64 = 30 * 60;
/// A session file plus the siblings SQLite keeps next to it.
const SESSION_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];
/// Moving a session file can lose a race against a request that still holds
/// a client of the old connection; retry briefly instead of failing.
const MOVE_ATTEMPTS: u32 = 10;

/// Where a sign-in stands.
pub enum LoginStep {
    /// Signed in as this account; its manager is registered and ready.
    Done(i64),
    PasswordRequired { hint: Option<String> },
}

/// Brute-force gate for code and password submissions. Hub-wide, exactly as
/// it was process-wide before several accounts existed: counting per pending
/// login would let an attacker clear the counter by asking for a new code.
#[derive(Default)]
struct Throttle {
    failed: u32,
    blocked_until: Option<Instant>,
}

/// One login in flight. The flow sits behind its own lock so concurrent
/// logins never queue behind each other; the start time stays outside it so
/// abandoned logins can be expired without taking that lock.
struct Login {
    started: Instant,
    pending: Mutex<Pending>,
}

/// Every Telegram account this process serves, plus the logins that are
/// still trying to become one.
pub struct TgHub {
    cfg: Config,
    /// Directory holding one session file per account.
    dir: PathBuf,
    users: Mutex<HashMap<i64, Arc<TgManager>>>,
    logins: Mutex<HashMap<String, Arc<Login>>>,
    throttle: Mutex<Throttle>,
}

impl TgHub {
    pub fn new(cfg: Config) -> Self {
        TgHub {
            dir: sessions_dir(&cfg.session_path),
            cfg,
            users: Mutex::new(HashMap::new()),
            logins: Mutex::new(HashMap::new()),
            throttle: Mutex::new(Throttle::default()),
        }
    }

    /// Rebuilds a manager for every account with a session file on disk and
    /// returns the ones that are live. Session files Telegram rejects are
    /// deleted; leftovers from logins interrupted by a restart too.
    pub async fn restore(&self) -> Vec<i64> {
        let (uids, abandoned) = self.scan_sessions().await;
        for path in abandoned {
            tracing::info!(?path, "removing a session file left by an unfinished login");
            let _ = remove_session(&path).await;
        }

        let known: Vec<i64> = {
            let users = self.users.lock().await;
            uids.into_iter().filter(|u| !users.contains_key(u)).collect()
        };
        // Each check is a network round trip; run them together so boot time
        // does not grow with the number of accounts.
        let checked = futures::future::join_all(known.into_iter().map(|uid| async move {
            let manager = Arc::new(TgManager::new(
                self.cfg.clone(),
                path_string(&self.user_session(uid)),
                uid,
            ));
            let status = manager.status().await;
            (uid, manager, status)
        }))
        .await;

        let mut live = Vec::new();
        for (uid, manager, status) in checked {
            if !status.authorized && (status.connected || status.relogin) {
                // Telegram answered, or answered with an auth error: either
                // way it disowned the key, so the file is worthless.
                tracing::warn!(user_id = uid, "stored session is no longer authorized");
                manager.close().await;
                let _ = remove_session(&self.user_session(uid)).await;
                continue;
            }
            if !status.connected {
                // Unreachable, not rejected. Keeping the account beats
                // deleting a good session because the network blinked.
                tracing::warn!(
                    user_id = uid,
                    error = ?status.error,
                    "could not verify stored session; keeping it"
                );
            }
            self.users.lock().await.insert(uid, manager);
            live.push(uid);
        }
        tracing::info!(accounts = live.len(), "telegram sessions restored");
        live
    }

    /// Migrates the single pre-multi-tenant session file, if it is still
    /// there, into the per-account layout. Safe on every boot: the file is
    /// only moved once Telegram has named the account behind it.
    pub async fn adopt_legacy(&self) -> Option<i64> {
        let legacy = PathBuf::from(&self.cfg.session_path);
        if !tokio::fs::try_exists(&legacy).await.unwrap_or(false) {
            return None;
        }
        let manager = TgManager::new(
            self.cfg.clone(),
            self.cfg.session_path.clone(),
            super::UNKNOWN_USER,
        );
        let status = manager.status().await;
        let uid = match status.user {
            Some(user) if status.authorized => user.id,
            _ => {
                manager.close().await;
                if status.connected || status.relogin {
                    // Telegram disowned the key: keeping the file would only
                    // make every boot probe a dead session.
                    tracing::warn!("legacy telegram session is dead; removing it");
                    let _ = remove_session(&legacy).await;
                } else {
                    tracing::warn!(
                        error = ?status.error,
                        "legacy telegram session could not be verified; leaving it in place"
                    );
                }
                return None;
            }
        };
        manager.close().await;

        if self.users.lock().await.contains_key(&uid) {
            // Already migrated on an earlier boot and restored from the new
            // layout; the legacy copy is a duplicate key for the same
            // account, so dropping it loses nothing.
            tracing::info!(user_id = uid, "legacy session already migrated; removing it");
            let _ = remove_session(&legacy).await;
            return Some(uid);
        }

        let dest = self.user_session(uid);
        if let Err(e) = move_session(&legacy, &dest).await {
            // The legacy file stays where it is, so the next boot retries.
            tracing::error!(user_id = uid, "cannot migrate legacy session: {e}");
            return None;
        }
        self.users.lock().await.insert(
            uid,
            Arc::new(TgManager::new(self.cfg.clone(), path_string(&dest), uid)),
        );
        tracing::info!(user_id = uid, "legacy telegram session adopted");
        Some(uid)
    }

    pub async fn get(&self, user_id: i64) -> Option<Arc<TgManager>> {
        self.users.lock().await.get(&user_id).cloned()
    }

    /// Requests a confirmation code and returns the handle the client must
    /// present to finish this login. Nothing else ties a browser to the
    /// flow, so the handle is a secret.
    pub async fn start_login(&self, phone: &str) -> Result<String, String> {
        self.prune_logins().await;
        let login_id = new_login_id();
        let path = self.dir.join(format!("pending-{login_id}.db"));
        let mut pending = Pending::new(self.cfg.clone(), path.clone());
        if let Err(e) = pending.send_code(phone).await {
            pending.manager.close().await;
            let _ = remove_session(&path).await;
            return Err(e);
        }
        self.logins.lock().await.insert(
            login_id.clone(),
            Arc::new(Login {
                started: Instant::now(),
                pending: Mutex::new(pending),
            }),
        );
        Ok(login_id)
    }

    pub async fn submit_code(&self, login_id: &str, code: &str) -> Result<LoginStep, String> {
        let login = self.login(login_id).await?;
        self.gate().await?;
        let mut pending = login.pending.lock().await;
        match pending.sign_in(code).await {
            Ok(CodeStep::Done(user_id)) => {
                self.claim(login_id, &pending, user_id).await?;
                Ok(LoginStep::Done(user_id))
            }
            Ok(CodeStep::PasswordRequired { hint }) => Ok(LoginStep::PasswordRequired { hint }),
            Err(e) => {
                if e.wrong_secret {
                    self.record_failure().await;
                }
                Err(e.message)
            }
        }
    }

    pub async fn submit_password(&self, login_id: &str, password: &str) -> Result<i64, String> {
        let login = self.login(login_id).await?;
        self.gate().await?;
        let mut pending = login.pending.lock().await;
        match pending.check_password(password).await {
            Ok(user_id) => {
                self.claim(login_id, &pending, user_id).await?;
                Ok(user_id)
            }
            Err(e) => {
                if e.wrong_secret {
                    self.record_failure().await;
                }
                Err(e.message)
            }
        }
    }

    /// Forgets an account: its connections stop and its session files go.
    /// Idempotent, so a client may sign out twice without seeing an error.
    pub async fn logout(&self, user_id: i64) -> Result<(), String> {
        if let Some(manager) = self.users.lock().await.remove(&user_id) {
            manager.close().await;
        }
        remove_session(&self.user_session(user_id)).await?;
        self.remove_bot_sessions(user_id).await;
        tracing::info!(user_id, "signed out of Telegram; session file deleted");
        Ok(())
    }

    /// Files a finished login's session under its account and puts a fresh
    /// manager in front of it. The throwaway session is closed first:
    /// an open SQLite file cannot be moved on Windows.
    async fn claim(&self, login_id: &str, pending: &Pending, user_id: i64) -> Result<(), String> {
        self.logins.lock().await.remove(login_id);
        self.record_success().await;
        pending.manager.close().await;

        if let Some(previous) = self.users.lock().await.remove(&user_id) {
            // Same account signing in again: the old session has to let go
            // of the destination file before it is overwritten.
            tracing::info!(user_id, "replacing the previous session of this account");
            previous.close().await;
        }
        let dest = self.user_session(user_id);
        move_session(&pending.session_path, &dest).await?;
        self.users.lock().await.insert(
            user_id,
            Arc::new(TgManager::new(
                self.cfg.clone(),
                path_string(&dest),
                user_id,
            )),
        );
        tracing::info!(user_id, "telegram account linked");
        Ok(())
    }

    async fn login(&self, login_id: &str) -> Result<Arc<Login>, String> {
        self.logins
            .lock()
            .await
            .get(login_id)
            .cloned()
            .ok_or_else(|| "this login expired — start again".to_string())
    }

    /// Rejects submissions while a brute-force block is active.
    async fn gate(&self) -> Result<(), String> {
        let mut throttle = self.throttle.lock().await;
        if let Some(until) = throttle.blocked_until {
            let now = Instant::now();
            if now < until {
                let secs = (until - now).as_secs() + 1;
                return Err(format!(
                    "too many failed login attempts; try again in {secs}s"
                ));
            }
            // Block has lapsed.
            throttle.blocked_until = None;
        }
        Ok(())
    }

    async fn record_failure(&self) {
        let mut throttle = self.throttle.lock().await;
        throttle.failed += 1;
        if throttle.failed >= MAX_LOGIN_ATTEMPTS {
            throttle.blocked_until = Some(Instant::now() + Duration::from_secs(LOGIN_BLOCK_SECS));
            throttle.failed = 0;
            tracing::warn!("login blocked for {LOGIN_BLOCK_SECS}s after repeated failures");
        }
    }

    async fn record_success(&self) {
        let mut throttle = self.throttle.lock().await;
        throttle.failed = 0;
        throttle.blocked_until = None;
    }

    /// Drops logins nobody finished, with their throwaway session files.
    async fn prune_logins(&self) {
        let ttl = Duration::from_secs(LOGIN_TTL_SECS);
        let stale: Vec<Arc<Login>> = {
            let mut logins = self.logins.lock().await;
            let ids: Vec<String> = logins
                .iter()
                .filter(|(_, login)| login.started.elapsed() > ttl)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| logins.remove(id)).collect()
        };
        for login in stale {
            let pending = login.pending.lock().await;
            pending.manager.close().await;
            let _ = remove_session(&pending.session_path).await;
            tracing::info!("dropped an abandoned login");
        }
    }

    /// Session files in the directory: the accounts they belong to, and the
    /// paths of files left behind by logins that never finished.
    async fn scan_sessions(&self) -> (Vec<i64>, Vec<PathBuf>) {
        let mut uids = Vec::new();
        let mut abandoned = Vec::new();
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (uids, abandoned),
            Err(e) => {
                tracing::error!("cannot read session directory {:?}: {e}", self.dir);
                return (uids, abandoned);
            }
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // The `-wal`/`-shm` siblings travel with their database.
            let Some(stem) = name.strip_suffix(".db") else {
                continue;
            };
            if stem.starts_with("pending-") {
                abandoned.push(entry.path());
            } else if let Ok(uid) = stem.parse::<i64>() {
                // Bot pool files (`<uid>_bot_<id>.db`) never parse as an id.
                if uid > 0 {
                    uids.push(uid);
                }
            }
        }
        (uids, abandoned)
    }

    /// Deletes the download bots' own session files for one account.
    async fn remove_bot_sessions(&self, user_id: i64) {
        let prefix = format!("{user_id}_bot_");
        let Ok(mut dir) = tokio::fs::read_dir(&self.dir).await else {
            return;
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                if let Err(e) = tokio::fs::remove_file(entry.path()).await {
                    tracing::warn!("cannot delete bot session {:?}: {e}", entry.path());
                }
            }
        }
    }

    fn user_session(&self, user_id: i64) -> PathBuf {
        self.dir.join(format!("{user_id}.db"))
    }
}

/// Per-account session files live in a `sessions/` directory beside the
/// configured session path, so the legacy single-user file can sit next to
/// them until it is adopted.
fn sessions_dir(session_path: &str) -> PathBuf {
    match Path::new(session_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        Some(parent) => parent.join("sessions"),
        None => PathBuf::from("sessions"),
    }
}

/// Unguessable handle for a login in flight; it is the only proof a client
/// has that the flow is theirs, so it is sized like a bearer secret.
fn new_login_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Deletes a session file and its SQLite siblings.
async fn remove_session(path: &Path) -> Result<(), String> {
    for suffix in SESSION_SUFFIXES {
        let p = with_suffix(path, suffix);
        match tokio::fs::remove_file(&p).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot delete session file {p:?}: {e}")),
        }
    }
    Ok(())
}

/// Moves a session file, siblings included, over whatever sits at the
/// destination — a leftover `-wal` there would be replayed into the moved
/// database and corrupt it. Both files must already be closed.
async fn move_session(from: &Path, to: &Path) -> Result<(), String> {
    // The per-account directory does not exist yet on an install being
    // upgraded from the single-session layout: this move is what creates
    // the first file in it.
    if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("cannot create session dir {parent:?}: {e}"))?;
    }
    remove_session(to).await?;
    for suffix in SESSION_SUFFIXES {
        let src = with_suffix(from, suffix);
        if !tokio::fs::try_exists(&src).await.unwrap_or(false) {
            continue;
        }
        let dst = with_suffix(to, suffix);
        for attempt in 1..=MOVE_ATTEMPTS {
            match tokio::fs::rename(&src, &dst).await {
                Ok(()) => break,
                Err(e) if attempt == MOVE_ATTEMPTS => {
                    return Err(format!("cannot move session {src:?} to {dst:?}: {e}"));
                }
                Err(_) => {
                    // A request still holding a client of the old connection
                    // keeps the file open; that window is milliseconds wide.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hub lives in the shared axum state, so it has to cross threads.
    #[test]
    fn hub_is_shareable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TgHub>();
    }

    #[test]
    fn sessions_dir_sits_beside_the_configured_session() {
        assert_eq!(
            sessions_dir("data/session.db"),
            PathBuf::from("data").join("sessions")
        );
        assert_eq!(sessions_dir("session.db"), PathBuf::from("sessions"));
    }

    #[test]
    fn login_ids_are_long_and_unique() {
        let a = new_login_id();
        let b = new_login_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn suffixes_extend_the_file_name() {
        assert_eq!(
            with_suffix(Path::new("a/7.db"), "-wal"),
            PathBuf::from("a/7.db-wal")
        );
    }

    fn hub(dir: &Path) -> TgHub {
        let mut cfg = Config::default();
        cfg.session_path = path_string(&dir.join("session.db"));
        TgHub::new(cfg)
    }

    #[tokio::test]
    async fn moving_a_session_clears_a_stale_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("pending-abc.db");
        let to = dir.path().join("42.db");
        tokio::fs::write(&from, b"new").await.unwrap();
        tokio::fs::write(&to, b"old").await.unwrap();
        // A write-ahead log of the session being replaced: replaying it into
        // the new database would corrupt it, so it must go.
        tokio::fs::write(with_suffix(&to, "-wal"), b"stale")
            .await
            .unwrap();

        move_session(&from, &to).await.unwrap();

        assert_eq!(tokio::fs::read(&to).await.unwrap(), b"new");
        assert!(!from.exists());
        assert!(!with_suffix(&to, "-wal").exists());
    }

    /// Upgrading a single-session install moves the old session into a
    /// per-account directory that has never existed. The move has to create
    /// it, or the legacy account is stranded and its files stay unowned.
    #[tokio::test]
    async fn moving_a_session_creates_the_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("session.db");
        let to = dir.path().join("sessions").join("42.db");
        tokio::fs::write(&from, b"legacy").await.unwrap();

        move_session(&from, &to).await.expect("move creates the dir");

        assert_eq!(tokio::fs::read(&to).await.unwrap(), b"legacy");
        assert!(!from.exists());
    }

    #[tokio::test]
    async fn scan_separates_accounts_from_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        tokio::fs::create_dir_all(&hub.dir).await.unwrap();
        for name in [
            "7.db",
            "7.db-wal",
            "12.db",
            "7_bot_555.db",
            "pending-deadbeef.db",
            "notes.txt",
        ] {
            tokio::fs::write(hub.dir.join(name), b"x").await.unwrap();
        }

        let (mut uids, abandoned) = hub.scan_sessions().await;
        uids.sort_unstable();
        assert_eq!(uids, vec![7, 12]);
        assert_eq!(abandoned, vec![hub.dir.join("pending-deadbeef.db")]);
    }

    #[tokio::test]
    async fn abandoned_logins_and_their_files_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        tokio::fs::create_dir_all(&hub.dir).await.unwrap();
        let stale = hub.dir.join("pending-stale.db");
        let fresh = hub.dir.join("pending-fresh.db");
        tokio::fs::write(&stale, b"x").await.unwrap();
        tokio::fs::write(&fresh, b"x").await.unwrap();
        {
            let mut logins = hub.logins.lock().await;
            logins.insert(
                "stale".to_string(),
                Arc::new(Login {
                    started: Instant::now() - Duration::from_secs(LOGIN_TTL_SECS + 1),
                    pending: Mutex::new(Pending::new(hub.cfg.clone(), stale.clone())),
                }),
            );
            logins.insert(
                "fresh".to_string(),
                Arc::new(Login {
                    started: Instant::now(),
                    pending: Mutex::new(Pending::new(hub.cfg.clone(), fresh.clone())),
                }),
            );
        }

        hub.prune_logins().await;

        let logins = hub.logins.lock().await;
        assert_eq!(logins.keys().collect::<Vec<_>>(), vec!["fresh"]);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[tokio::test]
    async fn unknown_login_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        assert!(hub.submit_code("nope", "12345").await.is_err());
        assert!(hub.submit_password("nope", "hunter2").await.is_err());
    }
}
