use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::Json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{parse_range, percent_encode};

/// Public (shareable) endpoint: streams the stored Telegram document.
pub async fn raw_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    // Private files need a valid credential: the usual Authorization header,
    // a single-file share sig, or the short-lived media token (?mt=) used
    // by <img>/<video> srcs — never the long-lived session token itself.
    if !row.public {
        let ok = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| state.tokens.verify(t))
            || q.get("mt").is_some_and(|t| state.tokens.verify_media(t))
            || q.get("sig").is_some_and(|s| state.tokens.verify_file(&row.uid, s));
        if !ok {
            return Err(ApiError(
                axum::http::StatusCode::FORBIDDEN,
                "file is private".into(),
            ));
        }
    }

    // Single-range HTTP Range support — video seeking needs it. The offset
    // work happens on Telegram's side (skip_chunks) plus a small discard.
    let total = row.size as u64;
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);
    let (start, len, partial) = match range {
        Some((s, e)) => {
            let Some((s, e)) = (s < total).then_some((s, e.min(total - 1))) else {
                return Err(ApiError(
                    axum::http::StatusCode::RANGE_NOT_SATISFIABLE,
                    format!("range outside file (0-{})", total.saturating_sub(1)),
                ));
            };
            (s, e - s + 1, true)
        }
        None => (0, total, false),
    };

    let stream = crate::stream::parts_stream_from(state.tg.clone(), row.parts.clone(), start)
        .await
        .map_err(ApiError::unavailable)?;
    // Bound to the declared Content-Length — the underlying Telegram
    // stream runs to EOF regardless of the requested range.
    let body = Body::from_stream(crate::stream::cap(stream, len));
    let disposition = if q.get("dl").is_some_and(|v| v == "1") {
        "attachment"
    } else {
        "inline"
    };
    let encoded = percent_encode(&row.name);

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, &row.mime)
        .header(header::CONTENT_LENGTH, len);
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", start + len - 1, total),
        );
        builder = builder.status(axum::http::StatusCode::PARTIAL_CONTENT);
    }
    builder
        .header(
            header::CONTENT_DISPOSITION,
            format!("{disposition}; filename*=UTF-8''{encoded}"),
        )
        .body(body)
        .map_err(|e| ApiError::internal(format!("response build: {e}")))
}

/// TTL of share links minted for private files.
const FILE_LINK_TTL_SECS: u64 = 7 * 24 * 3600;

/// GET /api/files/{id}/link — minted share URL for a private file
/// (single-file scope, unlike the session token).
pub async fn file_link(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req_headers: axum::http::HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    // Only the owner may mint links.
    let authed = req_headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| state.tokens.verify(t));
    if !authed {
        return Err(ApiError::unauthorized("unauthorized"));
    }
    let sig = state.tokens.sign_file(&row.uid, FILE_LINK_TTL_SECS);
    Ok(Json(serde_json::json!({
        "url": format!("/api/files/{}/raw?sig={sig}", row.uid),
        "expires_in": FILE_LINK_TTL_SECS,
    })))
}

/// TTL of the short-lived media-read token handed to the web client.
const MEDIA_TTL_SECS: u64 = 3600;

/// GET /api/media-token — bearer-authenticated short-lived signed token for
/// private raw/thumb URLs (`?mt=`), so `<img>`/`<video>` srcs never carry
/// the long-lived session token (which would leak via logs/history/Referer).
pub async fn media_token(
    State(state): State<AppState>,
    req_headers: axum::http::HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let authed = req_headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| state.tokens.verify(t));
    if !authed {
        return Err(ApiError::unauthorized("unauthorized"));
    }
    Ok(Json(serde_json::json!({
        "token": state.tokens.sign_media(MEDIA_TTL_SECS),
        "expires_in": MEDIA_TTL_SECS,
    })))
}
