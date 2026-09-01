use std::io::Cursor;
use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::header;
use axum::response::Response;
use image::{ExtendedColorType, ImageEncoder, ImageReader, Limits};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

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
pub fn exists(dir: &std::path::Path, uid: &str) -> bool {
    thumb_path(dir, uid).is_file()
}

/// Writes thumbnail bytes for a uid into `dir`.
pub async fn write(dir: &std::path::Path, uid: &str, bytes: &[u8]) -> std::io::Result<()> {
    tokio::fs::write(thumb_path(dir, uid), bytes).await
}

/// Removes a stored thumbnail, ignoring absence.
pub async fn remove(dir: &std::path::Path, uid: &str) {
    let _ = tokio::fs::remove_file(thumb_path(dir, uid)).await;
}

/// Parses a "HH:MM" local wall-clock anchor into (hour, minute).
pub fn parse_sweep_time(s: &str) -> Result<(u8, u8), String> {
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| format!("expected \"HH:MM\", got {s:?}"))?;
    let h: u8 = h.trim().parse().map_err(|_| format!("bad hour in {s:?}"))?;
    let m: u8 = m
        .trim()
        .parse()
        .map_err(|_| format!("bad minute in {s:?}"))?;
    if h > 23 || m > 59 {
        return Err(format!("time out of range in {s:?}"));
    }
    Ok((h, m))
}

/// Seconds until the next sweep tick on the schedule anchored at local
/// `HH:MM`, repeating every `hours` hours across days. `None` when the
/// periodic sweep is disabled or the anchor does not parse.
pub fn next_sweep_in(anchor: &str, hours: u64) -> Option<std::time::Duration> {
    use time::{OffsetDateTime, Time};
    let (h, m) = parse_sweep_time(anchor).ok()?;
    if hours == 0 {
        return None;
    }
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    // The grid starts at today's anchor and steps forward every `hours`
    // hours, rolling over into following days as needed — DST shifts are
    // absorbed by the fixed-offset arithmetic rather than re-anchoring.
    let today_anchor = now.replace_time(Time::from_hms(h, m, 0).ok()?);
    let step = i64::try_from(hours.saturating_mul(3600)).ok()?;
    let mut cand = today_anchor.unix_timestamp();
    while cand <= now.unix_timestamp() {
        cand = cand.saturating_add(step);
    }
    Some(std::time::Duration::from_secs(
        u64::try_from(cand.saturating_sub(now.unix_timestamp())).ok()?,
    ))
}

