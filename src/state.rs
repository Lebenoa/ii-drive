use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::Tokens;
use crate::error::ApiError;
use crate::tg::{TgHub, TgManager};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<surrealdb::Surreal<surrealdb::engine::local::Db>>,
    pub tokens: Arc<Tokens>,
    /// Every signed-in Telegram account. Handlers never touch a client
    /// directly — they resolve their caller's account through [`Self::tg`].
    pub hub: Arc<TgHub>,
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
    epochs: Arc<tokio::sync::RwLock<HashMap<i64, u64>>>,
}

impl AppState {
    pub fn new(
        db: surrealdb::Surreal<surrealdb::engine::local::Db>,
        tokens: Tokens,
        hub: TgHub,
    ) -> Self {
        AppState {
            db: Arc::new(db),
            tokens: Arc::new(tokens),
            hub: Arc::new(hub),
            epochs: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_state() -> AppState {
        let db = crate::db::open_mem().await.expect("open test db");
        crate::app::shared_state(db)
    }

    /// The point of the epoch: logout has to kill tokens that are still well
    /// inside their 30-day TTL, and a token minted after the bump must work.
    #[tokio::test]
    async fn bumping_the_epoch_revokes_outstanding_tokens() {
        let state = temp_state().await;
        let stale = state.tokens.issue(11, state.epoch(11).await.expect("epoch"));
        assert_eq!(state.session_user(&stale).await, Some(11));

        state.bump_epoch(11).await.expect("bump");
        assert_eq!(
            state.session_user(&stale).await,
            None,
            "a token from before the bump is dead even though it has not expired"
        );

        // Signing back in mints under the new epoch and works again — the old
        // token stays dead, which is what the pre-epoch design got wrong.
        let fresh = state.tokens.issue(11, state.epoch(11).await.expect("epoch"));
        assert_eq!(state.session_user(&fresh).await, Some(11));
        assert_eq!(state.session_user(&stale).await, None);
    }

    /// One tenant's logout must not sign everybody else out.
    #[tokio::test]
    async fn revocation_is_per_account() {
        let state = temp_state().await;
        let theirs = state.tokens.issue(22, state.epoch(22).await.expect("epoch"));

        state.bump_epoch(11).await.expect("bump");
        assert_eq!(state.session_user(&theirs).await, Some(22));
    }

    /// The cache is a cache: a fresh process must still honour a logout that
    /// happened before it started, which is why the epoch is persisted.
    #[tokio::test]
    async fn a_bump_survives_a_restart() {
        let state = temp_state().await;
        let stale = state.tokens.issue(11, state.epoch(11).await.expect("epoch"));
        state.bump_epoch(11).await.expect("bump");

        // Same store, empty cache — as after a restart.
        let restarted = AppState {
            epochs: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            ..state.clone()
        };
        assert_eq!(restarted.session_user(&stale).await, None);
        assert_eq!(restarted.epoch(11).await.expect("epoch"), 1);
    }
}
