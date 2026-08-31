#![allow(
    clippy::arithmetic_side_effects, // part/size offset math bounded by declared sizes
    clippy::as_conversions,          // u64/usize bridging of byte counts and offsets
    clippy::cast_possible_truncation, // offsets fit usize on 64-bit hosts; sizes < 2^32
    clippy::cast_possible_wrap,       // u64 sizes to i64/i32 for Telegram: < i64::MAX
    clippy::indexing_slicing,         // part-index slicing guarded by idx < nparts
)]
use std::io;
use std::sync::Arc;

use axum::extract::multipart::Multipart;
use axum::{Extension, Json};

use crate::auth::Caller;
use crate::db::FileRow;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{FileDto, HEAD_CAP, bytes_repr, cleanup_parts, extract_media_thumb, now_unix};

/// A part-upload failure safe to retry by re-reading the spill buffer: the
/// socket to Telegram's DC was torn down mid-transfer (ECONNRESET /
/// ECONNABORTED / connect timeout / refused), killing the in-flight stream
/// but not the buffered part. Anything else — an RPC rejection such as a
/// flood wait, an invalid file, a permission error — would fail every retry
/// identically, so leave it to fail fast.
pub(super) fn is_transient(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    [
        "connection reset by peer",
        "connection aborted",
        "broken pipe",
        "established connection was aborted",
        "connection timed out",
        "timed out",
        "connection refused",
    ]
    .iter()
    .any(|frag| low.contains(frag))
}

/// Extra attempts (besides the first) a part upload gets after a transient
/// transport failure; each re-reads its part from the spill buffer.
pub(super) const PART_RETRIES: u32 = 2;

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
/// One spawned part upload: message id, name echo, mime echo, thumb, and
/// the `base64` per-part crypto nonce (`None` for plaintext parts).
pub type UploaderHandle =
    tokio::task::JoinHandle<Result<(i32, String, String, Option<Vec<u8>>, Option<String>), String>>;

/// A file fully described and already uploaded: everything `persist_row`
/// needs to record it.
pub(super) struct StoredFile {
    pub(super) name: String,
    pub(super) mime: String,
    pub(super) declared: u64,
    pub(super) folder: String,
    pub(super) parts: Vec<crate::db::FilePart>,
    pub(super) thumb: Option<Vec<u8>>,
    pub(super) head: Vec<u8>,
}

#[allow(clippy::too_many_lines)] // linear multipart-drain + part-fan-out handler; splitting fragments it
pub async fn upload_file(
    Extension(Caller(uid)): Extension<Caller>,
    headers: axum::http::HeaderMap,
    multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let tg = state.tg(uid).await?;

    // The Telegram uploader needs the exact byte count up front; the
    // client provides it.
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
            .is_none_or(|f| f.owner != uid)
    {
        return Err(ApiError::bad_request("folder not found"));
    }

    // The spill path is the only upload path now: buffer the body to
    // `spill_dir` first, then drain each part (PartPlan::new owns the
    // split/threshold/channel logic). There is no stream path anymore.
    spill_upload(state, tg, uid, multipart, declared, max, folder).await
}

/// Per-part split shared by both spill paths: sizes, count and channel
/// rotation depend only on the user's settings and the declared size.
#[derive(Clone)]
pub(super) struct PartPlan {
    pub(super) declared: u64,
    pub(super) part_size: u64,
    pub(super) nparts: usize,
    over_cap: bool,
    base: usize,
    chats: Vec<String>,
}

