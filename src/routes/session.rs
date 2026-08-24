use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::Response as AxumResponse;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::auth::Caller;
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

/// Step 1: request the Telegram login code. The returned `login_id` names
/// this attempt; the code and password steps quote it back, so two people
/// signing in at the same time never land in each other's login.
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
    let login_id = state.hub.start_login(phone).await.map_err(tg_err)?;
    Ok(Json(json!({ "login_id": login_id })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    pub login_id: String,
    pub code: String,
}

/// True once `user_id` has picked at least one storage channel.
async fn channel_ready(state: &AppState, user_id: i64) -> bool {
    !crate::db::get_channels(&state.db, &user_id.to_string())
        .await
        .unwrap_or_default()
        .is_empty()
}

/// The success payload of a completed login: the session token is minted for
/// the account that just signed in, never for whoever asked, and under that
/// account's current epoch so a logout that happened while this login was in
/// flight still wins.
async fn signed_in(state: &AppState, user_id: i64) -> ApiResult<Json<serde_json::Value>> {
    let epoch = state.epoch(user_id).await?;
    Ok(Json(json!({
        "status": "ok",
        "token": state.tokens.issue(user_id, epoch),
        "channel_selected": channel_ready(state, user_id).await,
    })))
}

/// Step 2: confirm the code. Returns a token when no 2FA is configured.
pub async fn auth_code(
    State(state): State<AppState>,
    Json(body): Json<CodeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let step = state
        .hub
        .submit_code(body.login_id.trim(), body.code.trim())
        .await
        .map_err(tg_err)?;
    match step {
        crate::tg::LoginStep::Done(user_id) => signed_in(&state, user_id).await,
        crate::tg::LoginStep::PasswordRequired { hint } => {
            Ok(Json(json!({ "status": "password_required", "hint": hint })))
        }
    }
}

#[derive(Deserialize)]
pub struct PasswordBody {
    pub login_id: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct SelectBody {
    pub channels: Vec<crate::db::ChannelSel>,
}

/// Lists candidate storage channels (dialogs + Saved Messages) and the
/// caller's current selection.
pub async fn list_channels(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let available = state.tg(uid).await?.list_channels().await.map_err(tg_err)?;
    let selected = crate::db::get_channels(&state.db, &uid.to_string()).await?;
    Ok(Json(serde_json::json!({
        "available": available,
        "selected": selected,
    })))
}

/// Persists the storage-channel selection; uploads round-robin across it.
pub async fn select_channels(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<SelectBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let tg = state.tg(uid).await?;
    if body.channels.len() > 20 {
        return Err(ApiError::bad_request("at most 20 storage channels"));
    }
    for c in &body.channels {
        if c.chat.trim().is_empty() {
            return Err(ApiError::bad_request("channel key must not be empty"));
        }
    }
    crate::db::set_channels(&state.db, &uid.to_string(), body.channels.clone()).await?;

    // Wire the existing download-bot pool into every selected channel —
    // including ones picked after the bots were added. Idempotent: bots
    // already promoted in a channel just re-run the harmless EditAdmin.
    let mut results: Vec<serde_json::Value> = Vec::new();
    for sel in &body.channels {
        let res = tg.add_bots_to_chat(&sel.chat).await;
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
    Extension(Caller(uid)): Extension<Caller>,
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
        .tg(uid)
        .await?
        .create_channel(title, about)
        .await
        .map_err(tg_err)?;
    Ok(Json(json!({ "channel": info })))
}

/// GET /api/bot — the caller's configured download bots (no tokens).
pub async fn list_bots(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let bots = state.tg(uid).await?.bot_list().await;
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

/// POST /api/bot — validate a bot token, add it to the caller's pool,
/// persist it, then invite/promote every pooled bot into all of the
/// caller's selected storage channels.
pub async fn add_bot(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<AddBotBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = body.token.trim();
    if token.is_empty() || !token.contains(':') {
        return Err(ApiError::bad_request(
            "that does not look like a bot token (expected digits:secret)",
        ));
    }

    let tg = state.tg(uid).await?;
    let (username, id) = tg.configure_bot(token).await.map_err(tg_err)?;

    // Persist (dedupe by id).
    let mut pool = crate::db::get_bots(&state.db, uid).await?;
    pool.retain(|b| b.id != id);
    pool.push(crate::db::BotInfo {
        token: token.to_string(),
        username: username.clone(),
        id,
    });
    crate::db::set_bots(&state.db, uid, &pool).await?;

    // The wizard's job is done once the bot is in the pool: drop the draft
    // so the next "create a bot" starts a fresh /newbot rather than
    // resuming a finished conversation.
    let key = uid.to_string();
    if let Ok(Some(draft)) = crate::db::get_bot_draft(&state.db, &key).await
        && draft.token == token
    {
        crate::db::clear_bot_draft(&state.db, &key).await?;
    }

    // Wire the whole pool into every selected storage channel.
    let selected = crate::db::get_channels(&state.db, &key)
        .await
        .unwrap_or_default();
    let mut results: Vec<serde_json::Value> = Vec::new();
    for sel in &selected {
        let res = tg.add_bots_to_chat(&sel.chat).await;
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

/// DELETE /api/bot/{id} — remove a bot from the caller's pool.
pub async fn remove_bot(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    state.tg(uid).await?.drop_bot(id).await;
    crate::db::remove_bot(&state.db, uid, id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Step 3 (only when 2FA is enabled): confirm and hand out the token.
pub async fn auth_password(
    State(state): State<AppState>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let user_id = state
        .hub
        .submit_password(body.login_id.trim(), &body.password)
        .await
        .map_err(tg_err)?;
    signed_in(&state, user_id).await
}

/// POST /api/auth/logout — retire the caller's session tokens, then sign its
/// account out of Telegram and forget its session. Only that one account:
/// other tenants keep working.
///
/// The epoch bump comes first and happens even if the Telegram sign-out
/// fails: dropping the hub session alone does not stop a stolen token, which
/// starts working again the moment the account signs back in. Bumping is the
/// part that actually revokes, so it must not be skipped by an upstream
/// error. Media tokens are deliberately not epoch-covered — they are
/// minutes-lived, so expiry already closes that window.
pub async fn auth_logout(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    state.bump_epoch(uid).await?;
    state.hub.logout(uid).await.map_err(tg_err)?;
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/me — connection status, whether storage channels are set up, and
/// whether the caller may reach operator-only endpoints (the UI hides those
/// surfaces; the endpoints themselves still check).
pub async fn me(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let status = state.tg(uid).await?.status().await;
    let channel_selected = status.authorized && channel_ready(&state, uid).await;
    // The phone is already in hand here, so the operator check costs nothing
    // extra; `state.is_admin` exists for callers without a status.
    let admin = status
        .user
        .as_ref()
        .and_then(|u| u.phone.as_deref())
        .is_some_and(|p| crate::config::get().is_admin_phone(p));
    Ok(Json(json!({
        "connected": status.connected,
        "authorized": status.authorized,
        "user": status.user,
        "error": status.error,
        "relogin": status.relogin,
        "channel_selected": channel_selected,
        "admin": admin,
    })))
}

/// GET /api/avatar — the caller's profile photo as image bytes.
pub async fn avatar(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<AxumResponse> {
    let Some(bytes) = state.tg(uid).await?.avatar().await else {
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

/// GET /api/rules — the caller's auto-upload routing rules (ordered).
pub async fn get_rules(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let rules = crate::db::get_rules(&state.db, &uid.to_string()).await?;
    Ok(Json(json!({ "rules": rules })))
}

/// PUT /api/rules — first-match-wins prefix rules; every target folder must
/// exist and belong to the caller, so neither a stale folder nor somebody
/// else's folder can silently eat uploads.
pub async fn save_rules(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<RulesBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if body.rules.len() > 32 {
        return Err(ApiError::bad_request("at most 32 routing rules"));
    }
    for r in &body.rules {
        let mime = r.mime.trim();
        if mime.is_empty() || mime.len() > 64 {
            return Err(ApiError::bad_request("rule mime must be 1-64 characters"));
        }
        let mine = crate::db::get_folder(&state.db, &r.folder)
            .await?
            .is_some_and(|f| f.owner == uid);
        if !mine {
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

/// GET /api/settings — the caller's upload-split threshold in MiB
/// (0 = never split).
pub async fn get_settings(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let bytes = crate::db::get_split(&state.db, &uid.to_string()).await?;
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
    Extension(Caller(uid)): Extension<Caller>,
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
    crate::db::set_split(&state.db, &uid.to_string(), bytes).await?;
    Ok(Json(json!({ "ok": true })))
}

/// POST /api/config/reload — re-reads config.toml from disk. Hot-applies
/// runtime fields (upload cap, phone allowlist, thumbnail toggle); paths and
/// credentials only take effect after a restart.
///
/// `config.toml` is process-wide, so a reload moves the upload cap and the
/// phone allowlist for every tenant at once: operators only. 404 rather than
/// 403 so a non-admin cannot even confirm the endpoint exists.
pub async fn reload_config(
    State(state): State<AppState>,
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    if !state.is_admin(uid).await {
        return Err(ApiError::not_found("not found"));
    }
    let cfg = crate::config::reload()
        .map_err(|e| e.to_string())
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({
        "ok": true,
        "max_file_size": cfg.max_file_size,
        "allowed_phones": cfg.allowed_phones.len(),
        "media_thumbs": cfg.media_thumbs,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch state on an in-memory store, so a handler test needs no temp
    /// directory (see `db::open_mem`).
    async fn temp_state() -> AppState {
        let db = crate::db::open_mem().await.expect("open test db");
        crate::app::shared_state(db)
    }

    /// The split threshold is one tenant's tuning knob: saving it must not
    /// move anybody else's, which is exactly what the old global row did.
    #[tokio::test]
    async fn settings_are_scoped_to_the_caller() {
        let state = temp_state().await;

        let saved = save_settings(
            State(state.clone()),
            Extension(Caller(11)),
            Json(SettingsBody { split_mb: 64 }),
        )
        .await
        .expect("save");
        assert_eq!(saved.0["ok"], true);

        let mine = get_settings(State(state.clone()), Extension(Caller(11)))
            .await
            .expect("read own");
        assert_eq!(mine.0["split_mb"], 64);

        let theirs = get_settings(State(state), Extension(Caller(22)))
            .await
            .expect("read other");
        assert_eq!(theirs.0["split_mb"], 0, "another tenant keeps its default");
    }

    /// A rule may only target a folder the caller owns — otherwise one
    /// tenant could route their uploads into another tenant's folder.
    #[tokio::test]
    async fn rules_reject_a_foreign_folder() {
        let state = temp_state().await;
        crate::db::create_folder(&state.db, 11, "fold-11", "mine", "")
            .await
            .expect("folder");

        let rule = crate::db::RouteRule {
            mime: "image/".to_string(),
            folder: "fold-11".to_string(),
        };
        let accepted = save_rules(
            State(state.clone()),
            Extension(Caller(11)),
            Json(RulesBody {
                rules: vec![rule.clone()],
            }),
        )
        .await
        .expect("own folder is accepted");
        assert_eq!(accepted.0["ok"], true);

        let err = save_rules(
            State(state.clone()),
            Extension(Caller(22)),
            Json(RulesBody { rules: vec![rule] }),
        )
        .await
        .expect_err("a foreign folder must not be routable");
        assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);

        let stored = crate::db::get_rules(&state.db, "22").await.expect("rules");
        assert!(stored.is_empty(), "the rejected rule set is not persisted");
    }

    /// `config.toml` is process-wide, so a reload changes the upload cap and
    /// the phone allowlist for every tenant. The caller here has no Telegram
    /// session, so no phone can be resolved for it and it cannot be an
    /// operator — the endpoint must not even admit to existing.
    #[tokio::test]
    async fn config_reload_is_operator_only() {
        let state = temp_state().await;
        let err = reload_config(State(state), Extension(Caller(11)))
            .await
            .expect_err("a non-admin tenant cannot reload global config");
        assert_eq!(err.0, axum::http::StatusCode::NOT_FOUND);
    }

    /// Logout has to revoke, not just forget: before the epoch existed, a
    /// token stolen from this account resumed working as soon as the account
    /// signed back in.
    #[tokio::test]
    async fn logout_revokes_outstanding_tokens() {
        let state = temp_state().await;
        let epoch = state.epoch(11).await.expect("epoch");
        let stolen = state.tokens.issue(11, epoch);
        assert_eq!(state.session_user(&stolen).await, Some(11));

        let body = auth_logout(State(state.clone()), Extension(Caller(11)))
            .await
            .expect("logout");
        assert_eq!(body.0["ok"], serde_json::json!(true));

        assert_eq!(state.session_user(&stolen).await, None);
        assert!(
            state.epoch(11).await.expect("epoch") > epoch,
            "logout advances the account's epoch"
        );
    }
}
