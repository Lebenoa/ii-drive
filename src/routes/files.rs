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

/// Public upload limits so clients can pre-check files before transferring.
pub async fn limits(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "max_file_size": state.config.max_file_size }))
}

#[derive(serde::Serialize)]
pub struct FileDto {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub created_at: i64,
    pub public: bool,
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
        }
    }
}

#[derive(serde::Deserialize)]
pub struct VisibilityBody {
    pub public: bool,
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
        uid: ulid::Ulid::new().to_string(),
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

    let max = state.config.max_file_size;
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
    let nparts = (declared.div_ceil(part_size).max(1) as usize).min(64);

    // Storage target per part: round-robin across the user's selected
    // channels, or the configured fallback.
    let user_key = state.tg.current_user_id().await.map(|id| id.to_string());
    let selected = match &user_key {
        Some(k) => crate::db::get_channels(&state.db, k).await.unwrap_or_default(),
        None => Vec::new(),
    };
    let chat_for = |i: usize| -> String {
        if selected.is_empty() {
            state.config.storage_chat.trim().to_ascii_lowercase()
        } else {
            selected[i % selected.len()].chat.to_ascii_lowercase()
        }
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
        // "upload aborted" means an uploader died first; its error is the
        // actual cause (e.g. Telegram down) and far more useful to the client.
        if err.1 == "upload aborted" {
            for u in uploaders {
                if let Ok(Err(tg_err)) = u.await {
                    return Err(ApiError::bad_request(tg_err));
                }
            }
        }
        return Err(err);
    }
    drop(txs);

    // Collect one message id per part; on any failure, roll back the parts
    // that did land so no orphan stays behind.
    let mut parts: Vec<crate::db::FilePart> = Vec::with_capacity(nparts);
    let mut first_err: Option<String> = None;
    for (i, u) in uploaders.into_iter().enumerate() {
        let res = u
            .await
            .map_err(|e| format!("upload task failed: {e}"))
            .and_then(|r| r);
        match res {
            Ok((message_id, _, _)) => parts.push(crate::db::FilePart {
                message_id,
                chat: chat_for(i),
                size: if i + 1 == nparts {
                    declared - part_size * (nparts as u64 - 1)
                } else {
                    part_size
                } as i64,
            }),
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
    tracing::info!(%name, parts = nparts, %declared, "uploaded file");
    let row = FileRow {
        uid: ulid::Ulid::new().to_string(),
        name,
        mime,
        size: declared as i64,
        message_id: parts[0].message_id,
        chat: parts[0].chat.clone(),
        created_at: now_unix(),
        folder,
        parts,
        public: false,
    };
    if let Err(e) = crate::db::insert(&state.db, &row).await {
        // No metadata row means the file is unmanageable; drop the messages
        // rather than leaving orphans in the storage chats.
        cleanup_parts(&state, &row.parts).await;
        return Err(e.into());
    }

    Ok(Json(serde_json::json!({ "file": FileDto::from(row) })))
}

/// Best-effort removal of already-posted part messages after a failure.
async fn cleanup_parts(state: &AppState, parts: &[crate::db::FilePart]) {
    for p in parts {
        if let Err(e) = state.tg.delete_message(p.message_id, &p.chat).await {
            tracing::error!(message_id = p.message_id, "orphaned telegram message: {e}");
        }
    }
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
    // the file stays listed and the delete can be retried (the messages
    // deleted so far are gone from Telegram, which is harmless).
    let mut failed: Option<String> = None;
    for p in &row.parts {
        if let Err(e) = state.tg.delete_message(p.message_id, &p.chat).await {
            failed = Some(e);
            break;
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

    // Private files need a valid session token: either the usual
    // Authorization header or ?token= for plain browser links/downloads.
    if !row.public {
        let ok = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| state.tokens.verify(t))
            || q
                .get("token")
                .is_some_and(|t| state.tokens.verify(t));
        if !ok {
            return Err(ApiError(
                axum::http::StatusCode::FORBIDDEN,
                "file is private".into(),
            ));
        }
    }

    let stream = crate::stream::parts_stream(state.tg.clone(), row.parts.clone())
        .await
        .map_err(ApiError::unavailable)?;
    let body = Body::from_stream(stream);
    let disposition = if q.contains_key("dl") || q.get("dl").is_some_and(|v| v == "1") {
        "attachment"
    } else {
        "inline"
    };
    let encoded = percent_encode(&row.name);

    Response::builder()
        .header(header::CONTENT_TYPE, &row.mime)
        .header(header::CONTENT_LENGTH, row.size)
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
