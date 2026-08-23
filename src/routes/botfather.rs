use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct BotFatherBody {
    pub text: String,
}

/// POST /api/botfather — relay one message to @BotFather over the signed-in
/// user's Telegram session and return its reply. The web wizard drives the
/// /newbot conversation through this; BotFather keeps the dialog state.
pub async fn send(
    State(state): State<AppState>,
    Json(body): Json<BotFatherBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("message must not be empty"));
    }
    if text.len() > 256 {
        return Err(ApiError::bad_request("message too long"));
    }
    let reply = state
        .tg
        .botfather_send(text)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "reply": reply })))
}

#[derive(Deserialize)]
pub struct BotTokenBody {
    pub bot: String,
}

/// GET /api/botfather/bots — ask @BotFather for /mybots and return the
/// owned-bot names parsed from its inline menu button labels.
pub async fn bots(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let names = state
        .tg
        .botfather_my_bots()
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "bots": names })))
}

/// POST /api/botfather/token {bot} — walk the BotFather menus to fetch one
/// owned bot's API token, so it can be imported without copy-paste.
pub async fn token(
    State(state): State<AppState>,
    Json(body): Json<BotTokenBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot = body.bot.trim();
    if bot.is_empty() {
        return Err(ApiError::bad_request("bot name must not be empty"));
    }
    let tok = state
        .tg
        .botfather_bot_token(bot)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({ "token": tok })))
}
