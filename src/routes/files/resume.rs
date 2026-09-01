use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::extract::Path;
use axum::{Extension, Json};
use tokio::sync::{Mutex, watch};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

use super::upload::{
    PartPlan, StoredFile, UploaderHandle, collect_uploaders, is_transient, persist_row,
    store_from_file, PART_RETRIES,
};

/// In-flight resumable uploads. Process-local on purpose: a restart
/// invalidates the spill files anyway, so durability would only preserve
/// garbage. Sessions expire after [`SESSION_TTL`] untouched.
struct Session {
    owner: i64,
    declared: u64,
    received: u64,
    name: String,
    mime: String,
    folder: String,
    path: PathBuf,
    last_touch: Instant,
    /// Broadcasts the total bytes received so far.  Background uploaders
    /// watch this to know when their part is fully on disk and can be
    /// pushed to Telegram.
    progress_tx: watch::Sender<u64>,
    /// Background uploaders spawned at `init` time — one per part.  Each
    /// waits for its byte range to land, then uploads immediately.
    uploaders: Vec<UploaderHandle>,
    /// Part split plan, kept so `complete` can map uploader results back
    /// to `FilePart` rows.
    part_plan: PartPlan,
    /// Guards against concurrent `complete` calls.  Two race-free
    /// completions would double-post parts to Telegram.
    complete_in_flight: Arc<AtomicBool>,
}

/// RAII guard that clears `complete_in_flight` when dropped, even on
/// early-return error paths.  The clear is synchronous (no async lock
/// reacquire, no spawn) so a retry always sees the updated flag.
struct CompleteGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for CompleteGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

const SESSION_TTL_SECS: u64 = 24 * 3600;

static SESSIONS: std::sync::LazyLock<Mutex<HashMap<String, Session>>> =
    std::sync::LazyLock::new(Mutex::default);

async fn sessions() -> tokio::sync::MutexGuard<'static, HashMap<String, Session>> {
    SESSIONS.lock().await
}

/// Sweeps expired sessions (deleting their spill files) while holding the
/// lock — cheap, since it runs at most once per new init.
async fn sweep(map: &mut HashMap<String, Session>) {
    let stale: Vec<String> = map
        .iter()
        .filter(|(_, s)| s.last_touch.elapsed().as_secs() > SESSION_TTL_SECS)
        .map(|(k, _)| k.clone())
        .collect();
    for id in stale {
        if let Some(s) = map.remove(&id) {
            // Abort any background uploaders before removing the spill file.
            for u in s.uploaders {
                u.abort();
            }
            let _ = tokio::fs::remove_file(&s.path).await;
        }
    }

    // Crash debris: part buffers (`<ulid>.pN`) from the always-spill upload
    // path have no session to expire them, so anything old in the spill dir
    // that no live session owns is garbage by definition.
    let dir = spill_path();
    let live: std::collections::HashSet<PathBuf> = map.values().map(|s| s.path.clone()).collect();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let p = entry.path();
        if live.contains(&p) {
            continue;
        }
        let expired = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age.as_secs() > SESSION_TTL_SECS);
        if expired {
            let _ = tokio::fs::remove_file(&p).await;
        }
    }
}

fn spill_path() -> PathBuf {
    PathBuf::from(&crate::config::get().spill_dir)
}

#[derive(serde::Deserialize)]
pub struct InitReq {
    size: u64,
    name: String,
    #[serde(default)]
    mime: String,
    #[serde(default)]
    folder: String,
}

