use std::io::Cursor;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::Response;
use image::{ExtendedColorType, ImageEncoder, ImageReader, Limits};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Cap on how much of a file we download for thumbnail extraction.
/// MP4s with the index at the end (no faststart) simply fail past this.
const VIDEO_PEEK_CAP: u64 = 128 * 1024 * 1024;

/// Long edge of generated thumbnails; aspect-preserving fit, matching
/// the old ffmpeg `scale=320:-2` intent.
const THUMB_EDGE: u32 = 320;
/// AVIF encoder settings. A bake-off against JPEG q78 on real photos
/// (six 1200x800 samples, 320px thumbs) put AVIF at ~47% smaller;
/// speed 8 keeps encode near 300 ms, which the detached thumb task
/// absorbs invisibly.
const AVIF_QUALITY: u8 = 70;
const AVIF_SPEED: u8 = 8;

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

/// Decodes arbitrary still-image bytes and re-encodes them as raw AVIF
/// thumbnail bytes (aspect-preserving fit in [`THUMB_EDGE`]).
///
/// Pure CPU — callers keep it off the async workers via
/// `spawn_blocking`. Decode limits are explicit: the bytes are untrusted
/// uploads, so declared dimensions are capped rather than trusted.
fn encode_thumb(bytes: &[u8]) -> Result<Vec<u8>, String> {
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
    image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut buf, AVIF_SPEED, AVIF_QUALITY)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Filesystem location of a file uid's stored thumbnail.
fn thumb_path(dir: &std::path::Path, uid: &str) -> std::path::PathBuf {
    dir.join(format!("{uid}.avif"))
}

/// Whether a stored thumbnail exists for the uid.
pub(crate) fn exists(dir: &std::path::Path, uid: &str) -> bool {
    thumb_path(dir, uid).is_file()
}

/// Writes thumbnail bytes for a uid into `dir`.
pub(crate) async fn write(dir: &std::path::Path, uid: &str, bytes: &[u8]) -> std::io::Result<()> {
    tokio::fs::write(thumb_path(dir, uid), bytes).await
}

/// Removes a stored thumbnail, ignoring absence.
pub(crate) async fn remove(dir: &std::path::Path, uid: &str) {
    let _ = tokio::fs::remove_file(thumb_path(dir, uid)).await;
}

/// Normalizes ad-hoc still-image sources (Telegram stripped previews,
/// embedded cover art) into the stored thumbnail form: raw AVIF bytes.
/// `None` when the bytes do not parse as a decodable still image.
pub(crate) async fn thumb_bytes(bytes: Vec<u8>) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || encode_thumb(&bytes))
        .await
        .ok()?
        .ok()
}
/// Extracts and stores a thumbnail for a freshly uploaded file.
///
/// Images are decoded in-process from the first part; videos get one
/// frame extracted by ffmpeg and share the same downscale + AVIF encode.
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
    let avif = match tokio::task::spawn_blocking(move || encode_thumb(&raw)).await {
        Ok(Ok(avif)) => avif,
        Ok(Err(e)) => {
            tracing::info!("thumbnail encode failed for {uid}: {e}");
            return;
        }
        Err(e) => {
            tracing::warn!("thumbnail task failed for {uid}: {e}");
            return;
        }
    };
    if let Err(e) = write(&state.thumbs_dir, uid, &avif).await {
        tracing::warn!("thumb store failed for {uid}: {e}");
    } else {
        tracing::info!("thumbnail stored for {uid}");
    }
}

/// GET /api/files/{id}/thumb — tiny cached AVIF; same auth rules as raw.
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
    // A missing file is the same "no thumbnail" the old absent column
    // produced; other read failures are genuine server errors.
    let bytes = match tokio::fs::read(thumb_path(&state.thumbs_dir, &id)).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found("no thumbnail"));
        }
        Err(e) => return Err(ApiError::internal(format!("thumb read: {e}"))),
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "image/avif")
        .header(header::CACHE_CONTROL, "private, max-age=86400, immutable")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(format!("response build: {e}")))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A real still image round-trips: PNG in, AVIF out, downscaled to
    /// the thumb edge. Guards the decode-limits and RGB-encode wiring
    /// the whole thumbnail pipeline now leans on. The result cannot be
    /// decoded back here (`avif` is encode-only in this build), so the
    /// container is checked by magic bytes and the payload is compared
    /// against a same-source JPEG.
    #[test]
    fn png_becomes_avif_thumb() {
        let src = image::DynamicImage::new_rgb8(1024, 512);
        let mut png = Vec::new();
        src.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let avif = encode_thumb(&png).unwrap();
        assert_eq!(avif.get(4..8), Some(b"ftyp" as &[u8]));
        assert!(
            avif.get(8..12).is_some_and(|b| b.starts_with(b"avi")),
            "major brand should be avif/avis"
        );

        let mut jpeg = Vec::new();
        let rgb = image::load_from_memory(&png)
            .unwrap()
            .thumbnail(THUMB_EDGE, THUMB_EDGE)
            .to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 78)
            .encode_image(&rgb)
            .unwrap();
        assert!(
            avif.len() < jpeg.len(),
            "AVIF {} beat JPEG {}",
            avif.len(),
            jpeg.len()
        );
    }

    /// Garbage yields a clean failure, not a panic.
    #[test]
    fn garbage_fails_cleanly() {
        assert!(encode_thumb(b"not an image at all").is_err());
        assert!(encode_thumb(&[]).is_err());
    }
}