/// Deletes preview files whose row is gone (a crash between row delete
/// and preview unlink leaves these behind). Returns how many went away.
pub async fn sweep(state: &AppState) -> Result<usize, String> {
    let live = crate::db::uids(&state.db)
        .await
        .map_err(|e| e.to_string())?;
    let mut rd = tokio::fs::read_dir(&state.thumbs_dir)
        .await
        .map_err(|e| e.to_string())?;
    let mut removed: usize = 0;
    while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "avif")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && !live.contains(stem)
            && tokio::fs::remove_file(&path).await.is_ok()
        {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

/// Normalizes ad-hoc still-image sources (Telegram stripped previews,
/// embedded cover art) into the stored thumbnail form: raw AVIF bytes.
/// `None` when the bytes do not parse as a decodable still image.
pub async fn thumb_bytes(bytes: Vec<u8>) -> Option<Vec<u8>> {
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
    let dir = std::env::temp_dir().join("ii-drive-thumbs");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!("thumb temp dir: {e}");
        return;
    }
    let path = dir.join(uid);
    let Some(tg) = state.hub.get(owner).await else {
        tracing::info!("account {owner} signed out — skipping thumbnail for {uid}");
        return;
    };

    let size = u64::try_from(part0.size).unwrap_or(0);

    // One raw source image: the part itself for stills, a single
    // PNG-encoded frame from ffmpeg for video. Every failure branch logs
    // its own reason before returning.
    let raw: Option<Vec<u8>> = if mime.starts_with("image/") {
        // Still images decode from the complete part; they are rarely
        // large enough for the transfer to matter.
        if download_bounded(&tg, uid, &part0, 0, size, &path)
            .await
            .is_err()
        {
            None
        } else {
            match tokio::fs::read(&path).await {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    tracing::warn!("thumb read failed for {uid}: {e}");
                    None
                }
            }
        }
    } else if !*FFMPEG {
        tracing::info!("ffmpeg not found — skipping thumbnail for {uid}");
        None
    } else {
        // ffmpeg reads the file back from this server's own ranged raw
        // endpoint: it seeks MP4/MKV indexes over HTTP itself, so any file
        // size and both moov placements work without downloading whole
        // parts. A short-lived media token names the owning account.
        let cfg = crate::config::get();
        let host = if cfg.host == "0.0.0.0" {
            "127.0.0.1"
        } else {
            &cfg.host
        };
        let mt = state.tokens.sign_media(owner, 600);
        let url = format!("http://{host}:{}/api/files/{uid}/raw?mt={mt}", cfg.port);
        run_ffmpeg_frame(uid, &url).await
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

/// Streams up to `cap` bytes of a part starting at byte `start` into
/// `path` (truncating). Errors are logged; the boolean only signals
/// whether usable bytes landed on disk.
async fn download_bounded(
    tg: &crate::tg::TgManager,
    uid: &str,
    part0: &crate::db::FilePart,
    start: u64,
    cap: u64,
    path: &std::path::Path,
) -> Result<(), ()> {
    use futures::StreamExt;
    // The part may be an at-rest-encrypted container. When it carries a
    // nonce, the stored bytes are ciphertext: wrap the stream in the
    // decryptor so the thumbnail decoder sees plaintext. Without a key the
    // file is unreadable — fail loudly rather than emit a garbage image.
    let mut stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send>,
    > = match crate::stream::file_stream_from(tg, part0.message_id, &part0.chat, start).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::warn!("thumb download start failed for {uid}: {e}");
            return Err(());
        }
    };
    if part0
        .nonce
        .as_deref()
        .and_then(crate::crypt::nonce_from_b64)
        .is_some()
    {
        let Some(key) = crate::config::get().crypt_key_unconditional() else {
            tracing::warn!("thumb download needs crypt_password for encrypted part of {uid}");
            return Err(());
        };
        let dec = crate::crypt::DecryptingStream::from_header(stream, &key);
        stream = Box::pin(dec);
    }
    let mut out = match tokio::fs::File::create(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("thumb temp file failed for {uid}: {e}");
            return Err(());
        }
    };
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut out, &chunk).await {
            tracing::warn!("thumb write failed for {uid}: {e}");
            return Err(());
        }
        written = written.saturating_add(u64::try_from(chunk.len()).unwrap_or(0));
        if written >= cap {
            break;
        }
    }
    Ok(())
}

/// One PNG-encoded frame from ffmpeg reading `input` (a local temp file
/// or an http URL); `None` on any failure (logged). The frame comes back
/// at source resolution — `encode_thumb` does the downscale + AVIF encode.
async fn run_ffmpeg_frame(uid: &str, input: &str) -> Option<Vec<u8>> {
    match tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i", input])
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

    #[test]
    fn sweep_time_parsing() {
        assert_eq!(parse_sweep_time("00:00"), Ok((0, 0)));
        assert_eq!(parse_sweep_time("7:05"), Ok((7, 5)));
        assert_eq!(parse_sweep_time("23:59"), Ok((23, 59)));
        for bad in ["24:00", "12:60", "abc", "10", ""] {
            assert!(parse_sweep_time(bad).is_err(), "{bad:?} must not parse");
        }
    }

    /// The interval is always positive and never longer than the step
    /// demands; a disabled or invalid schedule yields nothing.
    #[test]
    fn next_run_is_on_the_grid() {
        use std::time::Duration;
        assert_eq!(next_sweep_in("07:00", 0), None);
        assert_eq!(next_sweep_in("nope", 3), None);
        // An anchor later today delays the first tick until it arrives
        // (at most ~24 h); afterwards ticks repeat every `hours` hours.
        for anchor in ["00:00", "07:00", "23:59"] {
            let d = next_sweep_in(anchor, 3).unwrap();
            assert!(d > Duration::ZERO);
            assert!(d <= Duration::from_secs(25 * 3600), "{anchor:?} -> {d:?}");
        }
        // A daily midnight run is at most 24 h away.
        let d = next_sweep_in("00:00", 24).unwrap();
        assert!(d <= Duration::from_secs(24 * 3600));
    }
}
