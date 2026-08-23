use std::io;

use axum::body::Body;
use axum::extract::multipart::Multipart;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::Json;
use tokio_stream::wrappers::ReceiverStream;

use crate::db::FileRow;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Upper bound on how much of an aborted upload body we swallow just to
/// deliver the error response; beyond this the connection is simply closed.
const DRAIN_CAP: u64 = 32 * 1024 * 1024;

/// How much of the stream head to buffer for cover-art extraction.
const HEAD_CAP: usize = 512 * 1024;

/// Public upload limits so clients can pre-check files before transferring.
pub async fn limits() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "max_file_size": crate::config::get().max_file_size }))
}

#[derive(serde::Serialize)]
pub struct FileDto {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub created_at: i64,
    pub public: bool,
    /// true when a Telegram thumbnail exists for this file.
    pub has_thumb: bool,
}

impl From<FileRow> for FileDto {
    fn from(r: FileRow) -> Self {
        FileDto {
            id: r.uid,
            name: r.name,
            mime: r.mime,
            size: r.size,
            created_at: r.created_at,
            public: r.public,
            has_thumb: r.thumb.is_some(),
        }
    }
}

#[derive(serde::Deserialize)]
pub struct VisibilityBody {
    pub public: bool,
}

#[derive(serde::Deserialize)]
pub struct MoveBody {
    /// Target folder id, "" = root.
    pub folder: String,
}

