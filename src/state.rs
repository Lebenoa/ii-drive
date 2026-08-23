use std::sync::Arc;

use crate::auth::Tokens;
use crate::tg::TgManager;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<surrealdb::Surreal<surrealdb::engine::local::Db>>,
    pub tokens: Arc<Tokens>,
    pub tg: Arc<TgManager>,
}