impl PartPlan {
    pub(super) async fn new(state: &AppState, uid: i64, declared: u64) -> ApiResult<Self> {
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

    pub(super) const fn expected(&self, i: usize) -> u64 {
        if i + 1 == self.nparts {
            self.declared - self.part_size * (self.nparts as u64 - 1)
        } else {
            self.part_size
        }
    }

    pub(super) fn chat_for(&self, i: usize) -> String {
        let idx = if self.over_cap {
            self.base
        } else {
            self.base + i
        };
        self.chats[idx % self.chats.len()].clone()
    }
}

#[allow(clippy::too_many_lines)] // spill fan-out: parallel pumper+uploader wiring is inherently long
async fn spill_upload(
    state: &'static AppState,
    tg: Arc<crate::tg::TgManager>,
    uid: i64,
    mut multipart: Multipart,
    declared: u64,
    max: u64,
    folder: String,
) -> ApiResult<Json<serde_json::Value>> {
    use tokio::io::AsyncWriteExt;
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
            let base_r =
                tokio_util::io::StreamReader::new(tokio_stream::wrappers::ReceiverStream::new(rx));
            // Encrypt the part when at-rest encryption is on; the nonce
            // rides back with the result for the row.
            let mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>;
            let mut upload_size = expected;
            let mut nonce: Option<String> = None;
            match crate::config::get().crypt_key() {
                Ok(Some(key)) => {
                    let (er, used) = crate::crypt::EncryptingReader::new(base_r, &key);
                    reader = Box::new(er);
                    nonce = Some(crate::crypt::base64_encode(&used));
                    upload_size = crate::crypt::encrypted_size(expected);
                }
                _ => reader = Box::new(base_r),
            }
            #[allow(clippy::large_futures)] // spawned upload future is unavoidably large
            let (mid, _, _, thumb) = tg
                .upload(&mut reader, upload_size, &part_name, &mime, &chat)
                .await?;
            Ok((mid, part_name, mime, thumb, nonce))
        }));
    }

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
            mime = f.content_type().map_or_else(
                || {
                    mime_guess::from_path(&name)
                        .first_or_octet_stream()
                        .to_string()
                },
                str::to_string,
            );
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
                        for w in &mut writers {
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
    for w in &mut writers {
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
    // Telegram makes no stripped thumbnail for audio; fall back to the
    // embedded cover art captured from the stream head.
    let thumb = match tg_thumb.or_else(|| {
        (mime.starts_with("audio/"))
            .then(|| crate::art::extract(&head))
            .flatten()
    }) {
        Some(jpeg) => super::thumbs::thumb_bytes(jpeg).await,
        None => None,
    };
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
            thumb,
            head: head.clone(),
        },
    )
    .await
}

