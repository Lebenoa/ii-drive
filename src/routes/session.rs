use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response as AxumResponse;
use axum::http::header;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Maps a Telegram error to an API error; the stable session-invalid copy
/// becomes 401 so clients can log out structurally instead of matching prose.
fn tg_err(e: String) -> ApiError {
    if e == crate::tg::SESSION_INVALID_MSG {
        ApiError::unauthorized(e)
    } else {
        ApiError::bad_request(e)
    }
}

#[derive(Deserialize)]
pub struct PhoneBody {
    pub phone: String,
}

/// Step 1: request the Telegram login code.
pub async fn auth_phone(
    State(state): State<AppState>,
    Json(body): Json<PhoneBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let phone = body.phone.trim();
    if !crate::config::get().phone_allowed(phone) {
        return Err(ApiError::unauthorized(
            "this phone number is not allowed on this drive",
        ));
    }
    state
        .tg
        .send_code(phone)
        .await
        .map_err(tg_err)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    pub code: String,
}

/// True once the signed-in user has picked at least one storage channel.
async fn channel_ready(state: &AppState) -> bool {
    match state.tg.current_user_id().await {
        Some(id) => !crate::db::get_channels(&state.db, &id.to_string())
            .await
            .unwrap_or_default()
            .is_empty(),
        None => false,
    }
}

/// Step 2: confirm the code. Returns a token when no 2FA is configured.
pub async fn auth_code(
    State(state): State<AppState>,
    Json(body): Json<CodeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    match state.tg.sign_in(body.code.trim()).await {
        Ok(crate::tg::SignInOutcome::Done) => Ok(Json(json!({
            "status": "ok",
            "token": state.tokens.issue(),
            "channel_selected": channel_ready(&state).await,
        }))),
        Ok(crate::tg::SignInOutcome::PasswordRequired { hint }) => {
            Ok(Json(json!({ "status": "password_required", "hint": hint })))
        }
        Err(e) => Err(tg_err(e)),
    }
}

#[derive(Deserialize)]
pub struct PasswordBody {
    pub password: String,
}

#[derive(Deserialize)]
pub struct SelectBody {
    pub channels: Vec<crate::db::ChannelSel>,
}

/// Lists candidate storage channels (dialogs + Saved Messages) and the
/// user's current selection.
pub async fn list_channels(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let available = state
        .tg
        .list_channels()
        .await
        .map_err(tg_err)?;
    let selected = match state.tg.current_user_id().await {
        Some(id) => crate::db::get_channels(&state.db, &id.to_string()).await?,
        None => Vec::new(),
    };
    Ok(Json(serde_json::json!({
        "available": available,
        "selected": selected,
    })))
}

/// Persists the storage-channel selection; uploads round-robin across it.
pub async fn select_channels(
    State(state): State<AppState>,
    Json(body): Json<SelectBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(user_id) = state.tg.current_user_id().await else {
        return Err(ApiError::bad_request("Telegram is not connected"));
    };
    if body.channels.len() > 20 {
        return Err(ApiError::bad_request("at most 20 storage channels"));
    }
    for c in &body.channels {
        if c.chat.trim().is_empty() {
            return Err(ApiError::bad_request("channel key must not be empty"));
        }
    }
    crate::db::set_channels(&state.db, &user_id.to_string(), body.channels.clone()).await?;

    // Wire the existing download-bot pool into every selected channel —
    // including ones picked after the bots were added. Idempotent: bots
    // already promoted in a channel just re-run the harmless EditAdmin.
    let mut results: Vec<serde_json::Value> = Vec::new();
    for sel in &body.channels {
        let res = state.tg.add_bots_to_chat(&sel.chat).await;
        for (bot, r) in res {
            if let Err(e) = r {
                results.push(json!({
                    "chat": sel.chat,
                    "title": sel.title,
                    "bot": bot,
                    "error": e,
                }));
            }
        }
    }

    Ok(Json(json!({ "ok": true, "results": results })))
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub title: String,
    #[serde(default)]
    pub about: String,
}

