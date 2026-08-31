use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::SessionKind;

use super::TgManager;
use super::login::{CodeStep, Pending};

/// Failed code/password attempts before login is temporarily blocked.
const MAX_LOGIN_ATTEMPTS: u32 = 5;
/// How long the block lasts.
const LOGIN_BLOCK_SECS: u64 = 300;
/// A login nobody finished (browser closed mid-flow) is dropped after this
/// long, together with its throwaway session row.
const LOGIN_TTL_SECS: u64 = 30 * 60;
/// A number may be sent one login code per this window. Without it an
/// allowlisted number can be made to receive unlimited Telegram codes,
/// which both spams whoever owns it and burns the account's `FLOOD_WAIT`
/// budget on attacker-chosen traffic.
const CODE_RESEND_COOLDOWN_SECS: u64 = 60;
/// How often abandoned logins are swept up.
const PRUNE_INTERVAL_SECS: u64 = 60;

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
        Self {
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
    /// Embedded store holding every Telegram session row. Clones share
    /// the router, so this handle is live once `db::connect` has wired
    /// the process-wide one it was cloned from.
    db: super::Db,
    /// Namespace attach for `db`, run once on the first store access:
    /// namespace selection is per-`Surreal`-clone session state, so the
    /// clone has to select it for itself.
    attached: tokio::sync::OnceCell<()>,
    users: Mutex<HashMap<i64, Arc<TgManager>>>,
    logins: Arc<LoginMap>,
    /// One entry per normalized phone number.
    throttles: Mutex<HashMap<String, Throttle>>,
}

impl TgHub {
    /// Must be called from inside a Tokio runtime: the hub owns a pruning
    /// task from birth.
    pub fn new(cfg: Config, db: super::Db) -> Self {
        let logins: Arc<LoginMap> = Arc::new(Mutex::new(HashMap::new()));
        // Pruning has to run on a timer rather than off `start_login`: an
        // abandoned login otherwise keeps an open MTProto client alive for
        // the whole process lifetime whenever nobody ever starts another
        // login. The task holds only a `Weak`, so it ends by itself when
        // the hub is dropped instead of outliving it or needing a handle
        // somebody has to remember to cancel.
        tokio::spawn(prune_loop(
            Arc::downgrade(&logins),
            Duration::from_secs(PRUNE_INTERVAL_SECS),
        ));
        Self {
            cfg,
            db,
            attached: tokio::sync::OnceCell::const_new(),
            users: Mutex::new(HashMap::new()),
            logins,
            throttles: Mutex::new(HashMap::new()),
        }
    }

    /// Points the hub's handle (and, through [`TgManager::open_conn`],
    /// every manager's clone) at the app namespace. Required once before
    /// the first store access: namespace selection is session state, and
    /// this handle is its own session. Idempotent.
    pub async fn attach_session(&self) -> Result<(), String> {
        let () = self
            .attached
            .get_or_try_init(|| async {
                crate::db::attach_session(&self.db)
                    .await
                    .map_err(|e| format!("cannot attach telegram session store: {e}"))
            })
            .await?;
        Ok(())
    }

