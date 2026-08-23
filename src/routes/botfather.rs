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
