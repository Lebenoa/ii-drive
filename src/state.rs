use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use crate::auth::Tokens;
use crate::error::ApiError;
use crate::tg::{TgHub, TgManager};

/// Process-wide application state, reached through [`get`] rather than
/// threaded through axum's `State` extractor — the same shape
/// [`crate::config`] already uses. Handlers borrow it for `'static`, so
/// nothing is cloned per request and a detached background task can keep the
/// reference without an `Arc` of its own.
///
/// The database handle is created *unconnected* here and wired to the
/// configured store by [`crate::db::connect`] during startup. That is what
/// lets the state be a `LazyLock` at all: no step of this initializer is
/// async.
///
/// Lazy initialization imposes two ordering rules, both satisfied by `main`
/// touching the state directly after loading the config:
/// * [`crate::config::init`] must have run first — the token signer and the
///   Telegram hub capture their settings here and never re-read them, so a
///   later [`crate::config::reload`] leaves both alone (as it did before).
/// * first touch must be inside the tokio runtime, because [`TgHub::new`]
///   spawns the abandoned-login pruner.
static STATE: LazyLock<AppState> = LazyLock::new(|| {
    let cfg = crate::config::get();
    AppState {
        db: surrealdb::Surreal::init(),
        tokens: Tokens::new(&cfg.secret, cfg.token_ttl_secs),
        hub: TgHub::new(cfg),
        epochs: tokio::sync::RwLock::new(HashMap::new()),
    }
});

/// The process-wide state: a reference to an already-built static, so this
/// is free to call per request.
pub fn get() -> &'static AppState {
    &STATE
}

pub struct AppState {
    pub db: surrealdb::Surreal<surrealdb::engine::local::Db>,
    pub tokens: Tokens,
    /// Every signed-in Telegram account. Handlers never touch a client
    /// directly — they resolve their caller's account through [`Self::tg`].
    pub hub: TgHub,
    /// Per-account session-token epoch, read on every authenticated request.
    ///
    /// Load-through cache over `db::get_token_epoch`, and authoritative once
    /// an entry is loaded: every bump goes through [`Self::bump_epoch`] in
    /// this process, which writes the DB and the map together. The DB copy
    /// exists so a logout still holds after a restart, not to be re-read per
    /// request — a token check must not cost a query.
    ///
    /// `RwLock` because the read path (one lookup per request) vastly
    /// outnumbers the write path (one entry per login, one per logout).
    epochs: tokio::sync::RwLock<HashMap<i64, u64>>,
}

impl AppState {
    /// The Telegram account a request acts as.
    ///
    /// A token that survives [`Self::session_user`] still only proves who
    /// minted it: the account behind it can have gone away since (server
    /// restarted without that session file, Telegram revoked the session).
    /// Callers get 401 in that case so the web client re-authenticates
    /// instead of silently addressing somebody else's drive.
    pub async fn tg(&self, user_id: i64) -> Result<Arc<TgManager>, ApiError> {
        self.hub
            .get(user_id)
            .await
            .ok_or_else(|| ApiError::unauthorized("that Telegram session is no longer signed in"))
    }

    /// The account's current token epoch, loading it from the DB once.
    pub async fn epoch(&self, user_id: i64) -> Result<u64, ApiError> {
        if let Some(&e) = self.epochs.read().await.get(&user_id) {
            return Ok(e);
        }
        let stored = crate::db::get_token_epoch(&self.db, &user_id.to_string()).await?;
        // A concurrent bump may have landed while the DB read was in flight;
        // it is already both persisted and cached, so never overwrite it.
        let mut cache = self.epochs.write().await;
        let slot = cache.entry(user_id).or_insert(stored);
        *slot = (*slot).max(stored);
        Ok(*slot)
    }

    /// Retires every session token outstanding for `user_id` and returns the
    /// new epoch. Persist first, then cache: a crash in between leaves the
    /// stored epoch ahead of the cached one, which over-revokes (safe),
    /// whereas the reverse order would under-revoke.
    pub async fn bump_epoch(&self, user_id: i64) -> Result<u64, ApiError> {
        let next = crate::db::bump_token_epoch(&self.db, &user_id.to_string()).await?;
        let mut cache = self.epochs.write().await;
        let slot = cache.entry(user_id).or_insert(next);
        *slot = (*slot).max(next);
        Ok(*slot)
    }