// The sessions mutex guard must stay alive across the sweep+insert below.
// One handler per upload phase: validation, spill-buffer setup and the
// per-part uploader spawn form a single flow — splitting scatters it.
#[allow(clippy::too_many_lines)]
#[allow(clippy::significant_drop_tightening)]
pub async fn init(
    Extension(Caller(uid)): Extension<Caller>,
    Json(req): Json<InitReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    if req.size == 0 {
        return Err(ApiError::bad_request("size must be positive"));
    }
    if req.size > crate::state::get().instance().max_file_size {
        return Err(ApiError::too_large(format!(
            "file exceeds limit of {} bytes",
            crate::state::get().instance().max_file_size
        )));
    }
    if !req.folder.is_empty()
        && crate::db::get_folder(&state.db, &req.folder)
            .await?
            .is_none_or(|f| f.owner != uid)
    {
        return Err(ApiError::bad_request("folder not found"));
    }

    let dir = spill_path();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ApiError::bad_request(format!("spill dir unavailable: {e}")))?;
    let id = ulid::Ulid::generate().to_string();
    let path = dir.join(format!("{id}.upload"));
    tokio::fs::File::create(&path)
        .await
        .map_err(|e| ApiError::bad_request(format!("could not create upload buffer: {e}")))?;

    // Build the part plan and TgManager up front so background uploaders
    // can start as soon as bytes land.
    let tg = state.tg(uid).await?;
    let part_plan = PartPlan::new(state, uid, req.size).await?;
    let (progress_tx, progress_rx_init) = watch::channel(0u64);

    // Spawn one background uploader per part.  Each waits on the progress
    // channel until its byte range is fully on disk, then streams it to
    // Telegram immediately — no need to wait for `complete`.
    let mut uploaders = Vec::with_capacity(part_plan.nparts);
    for i in 0..part_plan.nparts {
        let expected = part_plan.expected(i);
        // i < nparts <= 64 (Telegram's per-file part cap), so the widened
        // arithmetic cannot wrap or overflow.
        #[allow(clippy::as_conversions, clippy::arithmetic_side_effects)]
        let part_end = ((i as u64 + 1) * part_plan.part_size).min(part_plan.declared);
        let tg = tg.clone();
        let chat = part_plan.chat_for(i);
        let path = path.clone();
        let name = req.name.clone();
        let mime = req.mime.clone();
        let part_size = part_plan.part_size;
        let nparts = part_plan.nparts;
        let mut progress_rx = progress_rx_init.clone();
        uploaders.push(tokio::spawn(async move {
            // Wait until our part is fully received.
            {
                let mut done = false;
                while !done {
                    let received = *progress_rx.borrow_and_update();
                    if received >= part_end {
                        done = true;
                    } else if progress_rx.changed().await.is_err() {
                        // Channel closed — session was aborted.
                        return Err("upload session ended".to_string());
                    }
                }
            }

            let part_name = if nparts > 1 {
                format!("{name}.part{:03}", i.saturating_add(1))
            } else {
                name
            };

            // Transient transport failures are retryable; the part is
            // fully buffered on disk, so re-open, re-seek and resend.
            let mut attempt: u32 = 0;
            loop {
                use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
                let mut f = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| format!("reopen upload buffer: {e}"))?;
                // i < nparts <= 64, so the widened arithmetic cannot wrap.
                #[allow(clippy::as_conversions, clippy::arithmetic_side_effects)]
                f.seek(std::io::SeekFrom::Start(i as u64 * part_size))
                    .await
                    .map_err(|e| format!("seek upload buffer: {e}"))?;
                let r = f.take(expected);
                // Encrypt the part when at-rest encryption is on.
                let mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>;
                let mut upload_size = expected;
                let mut nonce: Option<String> = None;
                match crate::config::get().crypt_key() {
                    Ok(Some(key)) => {
                        let (er, used) = crate::crypt::EncryptingReader::new(r, &key);
                        reader = Box::new(er);
                        nonce = Some(crate::crypt::nonce_b64(&used));
                        upload_size = crate::crypt::encrypted_size(expected);
                    }
                    _ => reader = Box::new(r),
                }
                #[allow(clippy::large_futures)]
                let res = tg
                    .upload(&mut reader, upload_size, &part_name, &mime, &chat)
                    .await;
                match res {
                    Ok((mid, _, _, thumb)) => {
                        break Ok((mid, part_name, mime.clone(), thumb, nonce));
                    }
                    Err(e) if attempt < PART_RETRIES && is_transient(&e) => {
                        attempt = attempt.saturating_add(1);
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

    {
        let mut guard = sessions().await;
        let map = &mut *guard;
        sweep(map).await;            map.insert(
            id.clone(),
            Session {
                owner: uid,
                declared: req.size,
                received: 0,
                name: req.name,
                mime: req.mime,
                folder: req.folder,
                path,
                last_touch: Instant::now(),
                progress_tx,
                uploaders,
                part_plan,
                complete_in_flight: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    Ok(Json(serde_json::json!({ "id": id })))
}

// The mutex guard lives through the whole function so the snapshot is coherent.
#[allow(clippy::significant_drop_tightening)]
async fn owned_session(uid: i64, id: &str) -> ApiResult<Session> {
    let mut guard = sessions().await;
    let map = &mut *guard;
    let s = map
        .get_mut(id)
        .filter(|s| s.owner == uid)
        .ok_or_else(|| ApiError::not_found("no such upload session"))?;
    s.last_touch = Instant::now();
    // Sessions live behind a mutex guard that must not cross handler awaits,
    // so callers get an owned snapshot instead of a reference.
    Ok(Session {
        owner: s.owner,
        declared: s.declared,
        received: s.received,
        name: s.name.clone(),
        mime: s.mime.clone(),
        folder: s.folder.clone(),
        path: s.path.clone(),
        last_touch: s.last_touch,
        // The remaining fields are only needed internally; uploaders are
        // taken in `complete` and progress_tx is shared via clone.
        progress_tx: s.progress_tx.clone(),
        uploaders: Vec::new(), // snapshot doesn't need the handles
        part_plan: s.part_plan.clone(),
        complete_in_flight: Arc::new(AtomicBool::new(false)),
    })
}

pub async fn status(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = owned_session(uid, &id).await?;
    Ok(Json(serde_json::json!({
        "received": s.received,
        "declared": s.declared,
    })))
}

/// Appends one chunk at exactly `X-Offset`. Strictly sequential appends keep
/// the spill file byte-identical to the original without any sparse-hole
/// bookkeeping, so a retry can never interleave.
pub async fn chunk(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    use tokio::io::AsyncWriteExt;
    let offset: u64 = headers
        .get("x-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::bad_request("missing X-Offset header"))?;

    let s = owned_session(uid, &id).await?;
    if offset != s.received {
        return Err(ApiError::conflict(format!(
            "offset mismatch: expected {}, got {}",
            s.received, offset
        )));
    }
    // received + chunk is bounded by the declared size; len() fits in u64.
    #[allow(clippy::arithmetic_side_effects, clippy::as_conversions)]
    if s.received + body.len() as u64 > s.declared {
        return Err(ApiError::bad_request("chunk exceeds declared size"));
    }
    let mut f = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&s.path)
        .await
        .map_err(|e| ApiError::bad_request(format!("reopen upload buffer: {e}")))?;
    if let Err(e) = f.write_all(&body).await.or(f.flush().await) {
        // Roll the file back to the last acknowledged byte so a retry
        // appends clean bytes instead of a torn tail.
        let _ = f.set_len(s.received).await;
        return Err(ApiError::bad_request(format!("append chunk: {e}")));
    }
    drop(f);

    // Bounded as above: never exceeds declared size.
    #[allow(clippy::arithmetic_side_effects, clippy::as_conversions)]
    let received = s.received + body.len() as u64;
    {
        let mut map = SESSIONS.lock().await;
        if let Some(s) = map.get_mut(&id) {
            s.received = received;
            s.last_touch = Instant::now();
            // Broadcast so background uploaders know their part is on disk.
            let _ = s.progress_tx.send(received);
        }
    }
    Ok(Json(serde_json::json!({ "received": received })))
}

/// Removes a spill file, retrying briefly: on Windows the delete fails
/// while this process still holds an append handle from an in-flight chunk,
/// and a silently failed delete would orphan the file until the TTL sweep.
async fn remove_spill(path: &std::path::Path) {
    for _ in 0..10 {
        if tokio::fs::remove_file(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}
pub async fn abort(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = owned_session(uid, &id).await?;
    // Cancel any background uploaders before removing the session.
    {
        let mut guard = SESSIONS.lock().await;
        if let Some(session) = guard.remove(&id) {
            for u in session.uploaders {
                u.abort();
            }
        }
    }
    remove_spill(&s.path).await;
    Ok(Json(serde_json::json!({ "aborted": true })))
}

// One completion orchestration: gate concurrent calls, join the
// overlap uploaders, derive the thumbnail, persist and clean up —
// splitting it would scatter the retry invariants.
#[allow(clippy::too_many_lines)]
#[allow(clippy::significant_drop_tightening)] // the sessions guard must span get_mut through the mem::take
pub async fn complete(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // The spill file must outlive a failed complete so the client can
    // retry; only a successful store retires it (the TTL sweeper catches
    // sessions that never complete).
    struct SpillFile(PathBuf);
    impl Drop for SpillFile {
        fn drop(&mut self) {
            let p = self.0.clone();
            tokio::spawn(async move {
                remove_spill(&p).await;
            });
        }
    }

    // Head bytes feed cover-art extraction; the file is fully flushed by
    // the chunk handler, so this read is safe mid-session.
    const HEAD_CAP: usize = super::HEAD_CAP;

    let state = crate::state::get();
    let tg = state.tg(uid).await?;

    // Take the session — uploaders are moved out so they can be joined.
    let s = {
        let mut guard = SESSIONS.lock().await;
        let s = guard
            .get_mut(&id)
            .filter(|s| s.owner == uid)
            .ok_or_else(|| ApiError::not_found("no such upload session"))?;
        if s.received != s.declared {
            return Err(ApiError::bad_request(format!(
                "incomplete upload: {} of {} bytes",
                s.received, s.declared
            )));
        }
        // Gate concurrent complete() calls: two racing completions
        // would double-post every part to Telegram.
        if s.complete_in_flight.load(Ordering::Acquire) {
            return Err(ApiError::conflict(
                "another complete() is already in progress".to_string(),
            ));
        }
        s.complete_in_flight.store(true, Ordering::Release);
        // Move uploaders and part_plan out; the session entry is removed
        // after a successful persist.
        let uploaders = std::mem::take(&mut s.uploaders);
        let part_plan = s.part_plan.clone();
        let name = s.name.clone();
        let mime = s.mime.clone();
        let folder = s.folder.clone();
        let path = s.path.clone();
        let declared = s.declared;
        Session {
            owner: s.owner,
            declared,
            received: s.received,
            name,
            mime,
            folder,
            path,
            last_touch: s.last_touch,
            progress_tx: s.progress_tx.clone(),
            uploaders,
            part_plan,
            complete_in_flight: Arc::clone(&s.complete_in_flight),
        }
    };
    // RAII guard: clears complete_in_flight synchronously on drop,
    // covering every exit path including early ? returns.
    let _guard = CompleteGuard {
        flag: Arc::clone(&s.complete_in_flight),
    };

    // Head bytes feed cover-art extraction; the file is fully flushed by
    // the chunk handler, so this read is safe mid-session.
    let mut head = Vec::new();
    {
        use tokio::io::AsyncReadExt as _;
        let f = tokio::fs::File::open(&s.path)
            .await
            .map_err(|e| ApiError::bad_request(format!("reopen upload buffer: {e}")))?;
        #[allow(clippy::as_conversions)]
        let cap = HEAD_CAP as u64;
        f.take(cap)
            .read_to_end(&mut head)
            .await
            .map_err(|e| ApiError::bad_request(format!("read upload buffer head: {e}")))?;
    }

    // If the overlap uploaders are still present, join them.  If they
    // were already consumed by a previous (failed) `complete` call, fall
    // back to the sequential `store_from_file` path which creates fresh
    // uploaders from the spill buffer.
    let result = if s.uploaders.is_empty() {
        // Overlap uploaders were consumed by a prior attempt.  Fall back
        // to sequential upload from the spill buffer.
        store_from_file(
            state,
            tg,
            uid,
            &s.path,
            s.declared,
            &s.name,
            &s.mime,
            &s.folder,
            &head,
        )
        .await
    } else {
        let (parts, tg_thumb) = collect_uploaders(s.uploaders, &s.part_plan, &tg).await?;
        let mime = if s.mime.is_empty() {
            mime_guess::from_path(&s.name)
                .first_or_octet_stream()
                .to_string()
        } else {
            s.mime.clone()
        };
        let thumb = match tg_thumb.or_else(|| {
            (mime.starts_with("audio/"))
                .then(|| crate::art::extract(&head))
                .flatten()
        }) {
            Some(jpeg) => super::thumbs::thumb_bytes(jpeg).await,
            None => None,
        };
        tracing::info!(name = %s.name, parts = s.part_plan.nparts, declared = s.declared, "uploaded file");
        let r = persist_row(
            state,
            tg.clone(),
            uid,
            StoredFile {
                name: s.name.clone(),
                mime,
                declared: s.declared,
                folder: s.folder.clone(),
                parts: parts.clone(),
                thumb,
                head,
            },
        )
        .await;
        // If parts were posted to Telegram but the DB row wasn't
        // created, clean them up so a retry via store_from_file
        // doesn't produce orphans.
        if r.is_err() {
            super::cleanup_parts(&tg, &parts).await;
        }
        r
    };

    // Only remove the session and spill file on success so the client
    // can retry on failure.
    if result.is_ok() {
        let _spill = SpillFile(s.path.clone());
        SESSIONS.lock().await.remove(&id);
    }
    // _guard drops here, synchronously clearing complete_in_flight —
    // covering both success (session already removed → flag gone) and
    // failure (flag reset so retries can proceed).
    result
}
