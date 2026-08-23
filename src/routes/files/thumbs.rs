use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Cap on how much of a file we download for thumbnail extraction.
/// MP4s with the index at the end (no faststart) simply fail past this.
const VIDEO_PEEK_CAP: u64 = 128 * 1024 * 1024;

/// Which thumbnail encoder this ffmpeg build supports; probed once at
/// first use. AVIF (libaom) when present — smallest — else WebP.
#[derive(Clone, Copy, PartialEq)]
enum ThumbEncoder {
    None,
    Webp,
    Avif,
}

/// Probed once on first touch. The probe shells out to ffmpeg twice, so
/// every reader forces it inside `block_in_place`.
static FFMPEG_ENCODER: LazyLock<ThumbEncoder> = LazyLock::new(|| {
    let runs = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| !o.stdout.is_empty());
    if !runs {
        return ThumbEncoder::None;
    }
    // -encoders lists on stderr; look for the AV1 still-image encoder.
    let has_aom = std::process::Command::new("ffmpeg")
        .arg("-encoders")
        .output()
        .is_ok_and(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            text.contains("libaom-av1") || err.contains("libaom-av1")
        });
    if has_aom {
        ThumbEncoder::Avif
    } else {
        ThumbEncoder::Webp
    }
});

/// Downloads the first part of a freshly uploaded video or image (bounded),
/// extracts a 320px WebP thumbnail with ffmpeg (first frame for video,
/// downscale for images), and stores it on the row. Runs in the background;
/// failures are logged and simply leave no thumb.
///
/// `owner` is the account that holds the part message: this runs detached
/// from the request, so the client is resolved here rather than borrowed
/// from the caller.
pub async fn extract_media_thumb(
    state: &AppState,
    owner: i64,
    uid: &str,
    part0: crate::db::FilePart,
) {
    let encoder = tokio::task::block_in_place(|| *FFMPEG_ENCODER);
    if encoder == ThumbEncoder::None {
        tracing::info!("ffmpeg not found — skipping thumbnail for {uid}");
        return;
    }
    if part0.size as u64 > VIDEO_PEEK_CAP {
        tracing::info!("file {uid} too large for thumbnail extraction, skipping");
        return;
    }

    let dir = std::env::temp_dir().join("ii-drive-thumbs");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!("thumb temp dir: {e}");
        return;
    }
    let path = dir.join(uid);
    let tg = match state.hub.get(owner).await {
        Some(tg) => tg,
        None => {
            tracing::info!("account {owner} signed out — skipping thumbnail for {uid}");
            return;
        }
    };
    {
        use futures::StreamExt;
        let mut stream = match crate::stream::file_stream(&tg, part0.message_id, &part0.chat)
            .await
        {
            Ok(s) => Box::pin(s),
            Err(e) => {
                tracing::warn!("thumb download start failed for {uid}: {e}");
                return;
            }
        };
        let mut out = match tokio::fs::File::create(&path).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("thumb temp file failed for {uid}: {e}");
                return;
            }
        };
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut out, &chunk).await {
                tracing::warn!("thumb write failed for {uid}: {e}");
                let _ = tokio::fs::remove_file(&path).await;
                return;
            }
            written += chunk.len() as u64;
            if written > VIDEO_PEEK_CAP {
                break; // enough — or the index is at the end; ffmpeg will tell
            }
        }
    }

    // 320px, one frame. AVIF first when the build has libaom (smallest);
    // WebP is the everywhere-fallback. On AVIF failure retry once as WebP
    // in case the encoder exists but the muxer does not.
    let common = ["-v", "error", "-i"];
    let tail_webp = [
        "-frames:v",
        "1",
        "-vf",
        "scale=320:-2",
        "-f",
        "webp",
        "-lossless",
        "0",
        "-compression_level",
        "6",
        "-quality",
        "78",
        "pipe:1",
    ];
    let tail_avif = [
        "-frames:v",
        "1",
        "-vf",
        "scale=320:-2",
        "-c:v",
        "libaom-av1",
        "-crf",
        "42",
        "-cpu-used",
        "8",
        "-still-picture",
        "1",
        "-f",
        "avif",
        "pipe:1",
    ];
    let attempt = |tail: &[&str]| {
        tokio::process::Command::new("ffmpeg")
            .args(common)
            .arg(&path)
            .args(tail)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
    };

    let out = match if encoder == ThumbEncoder::Avif {
        attempt(&tail_avif).await
    } else {
        attempt(&tail_webp).await
    } {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => o.stdout,
        Ok(_) if encoder == ThumbEncoder::Avif => {
            tracing::info!("avif encode failed for {uid}; falling back to webp");
            match attempt(&tail_webp).await {
                Ok(o) if o.status.success() && !o.stdout.is_empty() => o.stdout,
                _ => {
                    let _ = tokio::fs::remove_file(&path).await;
                    tracing::info!("no thumbnail produced for {uid}");
                    return;
                }
            }
        }
        Ok(o) => {
            let _ = tokio::fs::remove_file(&path).await;
            tracing::info!(
                "ffmpeg produced no thumbnail for {uid} (status {:?})",
                o.status.code()
            );
            return;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            tracing::warn!("ffmpeg failed for {uid}: {e}");
            return;
        }
    };
    let _ = tokio::fs::remove_file(&path).await;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(out);
    match crate::db::set_thumb(&state.db, uid, &b64).await {
        Ok(true) => tracing::info!("thumbnail stored for {uid}"),
        Ok(false) => tracing::warn!("row {uid} vanished before thumb stored"),
        Err(e) => tracing::warn!("thumb store failed for {uid}: {e}"),
    }
}

/// GET /api/files/{id}/thumb — tiny cached JPEG; same auth rules as raw.
pub async fn file_thumb(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    if !super::may_read(&state.tokens, &row, req.headers(), &q) {
        return Err(ApiError(
            axum::http::StatusCode::FORBIDDEN,
            "file is private".into(),
        ));
    }

    let b64 = row
        .thumb
        .ok_or_else(|| ApiError::not_found("no thumbnail"))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ApiError::internal(format!("thumb decode: {e}")))?;

    let ctype = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.get(4..8) == Some(b"ftyp") && bytes.get(8..12).is_some_and(|b| b.starts_with(b"avi")) {
        "image/avif"
    } else {
        "image/jpeg"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .header(header::CACHE_CONTROL, "private, max-age=86400, immutable")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("response build: {e}")))
}
