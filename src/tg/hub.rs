use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
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
/// A number may be sent one login code per this window. Without it an
/// allowlisted number can be made to receive unlimited Telegram codes,
/// which both spams whoever owns it and burns the account's FLOOD_WAIT
/// budget on attacker-chosen traffic.
const CODE_RESEND_COOLDOWN_SECS: u64 = 60;
/// How often abandoned logins are swept up.
const PRUNE_INTERVAL_SECS: u64 = 60;
/// A session file plus the siblings SQLite keeps next to it.
const SESSION_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];
/// Moving a session file can lose a race against a request that still holds
/// a client of the old connection; retry briefly instead of failing.
const MOVE_ATTEMPTS: u32 = 10;

/// Where a sign-in stands.
pub enum LoginStep {
    /// Signed in as this account; its manager is registered and ready.
    Done(i64),
    PasswordRequired {
        hint: Option<String>,
    },
}

/// Brute-force gate and code-resend state for one phone number. Keyed by
/// number rather than kept hub-wide: one hub-wide counter meant five wrong
/// codes from anybody locked every account out of signing in. Counting per
/// pending login would be the opposite mistake — an attacker would clear the
/// counter by asking for a new code — so the number, which is what the
/// secrets belong to, is the key.
struct Throttle {
    failed: u32,
    blocked_until: Option<Instant>,
    /// When Telegram was last asked to send a code to this number.
    last_code_sent: Option<Instant>,
    /// Last change to this entry, so numbers nobody is trying any more are
    /// forgotten instead of growing the map once per number ever probed.
    touched: Instant,
}

impl Throttle {
    fn new() -> Self {
        Throttle {
            failed: 0,
            blocked_until: None,
            last_code_sent: None,
            touched: Instant::now(),
        }
    }
}

/// One login in flight. The flow sits behind its own lock so concurrent
/// logins never queue behind each other; the start time stays outside it so
/// abandoned logins can be expired without taking that lock.
struct Login {
    started: Instant,
    /// Normalized number this login is for; it is the throttle key, and the
    /// flow itself keeps its own copy behind the `pending` lock.
    phone: String,
    pending: Mutex<Pending>,
}

/// The logins map is shared with the pruning task, which holds only a `Weak`
/// to it so it stops on its own once the hub is dropped.
type LoginMap = Mutex<HashMap<String, Arc<Login>>>;

/// Every Telegram account this process serves, plus the logins that are
/// still trying to become one.
pub struct TgHub {
    cfg: Config,
    /// Directory holding one session file per account.
    dir: PathBuf,
    users: Mutex<HashMap<i64, Arc<TgManager>>>,
    logins: Arc<LoginMap>,
    /// One entry per normalized phone number.
    throttles: Mutex<HashMap<String, Throttle>>,
    /// Serializes the close+move+insert section of [`TgHub::claim`]. Two
    /// logins for the same account hold two different `pending` locks, so
    /// without this both reach `move_session` for the same `<uid>.db`; that
    /// move wipes its destination first, so the loser would delete the
    /// winner's freshly filed session and leave the registered manager
    /// pointing at a file that no longer exists — an account that looks
    /// signed in with no session behind it. One hub-wide mutex rather than a
    /// per-account guard map: a claim happens once at the end of a sign-in
    /// and only renames a few files, so cross-account contention is
    /// irrelevant next to a map that would need its own reaping.
    claim_lock: Mutex<()>,
}