/// Creates a brand-new Telegram channel for storage use. The channel is NOT
/// auto-selected; the client adds it to the selection and saves explicitly.
pub async fn create_channel(
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let title = body.title.trim();
    if title.is_empty() || title.len() > 128 {
        return Err(ApiError::bad_request(
            "channel name must be 1-128 characters",
        ));
    }
    let about = body.about.trim();
    if about.len() > 500 {
        return Err(ApiError::bad_request("description is too long (max 500)"));
    }
    let info = state
        .tg
        .create_channel(title, about)
        .await
        .map_err(tg_err)?;
    Ok(Json(json!({ "channel": info })))
}

/// GET /api/bot — configured download bots (no tokens).
pub async fn list_bots(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let bots = state.tg.bot_list().await;
    let items: Vec<serde_json::Value> = bots
        .into_iter()
        .map(|(id, username)| json!({ "id": id, "username": username }))
        .collect();
    Ok(Json(json!({ "bots": items })))
}

#[derive(Deserialize)]
pub struct AddBotBody {
    pub token: String,
}

/// POST /api/bot — validate a bot token, add it to the pool, persist it,
/// then invite/promote every pooled bot into all selected storage channels.
pub async fn add_bot(
    State(state): State<AppState>,
    Json(body): Json<AddBotBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = body.token.trim();
    if token.is_empty() || !token.contains(':') {
        return Err(ApiError::bad_request(
            "that does not look like a bot token (expected digits:secret)",
        ));
    }

    let (username, id) = state
        .tg
        .configure_bot(token)
        .await
        .map_err(tg_err)?;

    // Persist (dedupe by id).
    let mut pool = crate::db::get_bots(&state.db).await?;
    pool.retain(|b| b.id != id);
    pool.push(crate::db::BotInfo {
        token: token.to_string(),
        username: username.clone(),
        id,
    });
    crate::db::set_bots(&state.db, &pool).await?;

    // Wire the whole pool into every selected storage channel.
    let selected = match state.tg.current_user_id().await {
        Some(uid) => crate::db::get_channels(&state.db, &uid.to_string())
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut results: Vec<serde_json::Value> = Vec::new();
    for sel in &selected {
        let res = state.tg.add_bots_to_chat(&sel.chat).await;
        for (bot, r) in res {
            results.push(json!({
                "chat": sel.chat,
                "title": sel.title,
                "bot": bot,
                "ok": r.is_ok(),
                "error": r.err(),
            }));
        }
    }

    Ok(Json(json!({
        "bot": { "id": id, "username": username },
        "pool_size": pool.len(),
        "results": results,
    })))
}

/// DELETE /api/bot/{id} — remove a bot from the pool.
pub async fn remove_bot(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    state.tg.drop_bot(id).await;
    crate::db::remove_bot(&state.db, id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Step 3 (only when 2FA is enabled): confirm and hand out the token.
pub async fn auth_password(
    State(state): State<AppState>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<Json<serde_json::Value>> {
    state
        .tg
        .check_password(&body.password)
        .await
        .map_err(tg_err)?;
    Ok(Json(json!({
        "status": "ok",
        "token": state.tokens.issue(),
        "channel_selected": channel_ready(&state).await,
    })))
}

/// GET /api/me — connection status plus whether storage channels are set up.
pub async fn me(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {    let status = state.tg.status().await;
    let channel_selected = if status.authorized {
        channel_ready(&state).await
    } else {
        false
    };
    Ok(Json(json!({
        "connected": status.connected,
        "authorized": status.authorized,
        "user": status.user,
        "error": status.error,
        "relogin": status.relogin,
        "channel_selected": channel_selected,
    })))
}

/// GET /api/avatar — the signed-in user's profile photo as image bytes.
pub async fn avatar(State(state): State<AppState>) -> ApiResult<AxumResponse> {
    let Some(bytes) = state.tg.avatar().await else {
        return Err(ApiError::not_found("no profile photo"));
    };
    AxumResponse::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "private, max-age=300")
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("cannot build avatar response: {e}")))
}

#[derive(Deserialize)]
pub struct RulesBody {
    #[serde(default)]
    pub rules: Vec<crate::db::RouteRule>,
}

/// GET /api/rules — this user's auto-upload routing rules (ordered).
pub async fn get_rules(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let Some(uid) = state.tg.current_user_id().await else {
        return Err(ApiError::bad_request("Telegram is not connected"));
    };
    let rules = crate::db::get_rules(&state.db, &uid.to_string()).await?;
    Ok(Json(json!({ "rules": rules })))
}

/// PUT /api/rules — first-match-wins prefix rules; every target folder
/// must exist, so stale folders cannot silently eat uploads.
pub async fn save_rules(
    State(state): State<AppState>,
    Json(body): Json<RulesBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(uid) = state.tg.current_user_id().await else {
        return Err(ApiError::bad_request("Telegram is not connected"));
    };
    if body.rules.len() > 32 {
        return Err(ApiError::bad_request("at most 32 routing rules"));
    }
    for r in &body.rules {
        let mime = r.mime.trim();
        if mime.is_empty() || mime.len() > 64 {
            return Err(ApiError::bad_request("rule mime must be 1-64 characters"));
        }
        if crate::db::get_folder(&state.db, &r.folder).await?.is_none() {
            return Err(ApiError::bad_request(format!(
                "folder `{}` not found",
                r.folder
            )));
        }
    }
    crate::db::set_rules(&state.db, &uid.to_string(), &body.rules).await?;
    Ok(Json(json!({ "ok": true })))
}

const MB: u64 = 1024 * 1024;

/// GET /api/settings — upload-split threshold in MiB (0 = never split).
pub async fn get_settings(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let bytes = crate::db::get_split(&state.db).await?;
    Ok(Json(json!({ "split_mb": bytes / MB })))
}

#[derive(Deserialize)]
pub struct SettingsBody {
    #[serde(default)]
    pub split_mb: u64,
}

/// PUT /api/settings — files larger than `split_mb` MiB upload as parallel
/// parts. A threshold at or above the upload cap can never trigger (no file
/// can exceed it), which would make it a disguised "off", so it is rejected.
/// Values above Telegram's 2 GiB per-document cap are rejected too — parts
/// must fit in one document.
pub async fn save_settings(
    State(state): State<AppState>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    const TG_DOC_CAP_MB: u64 = 2048;
    let bytes = body
        .split_mb
        .checked_mul(MB)
        .ok_or_else(|| ApiError::bad_request("split threshold too large"))?;
    if body.split_mb > TG_DOC_CAP_MB {
        return Err(ApiError::bad_request(format!(
            "split threshold cannot exceed Telegram's document limit ({TG_DOC_CAP_MB} MB)"
        )));
    }
    if body.split_mb > 0 && bytes >= crate::config::get().max_file_size {
        let cap = crate::config::get().max_file_size / MB;
        return Err(ApiError::bad_request(format!(
            "split threshold must be below the upload limit ({cap} MB) — use 0 to disable splitting"
        )));
    }
    crate::db::set_split(&state.db, bytes).await?;
    Ok(Json(json!({ "ok": true })))
}

/// POST /api/config/reload — re-reads config.toml from disk. Hot-applies
/// runtime fields (upload cap, phone allowlist, thumbnail toggle); paths and
/// credentials only take effect after a restart.
pub async fn reload_config() -> ApiResult<Json<serde_json::Value>> {
    let cfg = crate::config::reload().map_err(|e| e.to_string()).map_err(ApiError::bad_request)?;
    Ok(Json(json!({
        "ok": true,
        "max_file_size": cfg.max_file_size,
        "allowed_phones": cfg.allowed_phones.len(),
        "media_thumbs": cfg.media_thumbs,
    })))
}

