use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use axum::extract::Path;
use axum::{Extension, Json};
use tokio::sync::Mutex;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

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
            let _ = tokio::fs::remove_file(&s.path).await;
        }
    }

    // Crash debris: part buffers (`<ulid>.pN`) from the spill strategy have
    // no session to expire them, so anything old in the spill dir that no
    // live session owns is garbage by definition.
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

    {
        let mut guard = sessions().await;
        let map = &mut *guard;
        sweep(map).await;
        map.insert(
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
    })
}

async fn set_received(id: &str, n: u64) {
    let mut map = SESSIONS.lock().await;
    if let Some(s) = map.get_mut(id) {
        s.received = n;
        s.last_touch = Instant::now();
    }
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
    set_received(&id, received).await;
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
    remove_spill(&s.path).await;
    SESSIONS.lock().await.remove(&id);
    Ok(Json(serde_json::json!({ "aborted": true })))
}

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
    let s = owned_session(uid, &id).await?;
    if s.received != s.declared {
        return Err(ApiError::bad_request(format!(
            "incomplete upload: {} of {} bytes",
            s.received, s.declared
        )));
    }
    let mut head = Vec::new();
    {
        use tokio::io::AsyncReadExt as _;
        let f = tokio::fs::File::open(&s.path)
            .await
            .map_err(|e| ApiError::bad_request(format!("reopen upload buffer: {e}")))?;
        // HEAD_CAP (512 KiB) fits in u64.
        #[allow(clippy::as_conversions)]
        let cap = HEAD_CAP as u64;
        f.take(cap)
            .read_to_end(&mut head)
            .await
            .map_err(|e| ApiError::bad_request(format!("read upload buffer head: {e}")))?;
    }

    let file = super::upload::FileInput {
        path: &s.path,
        declared: s.declared,
        name: s.name.clone(),
        mime: s.mime.clone(),
        folder: s.folder.clone(),
    };
    match super::upload::store_from_file(state, tg, uid, file, &head).await {
        Ok(v) => {
            let _guard = SpillFile(s.path.clone());
            SESSIONS.lock().await.remove(&id);
            Ok(v)
        }
        Err(e) => Err(e),
    }
}