impl TgHub {
    /// Must be called from inside a Tokio runtime: the hub owns a pruning
    /// task from birth.
    pub fn new(cfg: Config) -> Self {
        let logins: Arc<LoginMap> = Arc::new(Mutex::new(HashMap::new()));
        // Pruning has to run on a timer rather than off `start_login`: an
        // abandoned login otherwise keeps an open MTProto client and its
        // `pending-*.db` alive for the whole process lifetime whenever
        // nobody ever starts another login. The task holds only a `Weak`,
        // so it ends by itself when the hub is dropped instead of
        // outliving it or needing a handle somebody has to remember to
        // cancel.
        tokio::spawn(prune_loop(
            Arc::downgrade(&logins),
            Duration::from_secs(PRUNE_INTERVAL_SECS),
        ));
        TgHub {
            dir: sessions_dir(&cfg.session_path),
            cfg,
            users: Mutex::new(HashMap::new()),
            logins,
            throttles: Mutex::new(HashMap::new()),
            claim_lock: Mutex::new(()),
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
            uids.into_iter()
                .filter(|u| !users.contains_key(u))
                .collect()
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
            tracing::info!(
                user_id = uid,
                "legacy session already migrated; removing it"
            );
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
        // Asking Telegram to send a code is as much of an attack surface as
        // submitting one: it is the step that actually reaches the owner of
        // the number.
        let key = crate::config::normalize_phone(phone);
        self.gate(&key).await?;
        self.reserve_code_send(&key).await?;
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
                phone: key,
                pending: Mutex::new(pending),
            }),
        );
        Ok(login_id)
    }

    pub async fn submit_code(&self, login_id: &str, code: &str) -> Result<LoginStep, String> {
        let login = self.login(login_id).await?;
        self.gate(&login.phone).await?;
        let mut pending = login.pending.lock().await;
        match pending.sign_in(code).await {
            Ok(CodeStep::Done(user_id)) => {
                self.claim(login_id, &login.phone, &pending, user_id)
                    .await?;
                Ok(LoginStep::Done(user_id))
            }
            Ok(CodeStep::PasswordRequired { hint }) => Ok(LoginStep::PasswordRequired { hint }),
            Err(e) => {
                if e.wrong_secret {
                    self.record_failure(&login.phone).await;
                }
                Err(e.message)
            }
        }
    }

    pub async fn submit_password(&self, login_id: &str, password: &str) -> Result<i64, String> {
        let login = self.login(login_id).await?;
        self.gate(&login.phone).await?;
        let mut pending = login.pending.lock().await;
        match pending.check_password(password).await {
            Ok(user_id) => {
                self.claim(login_id, &login.phone, &pending, user_id)
                    .await?;
                Ok(user_id)
            }
            Err(e) => {
                if e.wrong_secret {
                    self.record_failure(&login.phone).await;
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
    /// an open SQLite file cannot be moved on Windows. Everything from that
    /// close to the registration of the new manager runs under
    /// [`TgHub::claim_lock`], so two logins finishing for the same account
    /// cannot interleave their moves and lose the session file.
    async fn claim(
        &self,
        login_id: &str,
        phone: &str,
        pending: &Pending,
        user_id: i64,
    ) -> Result<(), String> {
        let _serialized = self.claim_lock.lock().await;
        self.logins.lock().await.remove(login_id);
        self.record_success(phone).await;
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

    /// Rejects submissions while this number's brute-force block is active.
    async fn gate(&self, phone: &str) -> Result<(), String> {
        let mut throttles = self.throttles.lock().await;
        let Some(entry) = throttles.get_mut(phone) else {
            return Ok(());
        };
        if let Some(until) = entry.blocked_until {
            let now = Instant::now();
            if now < until {
                let secs = (until - now).as_secs() + 1;
                return Err(format!(
                    "too many failed login attempts for this number; try again in {secs}s"
                ));
            }
            // Block has lapsed.
            entry.blocked_until = None;
            entry.touched = now;
        }
        Ok(())
    }

    /// Claims the right to have Telegram send a code to `phone`. The send is
    /// booked before it is attempted on purpose: two requests racing here
    /// must not both reach Telegram, and a send that failed is itself a
    /// reason to back off rather than to retry immediately.
    async fn reserve_code_send(&self, phone: &str) -> Result<(), String> {
        let cooldown = Duration::from_secs(CODE_RESEND_COOLDOWN_SECS);
        let mut throttles = self.throttles.lock().await;
        prune_throttles(&mut throttles);
        let entry = throttles
            .entry(phone.to_string())
            .or_insert_with(Throttle::new);
        if let Some(sent) = entry.last_code_sent {
            let elapsed = sent.elapsed();
            if elapsed < cooldown {
                let secs = (cooldown - elapsed).as_secs() + 1;
                return Err(format!(
                    "a login code was already sent to this number; wait {secs}s before asking for another"
                ));
            }
        }
        let now = Instant::now();
        entry.last_code_sent = Some(now);
        entry.touched = now;
        Ok(())
    }

    async fn record_failure(&self, phone: &str) {
        let mut throttles = self.throttles.lock().await;
        prune_throttles(&mut throttles);
        let entry = throttles
            .entry(phone.to_string())
            .or_insert_with(Throttle::new);
        entry.failed += 1;
        entry.touched = Instant::now();
        if entry.failed >= MAX_LOGIN_ATTEMPTS {
            entry.blocked_until = Some(entry.touched + Duration::from_secs(LOGIN_BLOCK_SECS));
            entry.failed = 0;
            tracing::warn!(
                "sign-in for this number blocked for {LOGIN_BLOCK_SECS}s after repeated failures"
            );
        }
    }

    async fn record_success(&self, phone: &str) {
        let mut throttles = self.throttles.lock().await;
        // The resend cooldown survives: a code was still sent to this number
        // however the login ended.
        if let Some(entry) = throttles.get_mut(phone) {
            entry.failed = 0;
            entry.blocked_until = None;
            entry.touched = Instant::now();
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
            if entry.file_name().to_string_lossy().starts_with(&prefix)
                && let Err(e) = tokio::fs::remove_file(entry.path()).await
            {
                tracing::warn!("cannot delete bot session {:?}: {e}", entry.path());
            }
        }
    }

    fn user_session(&self, user_id: i64) -> PathBuf {
        self.dir.join(format!("{user_id}.db"))
    }
}

/// Drops abandoned logins every `every`, and stops as soon as the hub that
/// owns the map is gone — the task is the hub's, so it must not outlive it.
async fn prune_loop(logins: Weak<LoginMap>, every: Duration) {
    let mut ticker = tokio::time::interval(every);
    // A tick missed while a prune was slow must not queue up a burst of
    // catch-up prunes behind it.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let Some(logins) = logins.upgrade() else {
            return;
        };
        prune_stale_logins(&logins).await;
    }
}

/// Drops logins nobody finished, with their throwaway session files.
async fn prune_stale_logins(logins: &LoginMap) {
    let ttl = Duration::from_secs(LOGIN_TTL_SECS);
    let stale: Vec<Arc<Login>> = {
        let mut logins = logins.lock().await;
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

/// Forgets numbers untouched for a whole block's length, so trying a fresh
/// number per request cannot grow the map without bound. A block is exactly
/// that long and resets the counter when it starts, so nothing an entry
/// still owes can survive its own expiry.
fn prune_throttles(throttles: &mut HashMap<String, Throttle>) {
    let ttl = Duration::from_secs(LOGIN_BLOCK_SECS);
    throttles.retain(|_, t| t.touched.elapsed() <= ttl);
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
        TgHub::new(Config {
            session_path: path_string(&dir.join("session.db")),
            ..Config::default()
        })
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

        move_session(&from, &to)
            .await
            .expect("move creates the dir");

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

    fn pending_login(hub: &TgHub, phone: &str, started: Instant, path: PathBuf) -> Arc<Login> {
        Arc::new(Login {
            started,
            phone: phone.to_string(),
            pending: Mutex::new(Pending::new(hub.cfg.clone(), path)),
        })
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
                pending_login(
                    &hub,
                    "15550102030",
                    Instant::now() - Duration::from_secs(LOGIN_TTL_SECS + 1),
                    stale.clone(),
                ),
            );
            logins.insert(
                "fresh".to_string(),
                pending_login(&hub, "15550102031", Instant::now(), fresh.clone()),
            );
        }

        prune_stale_logins(&hub.logins).await;

        let logins = hub.logins.lock().await;
        assert_eq!(logins.keys().collect::<Vec<_>>(), vec!["fresh"]);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    /// The pruner has to reclaim an abandoned login on its own: before it ran
    /// on a timer, a leftover client and its `pending-*.db` survived for the
    /// whole process lifetime unless somebody started another login.
    #[tokio::test]
    async fn the_pruner_reclaims_abandoned_logins_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        tokio::fs::create_dir_all(&hub.dir).await.unwrap();
        let stale = hub.dir.join("pending-stale.db");
        tokio::fs::write(&stale, b"x").await.unwrap();
        hub.logins.lock().await.insert(
            "stale".to_string(),
            pending_login(
                &hub,
                "15550102030",
                Instant::now() - Duration::from_secs(LOGIN_TTL_SECS + 1),
                stale.clone(),
            ),
        );

        let tick = Duration::from_millis(5);
        let pruner = tokio::spawn(prune_loop(Arc::downgrade(&hub.logins), tick));
        for _ in 0..200 {
            if hub.logins.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(tick).await;
        }
        assert!(
            hub.logins.lock().await.is_empty(),
            "the timer dropped the abandoned login without a new login arriving"
        );
        assert!(!stale.exists());

        // The task belongs to the hub, so it must end with it rather than
        // ticking on for the rest of the process.
        drop(hub);
        tokio::time::timeout(Duration::from_secs(5), pruner)
            .await
            .expect("the pruner stops once the hub is gone")
            .expect("the pruner did not panic");
    }

    /// Two logins finishing for the same account each hold only their own
    /// `pending` lock, so their `claim`s used to be free to interleave — and
    /// a move that wipes its destination first can then leave the account
    /// with a database from one login and a write-ahead log from another,
    /// or with nothing at all.
    #[tokio::test]
    async fn concurrent_claims_for_one_account_file_exactly_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(hub(dir.path()));
        tokio::fs::create_dir_all(&hub.dir).await.unwrap();
        let uid = 77;

        let claims: Vec<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|tag| {
                let hub = Arc::clone(&hub);
                tokio::spawn(async move {
                    let path = hub.dir.join(format!("pending-{tag}.db"));
                    tokio::fs::write(&path, tag.as_bytes()).await.unwrap();
                    // The log has to travel with its database; a pair from
                    // two different logins is the corruption `move_session`
                    // exists to prevent.
                    tokio::fs::write(with_suffix(&path, "-wal"), tag.as_bytes())
                        .await
                        .unwrap();
                    let pending = Pending::new(hub.cfg.clone(), path);
                    hub.claim(tag, "15550102030", &pending, uid).await
                })
            })
            .collect();
        for claim in claims {
            claim.await.unwrap().expect("every claim completes");
        }

        let dest = hub.user_session(uid);
        let filed = tokio::fs::read(&dest)
            .await
            .expect("the account still has a session file");
        let log = tokio::fs::read(with_suffix(&dest, "-wal"))
            .await
            .expect("with its write-ahead log");
        assert_eq!(filed, log, "the session and its log come from one login");
        assert!(
            hub.get(uid).await.is_some(),
            "and a manager is in front of it"
        );
        for tag in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            assert!(
                !hub.dir.join(format!("pending-{tag}.db")).exists(),
                "no throwaway session is left behind"
            );
        }
    }

    /// The throttle used to be hub-wide, so five wrong codes from anybody
    /// locked every account out of signing in.
    #[tokio::test]
    async fn failed_attempts_only_block_the_number_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let attacker = "15550109999";
        let victim = "15550102030";

        for _ in 0..MAX_LOGIN_ATTEMPTS - 1 {
            hub.record_failure(attacker).await;
        }
        hub.gate(attacker)
            .await
            .expect("below the limit nothing is blocked");
        hub.record_failure(attacker).await;

        let err = hub
            .gate(attacker)
            .await
            .expect_err("the number that failed is blocked");
        assert!(err.contains("too many failed login attempts"), "{err}");
        hub.gate(victim)
            .await
            .expect("another number is unaffected");
        hub.reserve_code_send(victim)
            .await
            .expect("and can still ask for a code");
    }

    #[tokio::test]
    async fn signing_in_clears_that_numbers_block() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let phone = "15550102030";
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            hub.record_failure(phone).await;
        }
        assert!(hub.gate(phone).await.is_err());

        hub.record_success(phone).await;

        hub.gate(phone)
            .await
            .expect("a successful sign-in lifts the block");
    }

    #[tokio::test]
    async fn a_second_code_for_one_number_waits_out_the_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        let phone = "15550102030";
        hub.reserve_code_send(phone).await.expect("first code");

        let err = hub
            .reserve_code_send(phone)
            .await
            .expect_err("a resend inside the cooldown is refused");
        assert!(
            err.contains(&format!("{CODE_RESEND_COOLDOWN_SECS}s")),
            "the refusal names how long to wait: {err}"
        );
        hub.reserve_code_send("15550109999")
            .await
            .expect("a different number is not held back");
    }

    #[tokio::test]
    async fn unknown_login_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path());
        assert!(hub.submit_code("nope", "12345").await.is_err());
        assert!(hub.submit_password("nope", "hunter2").await.is_err());
    }
}