    /// Rebuilds a manager for every account with a session row in the
    /// store and returns the ones that are live. Sessions Telegram
    /// rejects are deleted; leftovers from logins interrupted by a
    /// restart too.
    pub async fn restore(&self) -> Vec<i64> {
        if let Err(e) = self.attach_session().await {
            tracing::error!("{e}");
            return Vec::new();
        }
        if let Err(e) = migrate_file_sessions(&self.db, &self.cfg.session_path).await {
            tracing::warn!("session file migration failed: {e}");
        }

        // Rows of logins that a restart interrupted: nothing can ever
        // claim them again.
        match crate::db::list_keys(&self.db, SessionKind::Pending).await {
            Ok(keys) => {
                for key in keys {
                    tracing::info!(%key, "removing a session row left by an unfinished login");
                    if let Err(e) = crate::db::delete_session(&self.db, &key).await {
                        tracing::warn!("cannot delete session row {key}: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("cannot list pending session rows: {e}"),
        }

        let keys = match crate::db::list_keys(&self.db, SessionKind::Account).await {
            Ok(keys) => keys,
            Err(e) => {
                tracing::error!("cannot list session rows: {e}");
                return Vec::new();
            }
        };
        // Each check is a network round trip; run them together so boot time
        // does not grow with the number of accounts.
        let checked = futures::future::join_all(keys.into_iter().map(|key| async move {
            let uid: i64 = key.strip_prefix("user-").and_then(|s| s.parse().ok()).unwrap_or(0);
            let manager = Arc::new(TgManager::new(
                self.cfg.clone(),
                self.db.clone(),
                key,
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
                // way it disowned the key, so the row is worthless.
                tracing::warn!(user_id = uid, "stored session is no longer authorized");
                manager.close().await;
                let _ = crate::db::delete_session(&self.db, manager.session_key()).await;
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

    pub async fn get(&self, user_id: i64) -> Option<Arc<TgManager>> {
        self.users.lock().await.get(&user_id).cloned()
    }

    /// Requests a confirmation code and returns the handle the client must
    /// present to finish this login. Nothing else ties a browser to the
    /// flow, so the handle is a secret.
    pub async fn start_login(&self, phone: &str) -> Result<String, String> {
        self.attach_session().await?;
        // Asking Telegram to send a code is as much of an attack surface as
        // submitting one: it is the step that actually reaches the owner of
        // the number.
        let key = crate::config::normalize_phone(phone);
        self.gate(&key).await?;
        self.reserve_code_send(&key).await?;
        let login_id = new_login_id();
        let session_key = format!("pending-{login_id}");
        let mut pending = Pending::new(self.cfg.clone(), self.db.clone(), session_key);
        if let Err(e) = pending.send_code(phone).await {
            pending.manager.close().await;
            let _ = crate::db::delete_session(&self.db, &pending.session_key).await;
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

    /// Forgets an account: its connections stop and its session rows go.
    /// Idempotent, so a client may sign out twice without seeing an error.
    pub async fn logout(&self, user_id: i64) -> Result<(), String> {
        self.attach_session().await?;
        let manager = self.users.lock().await.remove(&user_id);
        if let Some(manager) = manager {
            manager.close().await;
        }
        crate::db::delete_session(&self.db, &format!("user-{user_id}"))
            .await
            .map_err(|e| format!("cannot delete session row: {e}"))?;
        if let Err(e) =
            crate::db::delete_sessions_of(&self.db, SessionKind::Bot, user_id).await
        {
            tracing::warn!("cannot delete bot session rows of {user_id}: {e}");
        }
        tracing::info!(user_id, "signed out of Telegram; session rows deleted");
        Ok(())
    }

    /// Files a finished login's session under its account and puts a fresh
    /// manager in front of it. The throwaway row is re-keyed in place —
    /// an upsert, not a file move — so two logins finishing for the same
    /// account cannot interleave their way into a lost session: the last
    /// completed sign-in wins the row, as it must.
    async fn claim(
        &self,
        login_id: &str,
        phone: &str,
        pending: &Pending,
        user_id: i64,
    ) -> Result<(), String> {
        self.attach_session().await?;
        self.logins.lock().await.remove(login_id);
        self.record_success(phone).await;
        pending.manager.close().await;
        let previous = self.users.lock().await.remove(&user_id);
        if let Some(previous) = previous {
            // Same account signing in again: its old session is stale now.
            tracing::info!(user_id, "replacing the previous session of this account");
            previous.close().await;
        }
        let blob = crate::db::read_session(&self.db, &pending.session_key)
            .await
            .map_err(|e| format!("cannot read login session: {e}"))?
            .ok_or_else(|| "the login session vanished before it could be filed".to_string())?;
        crate::db::write_session(
            &self.db,
            &format!("user-{user_id}"),
            SessionKind::Account,
            user_id,
            &blob,
        )
        .await
        .map_err(|e| format!("cannot file the login session: {e}"))?;
        crate::db::delete_session(&self.db, &pending.session_key)
            .await
            .map_err(|e| format!("cannot clear the throwaway login session: {e}"))?;
        self.users.lock().await.insert(
            user_id,
            Arc::new(TgManager::new(
                self.cfg.clone(),
                self.db.clone(),
                format!("user-{user_id}"),
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

    #[allow(clippy::arithmetic_side_effects)] // secs rounding on a bounded u64 cannot overflow
    #[allow(clippy::significant_drop_tightening)] // `throttles` guard lives for the whole fn; entry borrows from it
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
    #[allow(clippy::arithmetic_side_effects)] // secs rounding on a bounded u64 cannot overflow
    #[allow(clippy::significant_drop_tightening)] // `throttles` guard lives for the whole fn; entry borrows from it
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
                let secs = cooldown.saturating_sub(elapsed).as_secs() + 1;
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

    #[allow(clippy::arithmetic_side_effects)] // failure counter bounded at MAX_LOGIN_ATTEMPTS; touch+offset cannot overflow
    #[allow(clippy::significant_drop_tightening)] // `throttles` guard lives for the whole fn; entry borrows from it
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

    /// Session rows in the store: the account user ids, and the keys of
    /// rows left behind by logins that never finished.
    #[cfg(test)]
    async fn scan_sessions(&self) -> (Vec<i64>, Vec<String>) {
        let mut uids = Vec::new();
        for key in crate::db::list_keys(&self.db, SessionKind::Account)
            .await
            .unwrap_or_default()
        {
            if let Ok(uid) = key.strip_prefix("user-").unwrap_or("").parse::<i64>() {
                if uid > 0 {
                    uids.push(uid);
                }
            }
        }
        uids.sort_unstable();
        let mut abandoned = crate::db::list_keys(&self.db, SessionKind::Pending)
            .await
            .unwrap_or_default();
        abandoned.sort_unstable();
        (uids, abandoned)
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

#[allow(clippy::significant_drop_tightening)] // tokio Mutex guards held across .await; entry/pending borrows require it
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
        if let Err(e) = crate::db::delete_session(&pending.manager.db, &pending.session_key).await
        {
            tracing::warn!("cannot delete the throwaway session row: {e}");
        }
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

/// Unguessable handle for a login in flight; it is the only proof a client
/// has that the flow is theirs, so it is sized like a bearer secret.
fn new_login_id() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

/// One-time import of the file-era session store: `sessions/*.db` blobs
/// move into `tg_session` rows, then the whole directory is set aside so
/// a failed import can be retried and no file is ever trusted twice.
///
/// The directory is the one the configured session path implies — the
/// location the previous storage derived from it.
async fn migrate_file_sessions(db: &super::Db, session_path: &str) -> Result<(), String> {
    let Some(dir) = Path::new(session_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|parent| parent.join("sessions"))
    else {
        return Ok(());
    };
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read session directory {}: {e}", dir.display())),
    };

    // A non-empty store already owns the sessions; the files are leftovers
    // of an earlier import, not a second account universe.
    let known = crate::db::list_keys(db, SessionKind::Account)
        .await
        .map_err(|e| format!("cannot list session rows: {e}"))?;
    if !known.is_empty() {
        return Ok(());
    }

    let mut imported = 0usize;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("cannot walk session directory {}: {e}", dir.display()))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".db") else {
            continue;
        };
        if stem.starts_with("pending-") {
            // A login interrupted mid-import-era: worthless now. It goes
            // with the directory when the import sets it aside.
            continue;
        }
        let Ok(uid) = stem.parse::<i64>() else {
            continue; // bot sessions and strays; they go with the directory
        };
        if uid <= 0 {
            continue;
        }
        let blob = tokio::fs::read_to_string(entry.path())
            .await
            .map_err(|e| format!("cannot read session file {}: {e}", entry.path().display()))?;
        // Sanity-check the blob before importing: an unparseable file
        // (grammers SQLite leftovers) must not become a session row.
        if serde_json::from_str::<mtprsto::session::SessionData>(&blob).is_err() {
            tracing::warn!(
                file = %entry.path().display(),
                "unparseable old session file skipped"
            );
            continue;
        }
        crate::db::write_session(db, &format!("user-{uid}"), SessionKind::Account, uid, &blob)
            .await
            .map_err(|e| format!("cannot import session {uid}: {e}"))?;
        // Bounded by the directory's entry count — cannot overflow.
        imported = imported.saturating_add(1);
    }

    if imported > 0 {
        tracing::info!(imported, "imported file-era Telegram sessions");
    }
    // Set the whole directory aside — imported files, unparseable ones,
    // bot sessions and interrupted logins alike. Nothing reads it again.
    let aside = dir.with_extension("imported");
    for attempt in 1..=10 {
        match tokio::fs::rename(&dir, &aside).await {
            Ok(()) => break,
            Err(e) if attempt == 10 => {
                tracing::warn!(
                    "cannot move {} aside after import ({}); imported files stay in place",
                    dir.display(), e
                );
            }
            Err(_) => {
                // A still-live client of the previous storage keeps a file
                // open on Windows; the window is milliseconds wide.
                tokio::time::sleep(Duration::from_millis(100)).await;
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
    fn login_ids_are_long_and_unique() {
        let a = new_login_id();
        let b = new_login_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    async fn scratch_db() -> super::super::Db {
        let db = surrealdb::Surreal::init();
        crate::db::connect_mem(&db).await.unwrap();
        db
    }

    async fn hub(dir: &Path) -> TgHub {
        let db = scratch_db().await;
        TgHub::new(
            Config {
                session_path: dir.join("session.db").to_string_lossy().into_owned(),
                ..Config::default()
            },
            db,
        )
    }

    /// The pending-rows listing is the whole scan now: accounts and
    /// throwaway login rows separate by kind, not by filename shape.
    #[tokio::test]
    async fn scan_separates_accounts_from_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path()).await;
        crate::db::write_session(&hub.db, "user-7", SessionKind::Account, 7, "a").await.unwrap();
        crate::db::write_session(&hub.db, "user-12", SessionKind::Account, 12, "b").await.unwrap();
        crate::db::write_session(&hub.db, "user-7-bot-555", SessionKind::Bot, 7, "c").await.unwrap();
        crate::db::write_session(&hub.db, "pending-deadbeef", SessionKind::Pending, 0, "d").await.unwrap();

        let (uids, abandoned) = hub.scan_sessions().await;
        assert_eq!(uids, vec![7, 12]);
        assert_eq!(abandoned, vec!["pending-deadbeef".to_string()]);
    }

    fn pending_login(hub: &TgHub, phone: &str, started: Instant, key: String) -> Arc<Login> {
        Arc::new(Login {
            started,
            phone: phone.to_string(),
            pending: Mutex::new(Pending::new(hub.cfg.clone(), hub.db.clone(), key)),
        })
    }

    #[tokio::test]
    async fn abandoned_logins_and_their_rows_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path()).await;
        crate::db::write_session(&hub.db, "pending-stale", SessionKind::Pending, 0, "x").await.unwrap();
        crate::db::write_session(&hub.db, "pending-fresh", SessionKind::Pending, 0, "y").await.unwrap();
        {
            let mut logins = hub.logins.lock().await;
            logins.insert(
                "stale".to_string(),
                pending_login(
                    &hub,
                    "15550102030",
                    Instant::now() - Duration::from_secs(LOGIN_TTL_SECS + 1),
                    "pending-stale".to_string(),
                ),
            );
            logins.insert(
                "fresh".to_string(),
                pending_login(&hub, "15550102031", Instant::now(), "pending-fresh".to_string()),
            );
        }

        prune_stale_logins(&hub.logins).await;

        let logins = hub.logins.lock().await;
        assert_eq!(logins.keys().collect::<Vec<_>>(), vec!["fresh"]);
        assert_eq!(
            crate::db::read_session(&hub.db, "pending-stale").await.unwrap(),
            None
        );
        assert_eq!(
            crate::db::read_session(&hub.db, "pending-fresh").await.unwrap().as_deref(),
            Some("y")
        );
    }

    /// The pruner has to reclaim an abandoned login on its own: before it ran
    /// on a timer, a leftover client and its session row survived for the
    /// whole process lifetime unless somebody started another login.
    #[tokio::test]
    async fn the_pruner_reclaims_abandoned_logins_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path()).await;
        crate::db::write_session(&hub.db, "pending-stale", SessionKind::Pending, 0, "x").await.unwrap();
        hub.logins.lock().await.insert(
            "stale".to_string(),
            pending_login(
                &hub,
                "15550102030",
                Instant::now() - Duration::from_secs(LOGIN_TTL_SECS + 1),
                "pending-stale".to_string(),
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
        assert_eq!(
            crate::db::read_session(&hub.db, "pending-stale").await.unwrap(),
            None
        );

        // The task belongs to the hub, so it must end with it rather than
        // ticking on for the rest of the process.
        drop(hub);
        tokio::time::timeout(Duration::from_secs(5), pruner)
            .await
            .expect("the pruner stops once the hub is gone")
            .expect("the pruner did not panic");
    }

    /// Two logins finishing for the same account used to interleave file
    /// moves and could leave nothing behind at all. With rows there is no
    /// window: every claim re-keys its own row, the last completed sign-in
    /// wins, and a manager is always in front of the account.
    #[tokio::test]
    async fn concurrent_claims_for_one_account_file_exactly_one_session() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(hub(dir.path()).await);
        let uid = 77;

        let claims: Vec<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|tag| {
                let hub = Arc::clone(&hub);
                tokio::spawn(async move {
                    let key = format!("pending-{tag}");
                    crate::db::write_session(&hub.db, &key, SessionKind::Pending, 0, tag)
                        .await
                        .unwrap();
                    let pending = Pending::new(hub.cfg.clone(), hub.db.clone(), key);
                    hub.claim(tag, "15550102030", &pending, uid).await
                })
            })
            .collect();
        for claim in claims {
            claim.await.unwrap().expect("every claim completes");
        }

        let filed = crate::db::read_session(&hub.db, &format!("user-{uid}"))
            .await
            .expect("the account has a session row");
        assert!(filed.is_some(), "a session survives every racing claim");
        assert!(
            hub.get(uid).await.is_some(),
            "and a manager is in front of it"
        );
        assert_eq!(
            hub.scan_sessions().await.1,
            Vec::<String>::new(),
            "no throwaway session row is left behind"
        );
    }

    /// The throttle used to be hub-wide, so five wrong codes from anybody
    /// locked every account out of signing in.
    #[tokio::test]
    async fn failed_attempts_only_block_the_number_that_failed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path()).await;
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
        let hub = hub(dir.path()).await;
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
        let hub = hub(dir.path()).await;
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
        let hub = hub(dir.path()).await;
        assert!(hub.submit_code("nope", "12345").await.is_err());
        assert!(hub.submit_password("nope", "hunter2").await.is_err());
    }

    /// File-era sessions migrate into rows on restore, and the source
    /// directory is set aside so nothing is imported twice.
    #[tokio::test]
    async fn file_sessions_migrate_once() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        tokio::fs::create_dir_all(&sessions).await.unwrap();
        let blob = serde_json::to_string(&mtprsto::session::SessionData::from_auth_key(
            &[7u8; 256],
            0,
            2,
        ))
        .unwrap();
        tokio::fs::write(sessions.join("42.db"), &blob).await.unwrap();
        tokio::fs::write(sessions.join("7.db"), b"not a session").await.unwrap();
        tokio::fs::write(sessions.join("pending-old.db"), b"x").await.unwrap();
        tokio::fs::write(sessions.join("42_bot_9.db"), b"x").await.unwrap();

        let db = scratch_db().await;
        let cfg = Config {
            session_path: dir.path().join("session.db").to_string_lossy().into_owned(),
            ..Config::default()
        };
        migrate_file_sessions(&db, &cfg.session_path).await.unwrap();

        assert_eq!(
            crate::db::read_session(&db, "user-42").await.unwrap().as_deref(),
            Some(blob.as_str())
        );
        assert_eq!(
            crate::db::list_keys(&db, SessionKind::Account).await.unwrap().len(),
            1,
            "only the parseable account session is imported"
        );
        assert!(
            !sessions.exists(),
            "the imported directory is moved aside"
        );
        assert!(dir.path().join("sessions.imported").exists());

        // A second restore is a no-op: rows exist, no re-import, and the
        // aside directory does not come back.
        migrate_file_sessions(&db, &cfg.session_path).await.unwrap();
        assert_eq!(
            crate::db::list_keys(&db, SessionKind::Account).await.unwrap().len(),
            1
        );
    }
}