    /// Verifies a session token AND its epoch, returning the account it acts
    /// as. Public routes need this because they never pass through
    /// [`crate::auth::guard`]; sharing one implementation keeps revocation
    /// from being enforced in one place and forgotten in the other.
    pub async fn session_user(&self, token: &str) -> Option<i64> {
        let (uid, epoch) = self.tokens.verify(token)?;
        // A token minted before the account's last logout is dead, whatever
        // its expiry says.
        (epoch >= self.epoch(uid).await.ok()?).then_some(uid)
    }

    /// Whether this account may reach operator-only endpoints.
    ///
    /// Resolved from the phone its Telegram session is signed in with, not
    /// from a configured user id: Telegram never shows a user their own
    /// numeric id, so an id-keyed allowlist is one nobody can fill in. The
    /// phone comes from the manager's cached `get_me`, so this costs no
    /// network call, and an account that is not signed in is never an
    /// operator.
    pub async fn is_admin(&self, user_id: i64) -> bool {
        let Some(tg) = self.hub.get(user_id).await else {
            return false;
        };
        let Some(phone) = tg.status().await.user.and_then(|u| u.phone) else {
            return false;
        };
        crate::config::get().is_admin_phone(&phone)
    }

    /// Drops the epoch cache, leaving the persisted copy untouched — the
    /// state a freshly started process is in.
    #[cfg(test)]
    pub(crate) async fn forget_epochs(&self) {
        self.epochs.write().await.clear();
    }
}

/// Runs `body` against the process-wide state, database included, on the
/// one runtime the whole test binary shares.
///
/// Both halves matter. `#[tokio::test]` builds a runtime per test and drops
/// it at the end, but the embedded database and the Telegram hub spawn tasks
/// on whichever runtime first touched the state — under a per-test runtime
/// the second test to run finds those tasks' channels closed. And since
/// there is exactly one state, every test shares one store, so each must
/// scope itself to accounts of its own from [`next_uid`] rather than
/// hard-coding ids two tests could both pick.
#[cfg(test)]
pub(crate) fn with_state<F, Fut>(body: F)
where
    F: FnOnce(&'static AppState) -> Fut,
    Fut: Future<Output = ()>,
{
    static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    });
    static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    RT.block_on(async {
        let state = get();
        READY
            .get_or_init(|| async {
                crate::db::connect_mem(&state.db)
                    .await
                    .expect("open test db");
            })
            .await;
        body(state).await;
    });
}

/// An account id no other test is using.
#[cfg(test)]
pub(crate) fn next_uid() -> i64 {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the epoch: logout has to kill tokens that are still well
    /// inside their 30-day TTL, and a token minted after the bump must work.
    #[test]
    fn bumping_the_epoch_revokes_outstanding_tokens() {
        with_state(|state| async move {
            let uid = next_uid();
            let stale = state
                .tokens
                .issue(uid, state.epoch(uid).await.expect("epoch"));
            assert_eq!(state.session_user(&stale).await, Some(uid));

            state.bump_epoch(uid).await.expect("bump");
            assert_eq!(
                state.session_user(&stale).await,
                None,
                "a token from before the bump is dead even though it has not expired"
            );

            // Signing back in mints under the new epoch and works again — the
            // old token stays dead, which is what the pre-epoch design got
            // wrong.
            let fresh = state
                .tokens
                .issue(uid, state.epoch(uid).await.expect("epoch"));
            assert_eq!(state.session_user(&fresh).await, Some(uid));
            assert_eq!(state.session_user(&stale).await, None);
        });
    }

    /// One tenant's logout must not sign everybody else out.
    #[test]
    fn revocation_is_per_account() {
        with_state(|state| async move {
            let (mine, theirs) = (next_uid(), next_uid());
            let their_token = state
                .tokens
                .issue(theirs, state.epoch(theirs).await.expect("epoch"));

            state.bump_epoch(mine).await.expect("bump");
            assert_eq!(state.session_user(&their_token).await, Some(theirs));
        });
    }

    /// The cache is a cache: a fresh process must still honour a logout that
    /// happened before it started, which is why the epoch is persisted.
    #[test]
    fn a_bump_survives_a_restart() {
        with_state(|state| async move {
            let uid = next_uid();
            let stale = state
                .tokens
                .issue(uid, state.epoch(uid).await.expect("epoch"));
            state.bump_epoch(uid).await.expect("bump");

            // Same store, empty cache — as after a restart.
            state.forget_epochs().await;
            assert_eq!(state.session_user(&stale).await, None);
            assert_eq!(state.epoch(uid).await.expect("epoch"), 1);
        });
    }
}
