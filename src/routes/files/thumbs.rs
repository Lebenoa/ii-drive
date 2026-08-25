use std::io::Cursor;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::Response;
use image::codecs::jpeg::JpegEncoder;
use image::{ImageReader, Limits};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Cap on how much of a file we download for thumbnail extraction.
/// MP4s with the index at the end (no faststart) simply fail past this.
const VIDEO_PEEK_CAP: u64 = 128 * 1024 * 1024;

/// Long edge of generated thumbnails; aspect-preserving fit, matching
/// the old ffmpeg `scale=320:-2` intent.
const THUMB_EDGE: u32 = 320;

/// JPEG quality for stored thumbnails; carried over from the ffmpeg
/// `-quality 78` the previous pipeline used.
const THUMB_QUALITY: u8 = 78;

/// Refusal bound for decoder-declared dimensions. Anything a real camera
/// or scanner produces fits far below this; the `Limits` default
/// `max_alloc` (512 MiB) still backstops the true memory ceiling.
const MAX_INPUT_EDGE: u32 = 16384;

/// ffmpeg presence, probed once. Still images are decoded in-process by
/// the `image` crate; only video frame extraction shells out.
static FFMPEG: LazyLock<bool> = LazyLock::new(|| {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
});

/// Decodes arbitrary still-image bytes and re-encodes them as a base64
/// JPEG thumbnail (aspect-preserving fit in [`THUMB_EDGE`]).
///
/// Pure CPU — callers keep it off the async workers via
/// `spawn_blocking`. Decode limits are explicit: the bytes are untrusted
/// uploads, so declared dimensions are capped rather than trusted.
fn encode_thumb_b64(bytes: &[u8]) -> Result<String, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_INPUT_EDGE);
    limits.max_image_height = Some(MAX_INPUT_EDGE);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let rgb = img.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, THUMB_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| e.to_string())?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf))
}

/// Normalizes ad-hoc still-image sources (Telegram stripped previews,
/// embedded cover art) into the stored thumbnail form: base64 JPEG.
/// `None` when the bytes do not parse as a decodable still image.
pub(crate) async fn thumb_b64(bytes: Vec<u8>) -> Option<String> {
    tokio::task::spawn_blocking(move || encode_thumb_b64(&bytes))
        .await
        .ok()?
        .ok()
}

/// Extracts and stores a thumbnail for a freshly uploaded file.
///
/// Images are decoded in-process from the first part; videos get one
/// frame extracted by ffmpeg and share the same downscale + JPEG encode.
/// Runs detached in the background; failures are logged and simply leave
/// no thumb.
///
/// `owner` is the account that holds the part message: this runs
/// detached from the request, so the client is resolved here rather
/// than borrowed from the caller.
pub async fn extract_media_thumb(
    state: &AppState,
    owner: i64,
    uid: &str,
    mime: &str,
    part0: crate::db::FilePart,
) {
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
        let mut stream = match crate::stream::file_stream(&tg, part0.message_id, &part0.chat).await
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

    // One raw source image: the part itself for stills, a single
    // PNG-encoded frame from ffmpeg for video. Every failure branch logs
    // its own reason before returning.
    let raw: Option<Vec<u8>> = if mime.starts_with("image/") {
        match tokio::fs::read(&path).await {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!("thumb read failed for {uid}: {e}");
                None
            }
        }
    } else if !*FFMPEG {
        tracing::info!("ffmpeg not found — skipping thumbnail for {uid}");
        None
    } else {
        // One frame, PNG on the pipe — `image` owns scaling and encoding.
        match tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-frames:v", "1", "-f", "image2", "-c:v", "png", "pipe:1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
        {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => Some(o.stdout),
            Ok(o) => {
                tracing::info!(
                    "ffmpeg produced no thumbnail for {uid} (status {:?})",
                    o.status.code()
                );
                None
            }
            Err(e) => {
                tracing::warn!("ffmpeg failed for {uid}: {e}");
                None
            }
        }
    };
    let _ = tokio::fs::remove_file(&path).await;
    let Some(raw) = raw else { return };

    let b64 = match tokio::task::spawn_blocking(move || encode_thumb_b64(&raw)).await {
        Ok(Ok(b64)) => b64,
        Ok(Err(e)) => {
            tracing::info!("thumbnail encode failed for {uid}: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("thumbnail task failed for {uid}: {e}");
            return;
        }
    };
    match crate::db::set_thumb(&state.db, uid, &b64).await {
        Ok(true) => tracing::info!("thumbnail stored for {uid}"),
        Ok(false) => tracing::warn!("row {uid} vanished before thumb stored"),
        Err(e) => tracing::warn!("thumb store failed for {uid}: {e}"),
    }
}

/// GET /api/files/{id}/thumb — tiny cached JPEG; same auth rules as raw.
pub async fn file_thumb(
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    req: axum::extract::Request,
) -> ApiResult<Response> {
    let state = crate::state::get();
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;

    // Indistinguishable from a missing row, as in `raw_file`: the status code
    // must not reveal that another account owns this id. The bearer is
    // epoch-checked for the same reason as there.
    let bearer_user = match super::bearer(req.headers()) {
        Some(tok) => state.session_user(tok).await,
        None => None,
    };
    if !super::may_read(&state.tokens, &row, &q, bearer_user) {
        return Err(ApiError::not_found("file not found"));
    }

    let b64 = row
        .thumb
        .ok_or_else(|| ApiError::not_found("no thumbnail"))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ApiError::internal(format!("thumb decode: {e}")))?;

    // New rows always hold JPEG; the extra sniff branches serve rows
    // written by the previous ffmpeg WebP/AVIF pipeline.
    let ctype = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if bytes.get(4..8) == Some(b"ftyp")
        && bytes.get(8..12).is_some_and(|b| b.starts_with(b"avi"))
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// A real still image round-trips: PNG in, JPEG out, downscaled to
    /// the thumb edge. Guards the decode-limits and RGB-encode wiring
    /// the whole thumbnail pipeline now leans on.
    #[test]
    fn png_becomes_jpeg_thumb() {
        let src = image::DynamicImage::new_rgb8(1024, 512);
        let mut png = Vec::new();
        src.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let b64 = encode_thumb_b64(&png).unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8, 0xFF]));

        let decoded = image::load_from_memory(&jpeg).unwrap();
        let (w, h) = (decoded.width(), decoded.height());
        assert_eq!(w, 320);
        assert_eq!(h, 160); // aspect preserved
    }

    /// Garbage yields a clean failure, not a panic.
    #[test]
    fn garbage_fails_cleanly() {
        assert!(encode_thumb_b64(b"not an image at all").is_err());
        assert!(encode_thumb_b64(&[]).is_err());
    }
}