/// PATCH /api/files/{id}/move — cut/paste target.
pub async fn move_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !body.folder.is_empty()
        && crate::db::get_folder(&state.db, &body.folder)
            .await?
            .is_none()
    {
        return Err(ApiError::bad_request("target folder not found"));
    }
    if !crate::db::set_folder(&state.db, &id, &body.folder).await? {
        return Err(ApiError::not_found("file not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// PATCH /api/files/{id}/visibility — flip private/public.
pub async fn set_visibility(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VisibilityBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !crate::db::set_public(&state.db, &id, body.public).await? {
        return Err(ApiError::not_found("file not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true, "public": body.public })))
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    /// Folder id to list; missing/empty means root.
    #[serde(default)]
    pub folder: Option<String>,
}

pub async fn list_files(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = crate::db::list(
        &state.db,
        q.q.as_deref().unwrap_or(""),
        q.folder.as_deref().unwrap_or(""),
        q.limit.unwrap_or(100).min(500),
        q.offset.unwrap_or(0),
    )
    .await?;
    let files: Vec<FileDto> = rows.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::json!({ "files": files })))
}

#[derive(serde::Deserialize)]
pub struct CreateFolderBody {
    pub name: String,
    #[serde(default)]
    pub parent: String,
}

/// POST /api/folders — create a folder (parent "" = root).
pub async fn create_folder(
    State(state): State<AppState>,
    Json(body): Json<CreateFolderBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(ApiError::bad_request("folder name must be 1-128 characters"));
    }
    if !body.parent.is_empty()
        && crate::db::get_folder(&state.db, &body.parent)
            .await?
            .is_none()
    {
        return Err(ApiError::bad_request("parent folder not found"));
    }
    let row = crate::db::FolderRow {
        uid: ulid::Ulid::generate().to_string(),
        name: name.to_string(),
        parent: body.parent,
    };
    crate::db::create_folder(&state.db, &row.uid, &row.name, &row.parent).await?;
    Ok(Json(serde_json::json!({ "folder": row })))
}

/// GET /api/folders — every folder, ordered by name.
pub async fn list_folders(
    State(state): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let folders = crate::db::list_folders(&state.db).await?;
    Ok(Json(serde_json::json!({ "folders": folders })))
}

/// DELETE /api/folders/{id} — only when empty (no files, no subfolders).
pub async fn delete_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if crate::db::get_folder(&state.db, &id).await?.is_none() {
        return Err(ApiError::not_found("folder not found"));
    }
    if !crate::db::folder_is_empty(&state.db, &id).await? {
        return Err(ApiError::bad_request("folder is not empty"));
    }
    crate::db::delete_folder(&state.db, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn upload_file(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    // grammers needs the exact byte count up front; the client provides it.
    let declared: u64 = headers
        .get("x-file-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::bad_request("missing X-File-Size header"))?;

    let max = crate::config::get().max_file_size;
    if declared > max {
        return Err(ApiError::too_large(format!(
            "file exceeds limit of {max} bytes"
        )));
    }

    // Target folder comes from a header ("" = root); multipart bodies cannot
    // carry extra fields alongside the streamed file field.
    let folder = headers
        .get("x-folder")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if !folder.is_empty()
        && crate::db::get_folder(&state.db, &folder)
            .await?
            .is_none()
    {
        return Err(ApiError::bad_request("folder not found"));
    }

    // Split into parallel parts when the user enabled a threshold and the
    // file is large enough. Parts upload concurrently over separate
    // connections, which is markedly faster for big files — especially with
    // several download bots, since each part message can be fetched by a
    // different bot under its own rate limit.
    let split_bytes = crate::db::get_split(&state.db).await.unwrap_or(0);
    let part_size = if split_bytes > 0 && declared > split_bytes {
        split_bytes
    } else {
        declared.max(1)
    };
    // More than 64 parts means the split threshold is far below the file
    // limit; refuse rather than silently merging the tail into one huge
    // last part.
    let nparts = declared.div_ceil(part_size).max(1) as usize;
    if nparts > 64 {
        return Err(ApiError::bad_request(format!(
            "file would split into {nparts} parts (max 64); raise the split threshold in settings"
        )));
    }

    // Storage target per part: round-robin across the user's selected
    // channels. There is no fallback — the web UI forces channel selection
    // right after login, so reaching this point without channels means an
    // out-of-band API caller skipped setup.
    let user_key = state.tg.current_user_id().await.map(|id| id.to_string());
    let selected = match &user_key {
        Some(k) => crate::db::get_channels(&state.db, k).await.unwrap_or_default(),
        None => Vec::new(),
    };
    if selected.is_empty() {
        return Err(ApiError::bad_request(
            "no storage channels selected — complete channel selection in the drive first",
        ));
    }
    let chat_for = |i: usize| -> String {
        selected[i % selected.len()].chat.to_ascii_lowercase()
    };

    // Uploaders start immediately (they must, to drain their channels while
    // we feed them); file name/mime arrive over a watch channel once the
    // multipart field is located. The field borrows `multipart`, so every
    // use of it stays inside one loop iteration.
    let (meta_tx, meta_rx) = tokio::sync::watch::channel(None::<(String, String)>);
    let mut txs = Vec::with_capacity(nparts);
    let mut uploaders = Vec::with_capacity(nparts);
    for i in 0..nparts {
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<bytes::Bytes>>(4);
        txs.push(tx);
        let mut reader =
            tokio_util::io::StreamReader::new(ReceiverStream::<io::Result<bytes::Bytes>>::new(rx));
        // The last part gets whatever remains.
        let expected = if i + 1 == nparts {
            declared - part_size * (nparts as u64 - 1)
        } else {
            part_size
        };
        let mut meta = meta_rx.clone();
        let tg = state.tg.clone();
        let chat = chat_for(i);
        uploaders.push(tokio::spawn(async move {
            // Helper keeps the watch guard from living across an await.
            fn snapshot(
                meta: &tokio::sync::watch::Receiver<Option<(String, String)>>,
            ) -> Option<(String, String)> {
                meta.borrow().clone()
            }
            let (name, mime) = match snapshot(&meta) {
                Some(v) => v,
                None => match meta.changed().await {
                    Ok(()) => snapshot(&meta).unwrap_or_default(),
                    Err(_) => (String::new(), String::new()),
                },
            };
            if name.is_empty() {
                return Err("multipart field `file` missing".to_string());
            }
            let part_name = if nparts > 1 {
                format!("{name}.part{:03}", i + 1)
            } else {
                name
            };
            tg.upload(&mut reader, expected, &part_name, &mime, &chat).await
        }));
    }

    let mut fed: u64 = 0;
    let mut head: Vec<u8> = Vec::new();
    let feed: Result<(), ApiError> = async {
        loop {
            let Some(mut f) = multipart
                .next_field()
                .await
                .map_err(|e| ApiError::bad_request(format!("bad multipart body: {e}")))?
            else {
                return Err(ApiError::bad_request("multipart field `file` missing"));
            };
            if f.name() != Some("file") {
                continue;
            }

            let name = f.file_name().unwrap_or("unnamed").to_string();
            let mime = f.content_type().map(|s| s.to_string()).unwrap_or_else(|| {
                mime_guess::from_path(&name).first_or_octet_stream().to_string()
            });
            let _ = meta_tx.send(Some((name, mime)));

            loop {
                match f.chunk().await {
                    Ok(Some(chunk)) => {
                        let before = fed;
                        fed += chunk.len() as u64;
                        // Keep the stream head: cover art lives in tags at
                        // the start of audio files (ID3 / FLAC metadata).
                        if head.len() < HEAD_CAP {
                            let take = (HEAD_CAP - head.len()).min(chunk.len());
                            head.extend_from_slice(&chunk[..take]);
                        }
                        if fed > max {
                            return Err(ApiError::too_large(format!(
                                "file exceeds limit of {max} bytes"
                            )));
                        }
                        // Route each byte to the part that owns its offset.
                        let mut off = 0usize;
                        while off < chunk.len() {
                            let idx = (((before + off as u64) / part_size) as usize).min(nparts - 1);
                            let part_end = ((idx as u64 + 1) * part_size).min(declared) as usize;
                            let take = (chunk.len() - off).min(part_end - (before as usize + off));
                            if txs[idx].send(Ok(chunk.slice(off..off + take))).await.is_err() {
                                return Err(ApiError::bad_request("upload aborted"));
                            }
                            off += take;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        return Err(ApiError::bad_request(format!("read upload stream: {e}")));
                    }
                }
            }
            break;
        }
        Ok(())
    }
    .await;

    if let Err(err) = feed {
        // Tell the uploaders the stream is dead so they never finalize
        // truncated uploads on Telegram.
        for tx in &txs {
            let _ = tx
                .send(Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "upload failed",
                )))
                .await;
        }
        // Consume what the client is still sending (bounded) so the error
        // response gets through instead of the connection being aborted.
        let mut drained: u64 = 0;
        while drained < DRAIN_CAP {
            match multipart.next_field().await {
                Ok(Some(mut f)) => {
                    while let Ok(Some(chunk)) = f.chunk().await {
                        drained += chunk.len() as u64;
                        if drained >= DRAIN_CAP {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        // Uploaders that had already finalized a part before the abort
        // posted live Telegram messages — collect and remove them so no
        // orphan stays behind (size is irrelevant for cleanup).
        let mut parts: Vec<crate::db::FilePart> = Vec::new();
        let mut uploader_err: Option<String> = None;
        for (i, u) in uploaders.into_iter().enumerate() {
            match u
                .await
                .map_err(|e| format!("upload task failed: {e}"))
                .and_then(|r| r)
            {
                Ok((message_id, _, _, _)) => parts.push(crate::db::FilePart {
                    message_id,
                    chat: chat_for(i),
                    size: 0,
                }),
                Err(e) if uploader_err.is_none() => uploader_err = Some(e),
                Err(_) => {}
            }
        }
        cleanup_parts(&state, &parts).await;
        // "upload aborted" means an uploader died first; its error is the
        // actual cause (e.g. Telegram down) and far more useful to the client.
        if err.1 == "upload aborted" && let Some(e) = uploader_err {
            return Err(ApiError::bad_request(e));
        }
        return Err(err);
    }
    drop(txs);

    // Collect one message id per part; on any failure, roll back the parts
    // that did land so no orphan stays behind.
    let mut parts: Vec<crate::db::FilePart> = Vec::with_capacity(nparts);
    let mut thumb_b64: Option<String> = None;
    let mut first_err: Option<String> = None;
    for (i, u) in uploaders.into_iter().enumerate() {
        let res = u
            .await
            .map_err(|e| format!("upload task failed: {e}"))
            .and_then(|r| r);
        match res {
            Ok((message_id, _, _, thumb)) => {
                if thumb_b64.is_none() && let Some(jpeg) = thumb {
                    use base64::Engine as _;
                    thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
                }
                parts.push(crate::db::FilePart {
                    message_id,
                    chat: chat_for(i),
                    size: if i + 1 == nparts {
                        declared - part_size * (nparts as u64 - 1)
                    } else {
                        part_size
                    } as i64,
                })
            }
            Err(e) => {
                first_err = Some(e);
                break;
            }
        }
    }
    if let Some(e) = first_err {
        cleanup_parts(&state, &parts).await;
        return Err(ApiError::bad_request(e));
    }

    if fed != declared {
        // The messages may already be live in Telegram (a client that lied
        // low in X-File-Size); remove them so storage matches the error.
        cleanup_parts(&state, &parts).await;
        return Err(ApiError::bad_request(format!(
            "uploaded {fed} bytes but X-File-Size declared {declared}"
        )));
    }
    let (name, mime) = meta_rx.borrow().clone().unwrap_or_default();
    // Auto-upload routing: when no folder was chosen, the first per-user
    // rule whose mime prefix matches claims the file. Stale rule folders
    // (deleted since) fall through to the root.
    let mut folder = folder;
    if folder.is_empty()
        && !mime.is_empty()
        && let Some(uid) = state.tg.current_user_id().await
            // Fail open to the root on a rules error, but never silently.
            && let Ok(rules) = crate::db::get_rules(&state.db, &uid.to_string())
                .await
                .inspect_err(|e| tracing::warn!("routing rules unavailable, file stays in root: {e}"))
            && let Some(rule) = rules
                .iter()
                .find(|r| mime.starts_with(r.mime.trim()))
            && crate::db::get_folder(&state.db, &rule.folder)
                .await
                .ok()
                .flatten()
                .is_some()
        {
            folder = rule.folder.clone();
    }
    // Telegram makes no stripped thumbnail for audio; pull embedded cover
    // art (ID3 APIC / FLAC PICTURE) from the buffered stream head instead.
    if thumb_b64.is_none()
        && mime.starts_with("audio/")
        && let Some(img) = crate::art::extract(&head)
    {
        use base64::Engine as _;
        thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(img));
    }
    tracing::info!(%name, parts = nparts, %declared, "uploaded file");
    let row = FileRow {
        uid: ulid::Ulid::generate().to_string(),
        name,
        mime,
        size: declared as i64,
        message_id: parts[0].message_id,
        chat: parts[0].chat.clone(),
        created_at: now_unix(),
        folder,
        parts,
        public: false,
        thumb: thumb_b64,
    };
    if let Err(e) = crate::db::insert(&state.db, &row).await {
        // No metadata row means the file is unmanageable; drop the messages
        // rather than leaving orphans in the storage chats.
        cleanup_parts(&state, &row.parts).await;
        return Err(e.into());
    }

    // Videos get a first-frame thumbnail and non-JPEG images a converted
    // one in the background (ffmpeg); JPEGs usually arrive with a stripped
    // Telegram thumb already stored above.
    if crate::config::get().media_thumbs
        && row.thumb.is_none()
        && (row.mime.starts_with("video/") || row.mime.starts_with("image/"))
    {
        let st = state.clone();
        let uid = row.uid.clone();
        let part0 = row.parts[0].clone();
        tokio::spawn(async move {
            extract_media_thumb(&st, &uid, part0).await;
        });
    }

    Ok(Json(serde_json::json!({ "file": FileDto::from(row) })))
}

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

fn ffmpeg_encoder() -> ThumbEncoder {
    use std::sync::OnceLock;
    static ENC: OnceLock<ThumbEncoder> = OnceLock::new();
    *ENC.get_or_init(|| {
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
    })
}

/// Downloads the first part of a freshly uploaded video or image (bounded),
/// extracts a 320px WebP thumbnail with ffmpeg (first frame for video,
/// downscale for images), and stores it on the row. Runs in the background;
/// failures are logged and simply leave no thumb.
pub async fn extract_media_thumb(state: &AppState, uid: &str, part0: crate::db::FilePart) {
    let encoder = tokio::task::block_in_place(ffmpeg_encoder);
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
    {
        use futures::StreamExt;
        let mut stream = match crate::stream::file_stream(&state.tg, part0.message_id, &part0.chat)
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

/// Best-effort removal of already-posted part messages after a failure.
async fn cleanup_parts(state: &AppState, parts: &[crate::db::FilePart]) {
    for p in parts {
        if let Err(e) = state.tg.delete_message(p.message_id, &p.chat).await {
            tracing::error!(message_id = p.message_id, "orphaned telegram message: {e}");
        }
    }
}

/// True when a delete failure only means the message was already gone —
/// retrying the file delete must still be able to succeed over those.
fn is_message_gone(err: &str) -> bool {
    let norm = err.to_lowercase().replace('_', "");
    norm.contains("messageidinvalid") || norm.contains("messageinvalid")
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    // Row first, then message: if the process dies in between, the API stays
    // consistent (file gone) and only Telegram holds an orphan. The reverse
    // order would leave a row pointing at a deleted message.
    let deleted = crate::db::delete(&state.db, &id).await?;
    if deleted == 0 {
        return Err(ApiError::not_found("file not found"));
    }
    // Delete every part's message; on the first failure restore the row so
    // the file stays listed and the delete can be retried. Parts that are
    // already gone on Telegram (e.g. a previous partial delete) are skipped
    // instead of failing the whole delete.
    let mut failed: Option<String> = None;
    for p in &row.parts {
        match state.tg.delete_message(p.message_id, &p.chat).await {
            Ok(()) => {}
            Err(e) if is_message_gone(&e) => {
                tracing::warn!(message_id = p.message_id, "part already deleted: {e}");
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failed {
        if let Err(re) = crate::db::insert(&state.db, &row).await {
            tracing::error!(uid = %row.uid, "cannot restore row after failed delete: {re}");
        }
        return Err(ApiError::internal(e));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parses `bytes=<start>-<end>` / `bytes=<start>-`; None otherwise.
fn parse_range(v: &str) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range unsupported
    }
    let (s, e) = spec.split_once('-')?;
    let start: u64 = s.parse().ok()?;
    let end: u64 = if e.is_empty() {
        u64::MAX
    } else {
        e.parse().ok()?
    };
    (start <= end).then_some((start, end))
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
