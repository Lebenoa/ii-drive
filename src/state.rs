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
}

impl AppState {
    /// The Telegram account a request acts as.
    ///
    /// A token stays valid for its whole TTL, but the account behind it can
    /// disappear in the meantime (signed out, session revoked, server
    /// restarted without that session file). Callers get 401 in that case so
    /// the web client re-authenticates instead of silently addressing
    /// somebody else's drive.
    pub async fn tg(&self, user_id: i64) -> Result<Arc<TgManager>, ApiError> {
        self.hub
            .get(user_id)
            .await
            .ok_or_else(|| ApiError::unauthorized("that Telegram session is no longer signed in"))
    }
}