/// Awaits every part uploader, collecting landed message ids; on any
/// failure the already-posted parts are removed so no orphans stay behind.
pub(super) async fn collect_uploaders(
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
            Ok((message_id, _, _, thumb, nonce)) => {
                if tg_thumb.is_none() {
                    tg_thumb = thumb;
                }
                parts.push(crate::db::FilePart {
                    message_id,
                    chat: plan.chat_for(i),
                    size: plan.expected(i) as i64,
                    nonce,
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
/// it.  Used as a fallback when the overlap uploaders have already been
/// consumed (e.g. after a failed `complete` that the client retries).
#[allow(clippy::too_many_arguments)] // faithful port of the original; splitting adds indirection
pub(super) async fn store_from_file(
    state: &'static AppState,
    tg: Arc<crate::tg::TgManager>,
    uid: i64,
    path: &std::path::Path,
    declared: u64,
    name: &str,
    mime: &str,
    folder: &str,
    head: &[u8],
) -> ApiResult<Json<serde_json::Value>> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let plan = PartPlan::new(state, uid, declared).await?;
    let nparts = plan.nparts;
    let resolved_mime = if mime.is_empty() {
        mime_guess::from_path(name)
            .first_or_octet_stream()
            .to_string()
    } else {
        mime.to_string()
    };

    let mut uploaders = Vec::with_capacity(plan.nparts);
    for i in 0..plan.nparts {
        let expected = plan.expected(i);
        let tg = tg.clone();
        let chat = plan.chat_for(i);
        let path = path.to_path_buf();
        let name = name.to_string();
        let resolved_mime = resolved_mime.clone();
        let part_size = plan.part_size;
        let nparts = plan.nparts;
        uploaders.push(tokio::spawn(async move {
            let part_name = if nparts > 1 {
                format!("{name}.part{:03}", i + 1)
            } else {
                name
            };
            let mut attempt: u32 = 0;
            loop {
                let mut f = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| format!("reopen upload buffer: {e}"))?;
                f.seek(std::io::SeekFrom::Start(i as u64 * part_size))
                    .await
                    .map_err(|e| format!("seek upload buffer: {e}"))?;
                let r = f.take(expected);
                let mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>;
                let mut upload_size = expected;
                let mut nonce: Option<String> = None;
                match crate::config::get().crypt_key() {
                    Ok(Some(key)) => {
                        let (er, used) = crate::crypt::EncryptingReader::new(r, &key);
                        reader = Box::new(er);
                        nonce = Some(crate::crypt::base64_encode(&used));
                        upload_size = crate::crypt::encrypted_size(expected);
                    }
                    _ => reader = Box::new(r),
                }
                #[allow(clippy::large_futures)]
                let res = tg
                    .upload(&mut reader, upload_size, &part_name, &resolved_mime, &chat)
                    .await;
                match res {
                    Ok((mid, _, _, thumb)) => {
                        break Ok((mid, part_name, resolved_mime.clone(), thumb, nonce));
                    }
                    Err(e) if attempt < PART_RETRIES && is_transient(&e) => {
                        attempt += 1;
                        tracing::info!(
                            part = %part_name,
                            attempt,
                            err = %e,
                            "transient part upload failure — retrying from the spill buffer"
                        );
                        tracing::info!(
                            part = %part_name,
                            attempt,
                            "retry starting"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }));
    }

    let (parts, tg_thumb) = collect_uploaders(uploaders, &plan, &tg).await?;

    let thumb = match tg_thumb.or_else(|| {
        (resolved_mime.starts_with("audio/"))
            .then(|| crate::art::extract(head))
            .flatten()
    }) {
        Some(jpeg) => super::thumbs::thumb_bytes(jpeg).await,
        None => None,
    };
    tracing::info!(%name, parts = nparts, %declared, "uploaded file (fallback)");
    persist_row(
        state,
        tg,
        uid,
        StoredFile {
            name: name.to_string(),
            mime: resolved_mime,
            declared,
            folder: folder.to_string(),
            parts,
            thumb,
            head: head.to_vec(),
        },
    )
    .await
}

pub(super) async fn persist_row(
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
        mut thumb,
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
    if thumb.is_none()
        && mime.starts_with("audio/")
        && let Some(img) = crate::art::extract(head)
        && let Some(avif) = super::thumbs::thumb_bytes(img).await
    {
        thumb = Some(avif);
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
    };
    if let Err(e) = crate::db::insert(&state.db, &row).await {
        // No metadata row means the messages are unmanageable orphans.
        cleanup_parts(&tg, &row.parts).await;
        return Err(e.into());
    }
    if let Some(avif) = thumb
        && let Err(e) = super::thumbs::write(&state.thumbs_dir, &row.uid, &avif).await
    {
        // The row exists and is served fine; only the preview is missing,
        // and the regeneration gate below covers it on a later upload.
        tracing::warn!("thumb write failed for {}: {e}", row.uid);
    }
    if crate::state::get().instance().media_thumbs
        && !super::thumbs::exists(&state.thumbs_dir, &row.uid)
        && (row.mime.starts_with("video/") || row.mime.starts_with("image/"))
    {
        let file_uid = row.uid.clone();
        let mime = row.mime.clone();
        let part0 = row.parts[0].clone();
        // The state is a `&'static`, so a detached task can borrow it
        // outright — nothing else to clone into the task.
        tokio::spawn(async move {
            extract_media_thumb(state, uid, &file_uid, &mime, part0).await;
        });
    }
    let has_thumb = super::thumbs::exists(&state.thumbs_dir, &row.uid);
    Ok(Json(
        serde_json::json!({ "file": FileDto::new(row, has_thumb) }),
    ))
}
