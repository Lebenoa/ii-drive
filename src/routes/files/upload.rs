use std::io;

use axum::extract::multipart::Multipart;
use axum::extract::State;
use axum::Json;
use tokio_stream::wrappers::ReceiverStream;

use crate::db::FileRow;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{
    bytes_repr, cleanup_parts, extract_media_thumb, now_unix, FileDto, DRAIN_CAP, HEAD_CAP,
};

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
            "file exceeds limit of {} bytes",
            bytes_repr(max)
        )));
    }

    // Telegram stores each document separately and caps its size (2 GiB on
    // a free account) regardless of the configured drive limit — so a
    // `max_file_size` above that is honored by transparently chunking the
    // file into cap-sized documents and re-joining them on download.
    const TG_DOC_CAP: u64 = 2 * 1024 * 1024 * 1024;
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
    // Over-cap chunked files keep ALL parts in one channel — a single
    // file re-joined from parts of one chat is cleaner to fetch and audit.
    // Which channel is chosen rotates per upload so parallel large files
    // spread across the selection; ordinary split uploads keep the
    // per-part round-robin.
    let base = state.tg.next_rotation();
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
