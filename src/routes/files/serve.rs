use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::Response;
use axum::{Extension, Json};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

use super::{bearer, may_read, parse_range, percent_encode};

/// Public (shareable) endpoint: streams the stored Telegram document.
pub async fn raw_file(
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let state = crate::state::get();
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    // Private files need a credential belonging to the owner: the usual
    // Authorization header, a single-file share sig, or the short-lived media
    // token (?mt=) used by <img>/<video> srcs. The bearer goes through
    // `session_user` so a token revoked by logout stops reading bytes here
    // too — this route never passes through `auth::guard`.
    //
    // Answer exactly as if the row did not exist. Several accounts share this
    // endpoint, so a distinguishable "forbidden" would let anyone probe which
    // ids exist in somebody else's drive.
    let bearer_user = match super::bearer(req.headers()) {
        Some(tok) => state.session_user(tok).await,
        None => None,
    };
    if !may_read(&state.tokens, &row, &q, bearer_user) {
        return Err(ApiError::not_found("file not found"));
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

    // The bytes live in the owner's storage chats, and the reader here may be
    // anonymous (public file or share link) — so the fetch always runs on the
    // owner's account, never the caller's.
    let tg = state.tg(row.owner).await?;
    let stream = crate::stream::parts_stream_from(tg, row.parts.clone(), start)
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
    Path(id): Path<String>,
    req_headers: axum::http::HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    // Minting a capability is an owner-only act, so this endpoint accepts the
    // session token alone — and only the owner's, checked through
    // `session_user` so a revoked token cannot mint fresh share links. It
    // sits on the public router (no `Caller`), hence the manual check. A
    // caller who is not the owner is told the file does not exist, so another
    // account's ids cannot be enumerated through the status code.
    let authed = match bearer(&req_headers) {
        Some(tok) => state.session_user(tok).await == Some(row.owner),
        None => false,
    };
    if !authed {
        return Err(ApiError::not_found("file not found"));
    }
    let sig = state.tokens.sign_file(&row.uid, FILE_LINK_TTL_SECS);
    Ok(Json(serde_json::json!({
        "url": format!("/api/files/{}/raw?sig={sig}", row.uid),
        "expires_in": FILE_LINK_TTL_SECS,
    })))
}

/// TTL of the short-lived media-read token handed to the web client.
const MEDIA_TTL_SECS: u64 = 3600;

/// GET /api/media-token — short-lived signed token for the caller's private
/// raw/thumb URLs (`?mt=`), so `<img>`/`<video>` srcs never carry the
/// long-lived session token (which would leak via logs/history/Referer).
/// The token names its account, so it only opens that account's files.
pub async fn media_token(
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    Ok(Json(serde_json::json!({
        "token": state.tokens.sign_media(uid, MEDIA_TTL_SECS),
        "expires_in": MEDIA_TTL_SECS,
    })))
}

/// GET /api/files/{id}/meta — public metadata for the share page
/// (`/s/{id}`, a pure SPA route): name, mime, size and the uploader's
/// display name when their account is reachable. Private files answer
/// 404 exactly like `raw_file` — existence must not leak.
pub async fn share_meta(Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let row = crate::db::get(&state.db, &id)
        .await?
        .filter(|row| row.public)
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    // Best effort: the name comes from the owner's cached Telegram
    // profile. A signed-out owner or first-hit RPC failure simply omits
    // it — the page never blocks on it.
    let owner = match state.hub.get(row.owner).await {
        Some(tg) => tg.status().await.user.map(|u| u.name),
        None => None,
    };
    Ok(Json(serde_json::json!({
        "name": row.name,
        "mime": row.mime,
        "size": row.size,
        "owner": owner,
    })))
}

#[cfg(test)]
mod share_tests {
    use super::*;

    /// The share metadata endpoint serves public rows and answers 404
    /// for private ones — the same non-leaking contract as `raw_file`.
    #[test]
    fn share_meta_serves_public_and_hides_private() {
        crate::state::with_state(|state| async move {
            let row = crate::db::FileRow {
                owner: 7,
                uid: "01SHARE".to_string(),
                name: "pic & <spec>.png".to_string(),
                mime: "image/png".to_string(),
                size: 1234,
                created_at: 0,
                folder: String::new(),
                parts: Vec::new(),
                public: true,
            };
            crate::db::insert(&state.db, &row).await.expect("insert");

            let meta = share_meta(axum::extract::Path(row.uid.clone()))
                .await
                .expect("public meta");
            assert_eq!(meta.0["name"], "pic & <spec>.png");
            assert_eq!(meta.0["mime"], "image/png");
            assert_eq!(meta.0["size"], 1234);

            // Flipping to private must take the metadata (and thus the
            // share link) down immediately.
            crate::db::set_public(&state.db, &row.uid, false)
                .await
                .expect("unshare");
            let res = share_meta(axum::extract::Path(row.uid)).await;
            match res {
                Err(e) => assert_eq!(e.0, axum::http::StatusCode::NOT_FOUND),
                Ok(_) => panic!("private file must not expose metadata"),
            }
        });
    }
}
