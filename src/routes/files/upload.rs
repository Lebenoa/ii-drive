use std::io;
use std::sync::Arc;

use axum::extract::multipart::Multipart;
use axum::{Extension, Json};
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::Caller;
use crate::db::FileRow;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{
    DRAIN_CAP, FileDto, HEAD_CAP, bytes_repr, cleanup_parts, extract_media_thumb, now_unix,
};

/// Per-request body-limit guard for POST /api/files. The limit cannot be
/// baked into the router at startup: the cap is an instance setting an
/// operator can change while the server runs, and a frozen copy kept
/// rejecting uploads after the cap was raised.
/// Browsers always send Content-Length, so this refuses oversized bodies
/// before any bytes move; absent or lying headers fall through to the
/// handler's streaming check, which enforces the same live limit.
pub async fn upload_limit(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    // Multipart overhead beyond the raw file bytes: boundary, headers.
    const SLACK: u64 = 1024 * 1024;
    let max = crate::state::get()
        .instance()
        .max_file_size
        .saturating_add(SLACK);
    let declared_len = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(len) = declared_len
        && len > max
    {
        return Err(ApiError::too_large(format!(
            "request body of {len} bytes exceeds limit of {} bytes",
            max - SLACK
        )));
    }
    Ok(next.run(req).await)
}

/// One spawned part upload: message id, name echo, mime echo, thumb.
pub(crate) type UploaderHandle =
    tokio::task::JoinHandle<Result<(i32, String, String, Option<Vec<u8>>), String>>;

/// A file fully described and already uploaded: everything persist_row
/// needs to record it.
struct StoredFile {
    name: String,
    mime: String,
    declared: u64,
    folder: String,
    parts: Vec<crate::db::FilePart>,
    thumb_b64: Option<String>,
    head: Vec<u8>,
}

