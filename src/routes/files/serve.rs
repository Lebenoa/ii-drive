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

/// Escapes text for safe interpolation into HTML content.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Bytes in a short human form for the share page header.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// GET /s/{id} — landing page for a **public** file: previews images,
/// video and audio inline and offers everything else for download.
///
/// The page itself is static HTML built from row metadata only; the media
/// is fetched by the browser from the same raw endpoint the SPA uses, so
/// Range seeking works here too. Private files answer 404 exactly like
/// `raw_file` — existence must not leak.
pub async fn share_page(Path(id): Path<String>) -> ApiResult<Response> {
    let state = crate::state::get();
    let row = crate::db::get(&state.db, &id)
        .await?
        .filter(|row| row.public)
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    let name = html_escape(&row.name);
    let size = human_size(row.size as u64);
    let raw = format!("/api/files/{}/raw", id);

    let viewer = if row.mime.starts_with("image/") {
        format!(r#"<img class="media" src="{raw}" alt="{name}">"#)
    } else if row.mime.starts_with("video/") {
        format!(r#"<video class="media" src="{raw}" controls preload="metadata"></video>"#)
    } else if row.mime.starts_with("audio/") {
        format!(r#"<audio class="media" src="{raw}" controls></audio>"#)
    } else {
        String::new()
    };
    let mime_line = html_escape(&row.mime);

    // Static metadata plus same-origin URLs built from ids above; `name`
    // and `mime` are escaped, so nothing user-controlled reaches the
    // document unescaped.
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name}</title>
<style>
  :root {{ color-scheme: dark; }}
  body {{ margin: 0; min-height: 100vh; display: flex; align-items: center;
         justify-content: center; background: #101014; color: #e8e8ee;
         font-family: system-ui, sans-serif; }}
  .card {{ max-width: 960px; width: calc(100% - 2rem); margin: 2rem 0;
          text-align: center; }}
  .media {{ max-width: 100%; max-height: 78vh; border-radius: 10px; }}
  h1 {{ font-size: 1.05rem; font-weight: 600; word-break: break-all;
       margin: 1rem 0 .25rem; }}
  p.meta {{ margin: 0 0 1.25rem; color: #9a9aa6; font-size: .85rem; }}
  a.dl {{ display: inline-block; padding: .55rem 1.4rem; border-radius: 8px;
         background: #3b82f6; color: #fff; text-decoration: none;
         font-size: .95rem; }}
  a.dl:hover {{ background: #2f6fe0; }}
</style>
</head>
<body>
<main class="card">
  {viewer}
  <h1>{name}</h1>
  <p class="meta">{size} &middot; {mime_line}</p>
  <a class="dl" href="{raw}?dl=1" download>Download</a>
</main>
</body>
</html>"#,
    );

    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(html))
        .map_err(|e| ApiError::internal(format!("response build: {e}")))
}

#[cfg(test)]
mod share_tests {
    use super::*;

    /// The landing page renders for public rows and answers 404 for
    /// private ones — the same non-leaking contract as `raw_file`.
    #[test]
    fn share_page_serves_public_and_hides_private() {
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

            let res = share_page(axum::extract::Path(row.uid.clone()))
                .await
                .expect("public page");
            assert_eq!(res.status(), 200);
            let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .expect("body");
            let html = String::from_utf8_lossy(&body);
            assert!(html.contains("pic &amp; &lt;spec&gt;.png"), "name escaped");
            assert!(html.contains("/api/files/01SHARE/raw"));
            assert!(html.contains(r#"<img class="media""#));

            // Flipping to private must take the page (and thus the share
            // link) down immediately.
            crate::db::set_public(&state.db, &row.uid, false)
                .await
                .expect("unshare");
            let res = share_page(axum::extract::Path(row.uid)).await;
            match res {
                Err(e) => assert_eq!(e.0, axum::http::StatusCode::NOT_FOUND),
                Ok(_) => panic!("private file must not render a share page"),
            }
        });
    }
}
