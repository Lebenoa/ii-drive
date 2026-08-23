use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::db::{BotDraft, DraftMsg};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct BotFatherBody {
    pub text: String,
}

/// Transcript cap. A `/newbot` run is a handful of lines; anything beyond
/// this is a stuck conversation, and the row should not grow without bound.
const MAX_DRAFT_LOG: usize = 40;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Telegram id of the signed-in account, used to key the draft row.
async fn draft_key(state: &AppState) -> Result<String, ApiError> {
    state
        .tg
        .current_user_id()
        .await
        .map(|id| id.to_string())
        .ok_or_else(|| ApiError::bad_request("Telegram is not connected"))
}

/// The stage the conversation is parked at, derived from the transcript so
/// there is a single source of truth. BotFather asks for a display name
/// first, then a username, then issues the token.
fn stage_of(draft: &BotDraft) -> &'static str {
    if !draft.token.is_empty() {
        return "token";
    }
    // Everything we sent after the opening `/newbot`.
    let answered = draft
        .log
        .iter()
        .filter(|m| m.who == "me" && m.text != "/newbot")
        .count();
    if answered == 0 {
        "name"
    } else {
        "username"
    }
}

fn draft_json(draft: &BotDraft) -> serde_json::Value {
    json!({
        "active": true,
        "stage": stage_of(draft),
        "token": draft.token,
        "updated_at": draft.updated_at,
        "log": draft
            .log
            .iter()
            .map(|m| json!({ "who": m.who, "text": m.text }))
            .collect::<Vec<_>>(),
    })
}

/// POST /api/botfather — relay one message to @BotFather over the signed-in
/// user's Telegram session and return its reply.
///
/// BotFather owns the conversation state, which is exactly why the exchange
/// is persisted here: if the wizard is abandoned mid-question, BotFather is
/// still waiting, and the stored draft is what lets us resume that same
/// conversation instead of firing a second `/newbot` into it.
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
    let key = draft_key(&state).await?;
    let mut draft = crate::db::get_bot_draft(&state.db, &key)
        .await?
        .unwrap_or_default();

    let reply = state
        .tg
        .botfather_send(text)
        .await
        .map_err(ApiError::bad_request)?;

    draft.log.push(DraftMsg { who: "me".into(), text: text.to_string() });
    draft.log.push(DraftMsg { who: "bf".into(), text: reply.clone() });
    if draft.log.len() > MAX_DRAFT_LOG {
        let cut = draft.log.len() - MAX_DRAFT_LOG;
        draft.log.drain(..cut);
    }
    // Capture the token server-side too: the browser may never come back
    // to click "add to pool", and re-deriving it later would mean walking
    // BotFather's menus again.
    if let Some(tok) = crate::tg::bot_token_regex().find(&reply) {
        draft.token = tok.as_str().to_string();
    }
    draft.updated_at = now_secs();
    crate::db::set_bot_draft(&state.db, &key, &draft).await?;

    Ok(Json(json!({
        "reply": reply,
        "draft": draft_json(&draft),
    })))
}

/// GET /api/botfather/draft — the pending `/newbot` conversation, so the
/// wizard can pick up where it left off after a reload or restart.
pub async fn draft(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let key = draft_key(&state).await?;
    match crate::db::get_bot_draft(&state.db, &key).await? {
        Some(d) => Ok(Json(draft_json(&d))),
        None => Ok(Json(json!({ "active": false }))),
    }
}

/// DELETE /api/botfather/draft — tell BotFather to drop its pending
/// question and forget the draft. Without this an abandoned wizard leaves
/// BotFather waiting for an answer indefinitely.
pub async fn cancel_draft(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let key = draft_key(&state).await?;
    let Some(draft) = crate::db::get_bot_draft(&state.db, &key).await? else {
        return Ok(Json(json!({ "ok": true, "cancelled": false })));
    };
    // A conversation that already produced a token has nothing pending on
    // BotFather's side; only an unfinished one needs the /cancel.
    let mut told = false;
    if draft.token.is_empty() {
        match state.tg.botfather_send("/cancel").await {
            Ok(_) => told = true,
            // The draft is dropped either way: a failed /cancel must not
            // strand the wizard on a conversation it cannot clear.
            Err(e) => tracing::warn!("botfather /cancel failed: {e}"),
        }
    }
    crate::db::clear_bot_draft(&state.db, &key).await?;
    Ok(Json(json!({ "ok": true, "cancelled": told })))
}

#[derive(Deserialize)]
pub struct BotTokenBody {
    pub bot: String,
}

/// GET /api/botfather/bots — ask @BotFather for /mybots and return the
/// owned-bot names parsed from its inline menu button labels. Bots already
/// configured in this drive are filtered out.
pub async fn bots(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let names = state
        .tg
        .botfather_my_bots()
        .await
        .map_err(ApiError::bad_request)?;
    let configured: std::collections::HashSet<String> = state
        .tg
        .bot_list()
        .await
        .into_iter()
        .map(|(_, username)| username.trim_start_matches('@').to_lowercase())
        .collect();
    let bots: Vec<String> = names
        .into_iter()
        .filter(|n| !configured.contains(&n.trim_start_matches('@').to_lowercase()))
        .collect();
    Ok(Json(json!({ "bots": bots })))
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