pub async fn upload_file(
    Extension(Caller(uid)): Extension<Caller>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    // Everything below runs on the caller's own account: their client posts
    // the parts, into their channels, under their routing rules.
    let tg = state.tg(uid).await?;
    let user_key = uid.to_string();

    // grammers needs the exact byte count up front; the client provides it.
    let declared: u64 = headers
        .get("x-file-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::bad_request("missing X-File-Size header"))?;

    let max = crate::state::get().instance().max_file_size;
    if declared > max {
        return Err(ApiError::too_large(format!(
            "file exceeds limit of {} bytes",
            bytes_repr(max)
        )));
    }

    // Telegram stores each document separately and caps its size (2 GiB on
    // a free account) regardless of the configured drive limit — so a
    // `max_file_size` above that is honored by transparently chunking the
    // file into cap-sized documents and re-joining them on download.
    //
    // The real ceiling is PART COUNT, not bytes: big-file uploads may use
    // at most ~4000 parts of 512 KiB each, so a document at the full 2 GiB
    // (exactly 4096 parts) is refused with FILE_PARTS_INVALID — observed on
    // a plain user session, so it is an account-level budget, not a bot
    // quirk. Cap documents at 4000 × 512 KiB; anything larger just becomes
    // more documents, which downloads re-join anyway.
    const TG_DOC_CAP: u64 = 4000 * 512 * 1024; // 2_048_000_000 ≈ 1.91 GiB
    let over_cap = declared > TG_DOC_CAP;

    // Target folder comes from a header ("" = root); multipart bodies cannot
    // carry extra fields alongside the streamed file field.
    let folder = headers
        .get("x-folder")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if !folder.is_empty()
        && !crate::db::get_folder(&state.db, &folder)
            .await?
            .is_some_and(|f| f.owner == uid)
    {
        return Err(ApiError::bad_request("folder not found"));
    }

    // Split into parallel parts when the user enabled a threshold and the
    // file is large enough. Parts upload concurrently over separate
    // connections, which is markedly faster for big files — especially with
    // several download bots, since each part message can be fetched by a
    // different bot under its own rate limit.
    let split_bytes = crate::db::get_split(&state.db, &user_key)
        .await
        .unwrap_or(0);
    // Chunk size: the user's split threshold when set (0 = off), never
    // above Telegram's per-document cap. Over-cap files always chunk —
    // they cannot fit a single document.
    let part_size = if !over_cap && (split_bytes == 0 || declared <= split_bytes) {
        declared.max(1)
    } else if split_bytes > 0 {
        split_bytes.min(TG_DOC_CAP)
    } else {
        TG_DOC_CAP
    };
    let nparts = declared.div_ceil(part_size).max(1) as usize;
    if nparts > 64 {
        return Err(ApiError::bad_request(format!(
            "file would need {nparts} parts (max 64)"
        )));
    }

    // Spill trades temporary disk for a decoupled drain: the whole body is
    // buffered before any part starts uploading, so Telegram's aggregate
    // rate is never throttled behind this request's sequential body feed.
    if crate::state::get().instance().upload_strategy == crate::db::UploadStrategy::Spill {
        return spill_upload(state, tg, uid, multipart, declared, max, folder).await;
    }

    // Storage target per part: round-robin across the user's selected
    // channels. There is no fallback — the web UI forces channel selection
    // right after login, so reaching this point without channels means an
    // out-of-band API caller skipped setup.
    let selected = crate::db::get_channels(&state.db, &user_key)
        .await
        .unwrap_or_default();
    if selected.is_empty() {
        return Err(ApiError::bad_request(
            "no storage channels selected — complete channel selection in the drive first",
        ));
    }
    // Over-cap chunked files keep ALL parts in one channel — a single
    // file re-joined from parts of one chat is cleaner to fetch and audit.
    // Which channel is chosen rotates per upload so parallel large files
    // spread across the selection; ordinary split uploads keep the
    // per-part round-robin.
    let base = tg.next_rotation();
    let chat_for = |i: usize| -> String {
        let idx = if over_cap { base } else { base + i };
        selected[idx % selected.len()].chat.to_ascii_lowercase()
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
        let tg = tg.clone();
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
            tg.upload(&mut reader, expected, &part_name, &mime, &chat)
                .await
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
                mime_guess::from_path(&name)
                    .first_or_octet_stream()
                    .to_string()
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
                            let idx =
                                (((before + off as u64) / part_size) as usize).min(nparts - 1);
                            let part_end = ((idx as u64 + 1) * part_size).min(declared) as usize;
                            let take = (chunk.len() - off).min(part_end - (before as usize + off));
                            if txs[idx]
                                .send(Ok(chunk.slice(off..off + take)))
                                .await
                                .is_err()
                            {
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
        cleanup_parts(&tg, &parts).await;
        // "upload aborted" means an uploader died first; its error is the
        // actual cause (e.g. Telegram down) and far more useful to the client.
        if err.1 == "upload aborted"
            && let Some(e) = uploader_err
        {
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
                if thumb_b64.is_none()
                    && let Some(jpeg) = thumb
                {
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
        cleanup_parts(&tg, &parts).await;
        return Err(ApiError::bad_request(e));
    }

    if fed != declared {
        // The messages may already be live in Telegram (a client that lied
        // low in X-File-Size); remove them so storage matches the error.
        cleanup_parts(&tg, &parts).await;
        return Err(ApiError::bad_request(format!(
            "uploaded {fed} bytes but X-File-Size declared {declared}"
        )));
    }
    let (name, mime) = meta_rx.borrow().clone().unwrap_or_default();
    persist_row(
        state,
        tg,
        uid,
        StoredFile {
            name,
            mime,
            declared,
            folder,
            parts,
            thumb_b64,
            head: head.clone(),
        },
    )
    .await
}

/// Per-part split shared by both spill paths: sizes, count and channel
/// rotation depend only on the user's settings and the declared size.
struct PartPlan {
    declared: u64,
    part_size: u64,
    nparts: usize,
    over_cap: bool,
    base: usize,
    chats: Vec<String>,
}

impl PartPlan {
    async fn new(state: &AppState, uid: i64, declared: u64) -> ApiResult<Self> {
        const TG_DOC_CAP: u64 = 4000 * 512 * 1024;
        let user_key = uid.to_string();
        let split_bytes = crate::db::get_split(&state.db, &user_key)
            .await
            .unwrap_or(0);
        let over_cap = declared > TG_DOC_CAP;
        let part_size = if !over_cap && (split_bytes == 0 || declared <= split_bytes) {
            declared.max(1)
        } else if split_bytes > 0 {
            split_bytes.min(TG_DOC_CAP)
        } else {
            TG_DOC_CAP
        };
        let nparts = declared.div_ceil(part_size).max(1) as usize;
        if nparts > 64 {
            return Err(ApiError::bad_request(format!(
                "file would need {nparts} parts (max 64)"
            )));
        }
        let chats = crate::db::get_channels(&state.db, &user_key)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.chat.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if chats.is_empty() {
            return Err(ApiError::bad_request(
                "no storage channels selected — complete channel selection in the drive first",
            ));
        }
        let base = state.tg(uid).await?.next_rotation();
        Ok(Self {
            declared,
            part_size,
            nparts,
            over_cap,
            base,
            chats,
        })
    }

    fn expected(&self, i: usize) -> u64 {
        if i + 1 == self.nparts {
            self.declared - self.part_size * (self.nparts as u64 - 1)
        } else {
            self.part_size
        }
    }

    fn chat_for(&self, i: usize) -> String {
        let idx = if self.over_cap {
            self.base
        } else {
            self.base + i
        };
        self.chats[idx % self.chats.len()].clone()
    }
}

async fn spill_upload(
    state: &'static AppState,
    tg: Arc<crate::tg::TgManager>,
    uid: i64,
    mut multipart: Multipart,
    declared: u64,
    max: u64,
    folder: String,
) -> ApiResult<Json<serde_json::Value>> {
    let plan = PartPlan::new(state, uid, declared).await?;
    let dir = std::path::PathBuf::from(&crate::config::get().spill_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::bad_request(format!("spill dir unavailable: {e}")))?;
    let stem = ulid::Ulid::generate().to_string();

    // One buffer per part, so a part's Telegram upload starts as soon as
    // its own range begins filling instead of after the whole body landed.
    //
    // Name/mime only arrive once the multipart field is located; uploaders
    // hold on those before touching Telegram so parts post with the real
    // file identity, not a placeholder.
    let (meta_tx, meta_rx) = tokio::sync::watch::channel(None::<(String, String)>);
    let feed_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::with_capacity(plan.nparts);
    let mut pumpers: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(plan.nparts);
    let mut uploaders = Vec::with_capacity(plan.nparts);
    for i in 0..plan.nparts {
        let path = dir.join(format!("{stem}.p{i}"));
        writers.push(tokio::io::BufWriter::with_capacity(
            128 * 1024,
            tokio::fs::File::create(&path).await.map_err(|e| {
                ApiError::bad_request(format!("could not create upload buffer: {e}"))
            })?,
        ));

        // Pumper: tails its part file until `expected` bytes have flowed,
        // tolerating EOF while the feed is still running. Uploaders read
        // through a plain channel-backed stream; tokio's watch has no
        // polling API to support a reader that waits on the writer.
        let expected = plan.expected(i);
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<bytes::Bytes>>(8);
        let done = feed_done.clone();
        let pumper_path = path.clone();
        pumpers.push(tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut f = match tokio::fs::File::open(&pumper_path).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            let mut buf = vec![0u8; 128 * 1024];
            let mut got: u64 = 0;
            while got < expected {
                match f.read(&mut buf).await {
                    Ok(0) => {
                        if done.load(std::sync::atomic::Ordering::SeqCst) {
                            return; // truncated part — uploader sees short stream
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Ok(n) => {
                        got += n as u64;
                        if tx
                            .send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                            .await
                            .is_err()
                        {
                            return; // uploader gone
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }));

        let tg = tg.clone();
        let chat = plan.chat_for(i);
        let expected = plan.expected(i);
        let index = i;
        let nparts = plan.nparts;
        let mut meta = meta_rx.clone();
        uploaders.push(tokio::spawn(async move {
            fn snapshot(
                meta: &tokio::sync::watch::Receiver<Option<(String, String)>>,
            ) -> Option<(String, String)> {
                meta.borrow().clone()
            }
            let (file_name, mime) = match snapshot(&meta) {
                Some(v) => v,
                None => match meta.changed().await {
                    Ok(()) => snapshot(&meta).unwrap_or_default(),
                    Err(_) => return Err("multipart field `file` missing".to_string()),
                },
            };
            if file_name.is_empty() {
                return Err("multipart field `file` missing".to_string());
            }
            let part_name = if nparts > 1 {
                format!("{file_name}.part{:03}", index + 1)
            } else {
                file_name
            };
            let mut r =
                tokio_util::io::StreamReader::new(tokio_stream::wrappers::ReceiverStream::new(rx));
            tg.upload(&mut r, expected, &part_name, &mime, &chat).await
        }));
    }

    use tokio::io::AsyncWriteExt;
    let mut name = String::new();
    let mut mime = String::new();
    let mut fed: u64 = 0;
    // Stream head stays buffered: cover art lives in leading audio tags.
    let mut head: Vec<u8> = Vec::new();

    let feed_result: ApiResult<()> = async {
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
            name = f.file_name().unwrap_or("unnamed").to_string();
            mime = f.content_type().map(|s| s.to_string()).unwrap_or_else(|| {
                mime_guess::from_path(&name)
                    .first_or_octet_stream()
                    .to_string()
            });
            let _ = meta_tx.send(Some((name.clone(), mime.clone())));
            loop {
                match f.chunk().await {
                    Ok(Some(chunk)) => {
                        fed += chunk.len() as u64;
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
                            let idx = (((fed - chunk.len() as u64 + off as u64) / plan.part_size)
                                as usize)
                                .min(plan.nparts - 1);
                            let part_end =
                                ((idx as u64 + 1) * plan.part_size).min(declared) as usize;
                            let take = (chunk.len() - off)
                                .min(part_end - (fed as usize - chunk.len() + off));
                            writers[idx]
                                .write_all(&chunk[off..off + take])
                                .await
                                .map_err(|e| {
                                    ApiError::bad_request(format!("write upload buffer: {e}"))
                                })?;
                            off += take;
                        }
                        for w in writers.iter_mut() {
                            w.flush().await.map_err(|e| {
                                ApiError::bad_request(format!("flush upload buffer: {e}"))
                            })?;
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

    // Either way the feed is over: pumpers see EOF as final, uploaders
    // finalize or fail fast, and the writers' last buffers must reach disk
    // before any pumper declares the part truncated.
    for w in writers.iter_mut() {
        let _ = w.flush().await;
    }
    drop(writers);
    feed_done.store(true, std::sync::atomic::Ordering::SeqCst);

    let result = async {
        feed_result?;
        if fed != declared {
            return Err(ApiError::bad_request(format!(
                "uploaded {fed} bytes but X-File-Size declared {declared}"
            )));
        }
        if name.is_empty() {
            return Err(ApiError::bad_request("multipart field `file` missing"));
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        for u in uploaders {
            u.abort();
        }
        // Pumpers hold open handles to the part files; on Windows a delete
        // against an open handle fails, so let them exit first.
        for p in pumpers {
            let _ = p.await;
        }
        for i in 0..plan.nparts {
            let _ = tokio::fs::remove_file(dir.join(format!("{stem}.p{i}"))).await;
        }
        return Err(e);
    }

    let (parts, tg_thumb) = collect_uploaders(uploaders, &plan, &tg).await?;
    for p in pumpers {
        let _ = p.await;
    }
    for i in 0..plan.nparts {
        let _ = tokio::fs::remove_file(dir.join(format!("{stem}.p{i}"))).await;
    }

    let mime = if mime.is_empty() {
        mime_guess::from_path(&name)
            .first_or_octet_stream()
            .to_string()
    } else {
        mime
    };
    let mut thumb_b64 = None;
    if let Some(jpeg) = tg_thumb.or_else(|| {
        // Telegram makes no stripped thumbnail for audio; fall back to the
        // embedded cover art captured from the stream head.
        (mime.starts_with("audio/"))
            .then(|| crate::art::extract(&head))
            .flatten()
    }) {
        use base64::Engine as _;
        thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
    }
    tracing::info!(%name, parts = plan.nparts, %declared, "uploaded file");
    persist_row(
        state,
        tg,
        uid,
        StoredFile {
            name,
            mime,
            declared,
            folder,
            parts,
            thumb_b64,
            head: head.clone(),
        },
    )
    .await
}

/// Awaits every part uploader, collecting landed message ids; on any
/// failure the already-posted parts are removed so no orphans stay behind.
async fn collect_uploaders(
    mut uploaders: Vec<UploaderHandle>,
    plan: &PartPlan,
    tg: &crate::tg::TgManager,
) -> ApiResult<(Vec<crate::db::FilePart>, Option<Vec<u8>>)> {
    let mut parts: Vec<crate::db::FilePart> = Vec::with_capacity(plan.nparts);
    let mut tg_thumb: Option<Vec<u8>> = None;
    for i in 0..uploaders.len() {
        match (&mut uploaders[i])
            .await
            .map_err(|e| format!("upload task failed: {e}"))
            .and_then(|r| r)
        {
            Ok((message_id, _, _, thumb)) => {
                if tg_thumb.is_none() {
                    tg_thumb = thumb;
                }
                parts.push(crate::db::FilePart {
                    message_id,
                    chat: plan.chat_for(i),
                    size: plan.expected(i) as i64,
                });
            }
            Err(e) => {
                // Stop the not-yet-joined uploads before rolling back: each
                // one would otherwise keep posting while we clean up.
                for rest in uploaders.into_iter().skip(i + 1) {
                    rest.abort();
                }
                cleanup_parts(tg, &parts).await;
                return Err(ApiError::bad_request(e));
            }
        }
    }
    Ok((parts, tg_thumb))
}

/// Fans a fully-buffered local file out to Telegram as parts, then records
/// it. Shared by the `spill` strategy and resumable uploads, which always
/// buffer by design.
/// A buffered local file plus its metadata, ready to fan out to Telegram.
pub(crate) struct FileInput<'a> {
    pub path: &'a std::path::Path,
    pub declared: u64,
    pub name: String,
    pub mime: String,
    pub folder: String,
}

pub(crate) async fn store_from_file(
    state: &'static AppState,
    tg: Arc<crate::tg::TgManager>,
    uid: i64,
    file: FileInput<'_>,
    head: &[u8],
) -> ApiResult<Json<serde_json::Value>> {
    let declared = file.declared;
    let name = file.name;
    let path = file.path;

    let plan = PartPlan::new(state, uid, declared).await?;
    let nparts = plan.nparts;
    let mime = if file.mime.is_empty() {
        mime_guess::from_path(&name)
            .first_or_octet_stream()
            .to_string()
    } else {
        file.mime
    };

    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
    let mut uploaders = Vec::with_capacity(plan.nparts);
    for i in 0..plan.nparts {
        let expected = plan.expected(i);
        let tg = tg.clone();
        let chat = plan.chat_for(i);
        let path = path.to_path_buf();
        let name = name.clone();
        let mime = mime.clone();
        let part_size = plan.part_size;
        let nparts = plan.nparts;
        uploaders.push(tokio::spawn(async move {
            let mut f = tokio::fs::File::open(&path)
                .await
                .map_err(|e| format!("reopen upload buffer: {e}"))?;
            f.seek(std::io::SeekFrom::Start(i as u64 * part_size))
                .await
                .map_err(|e| format!("seek upload buffer: {e}"))?;
            let mut r = f.take(expected);
            let part_name = if nparts > 1 {
                format!("{name}.part{:03}", i + 1)
            } else {
                name
            };
            tg.upload(&mut r, expected, &part_name, &mime, &chat).await
        }));
    }

    let (parts, tg_thumb) = collect_uploaders(uploaders, &plan, &tg).await?;

    let mut thumb_b64 = None;
    if let Some(jpeg) = tg_thumb.or_else(|| {
        // Telegram makes no stripped thumbnail for audio; fall back to the
        // embedded cover art captured from the buffer head.
        (mime.starts_with("audio/"))
            .then(|| crate::art::extract(head))
            .flatten()
    }) {
        use base64::Engine as _;
        thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
    }
    tracing::info!(%name, parts = nparts, %declared, "uploaded file");
    persist_row(
        state,
        tg,
        uid,
        StoredFile {
            name,
            mime,
            declared,
            folder: file.folder,
            parts,
            thumb_b64,
            head: head.to_vec(),
        },
    )
    .await
}

async fn persist_row(
    state: &'static AppState,
    tg: Arc<crate::tg::TgManager>,
    uid: i64,
    file: StoredFile,
) -> ApiResult<Json<serde_json::Value>> {
    let StoredFile {
        name,
        mime,
        declared,
        mut folder,
        parts,
        mut thumb_b64,
        head,
    } = file;
    let head = head.as_slice();
    // Auto-upload routing claims the file only when no explicit folder was
    // chosen; a stale rule falls through to the root rather than failing.
    if folder.is_empty()
        && !mime.is_empty()
        && let Ok(rules) = crate::db::get_rules(&state.db, &uid.to_string())
            .await
            .inspect_err(|e| tracing::warn!("routing rules unavailable, file stays in root: {e}"))
        && let Some(rule) = rules.iter().find(|r| mime.starts_with(r.mime.trim()))
        && crate::db::get_folder(&state.db, &rule.folder)
            .await
            .ok()
            .flatten()
            .is_some_and(|f| f.owner == uid)
    {
        folder = rule.folder.clone();
    }
    if thumb_b64.is_none()
        && mime.starts_with("audio/")
        && let Some(img) = crate::art::extract(head)
    {
        use base64::Engine as _;
        thumb_b64 = Some(base64::engine::general_purpose::STANDARD.encode(img));
    }
    let row = FileRow {
        owner: uid,
        uid: ulid::Ulid::generate().to_string(),
        name,
        mime,
        size: declared as i64,
        created_at: now_unix(),
        folder,
        parts,
        public: false,
        thumb: thumb_b64,
    };
    if let Err(e) = crate::db::insert(&state.db, &row).await {
        // No metadata row means the messages are unmanageable orphans.
        cleanup_parts(&tg, &row.parts).await;
        return Err(e.into());
    }
    if crate::state::get().instance().media_thumbs
        && row.thumb.is_none()
        && (row.mime.starts_with("video/") || row.mime.starts_with("image/"))
    {
        let file_uid = row.uid.clone();
        let part0 = row.parts[0].clone();
        // The state is a `&'static`, so a detached task can borrow it
        // outright — nothing to clone into the task.
        tokio::spawn(async move {
            extract_media_thumb(state, uid, &file_uid, part0).await;
        });
    }
    Ok(Json(serde_json::json!({ "file": FileDto::from(row) })))
}
