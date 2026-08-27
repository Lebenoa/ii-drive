#![allow(clippy::large_futures)] // send_message/run futures are awaited directly here; boxing adds no value
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use futures::stream::{FuturesUnordered, StreamExt as _};
use grammers_client::InvocationError;
use grammers_client::message::InputMessage;

use super::{
    FILE_REFERENCE_EXPIRED, FILEREF_UPGRADE_NEEDED, TgManager, friendly, stripped_thumb_jpeg,
};

// ---------------------------------------------------------------------------
// Parallel upload stream — replaces grammers' built-in `upload_stream` for
// big files (>10 MiB) to get 4× more MTProto pipelining (16 workers vs 4).
// ---------------------------------------------------------------------------

/// Number of concurrent upload workers. grammers defaults to 4; 16 fills
/// the TCP window much better and matches what the official Telegram client
/// does at the MTProto level.
const UPLOAD_WORKERS: usize = 16;

/// Telegram upload part size: 512 KiB, same as grammers' MAX_CHUNK_SIZE.
const CHUNK_SIZE: usize = 512 * 1024;

/// Files at or below this size use grammers' sequential upload path (only a
/// handful of chunks, no benefit from parallelism).
const BIG_FILE_THRESHOLD: usize = 10 * 1024 * 1024; // 10 MiB

/// Parallel stream upload to Telegram.
///
/// For files ≤ 10 MiB the built-in sequential path is used (few chunks,
/// negligible overhead).  For larger files `UPLOAD_WORKERS` concurrent tasks
/// each read 512 KiB chunks from a bounded channel and call
/// `SaveBigFilePart` in parallel, pipelining over the MTProto connection.
///
/// Returns a [`grammers_client::media::Uploaded`] ready to pass to
/// [`InputMessage::document`].
async fn parallel_upload_stream<S>(
    client: &grammers_client::Client,
    reader: &mut S,
    size: usize,
    name: String,
    dc_id: i32,
    pools: &[grammers_client::sender::SenderPoolHandle],
) -> Result<grammers_client::media::Uploaded, std::io::Error>
where
    S: tokio::io::AsyncRead + Unpin,
{
    if size <= BIG_FILE_THRESHOLD {
        return client.upload_stream(reader, size, name).await;
    }

    use grammers_client::media::Uploaded;
    use grammers_client::tl;
    use tl::{Deserializable as _, Serializable as _};

    let file_id: i64 = rand::random();
    let total_parts = size.div_ceil(CHUNK_SIZE) as i32;
    // Telegram FILE_MIGRATE (303) signals the upload DC changed.
    const FILE_MIGRATE: i32 = 303;
    // Shared DC id so all workers switch together when a migration
    // fires — parts on a single file_id must land on one DC.
    let current_dc = Arc::new(AtomicI32::new(dc_id));
    // Cap FILE_MIGRATE retries to avoid infinite loops on a
    // misbehaving server.
    const MAX_MIGRATE_RETRIES: u32 = 3;

    // Bounded channel: capacity = workers + 1 lets every worker hold a
    // chunk while one more is being read from disk, capping peak memory at
    // ~16 × 512 KiB = 8 MiB.
    let (tx, rx) = tokio::sync::mpsc::channel::<(i32, Vec<u8>)>(UPLOAD_WORKERS + 1);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    // Spawn workers first — they sit idle until chunks arrive.  Each worker
    // is pinned to one pool handle (round-robin) so its RPCs go through
    // its own TCP connection to the upload DC.
    let mut workers = FuturesUnordered::new();
    for idx in 0..UPLOAD_WORKERS {
        let pool = pools[idx % pools.len()].clone();
        let rx = Arc::clone(&rx);
        let current_dc = Arc::clone(&current_dc);
        workers.push(tokio::spawn(async move {
            loop {
                let Some((part, bytes)) = rx.lock().await.recv().await else {
                    break;
                };
                let body = tl::functions::upload::SaveBigFilePart {
                    file_id,
                    file_part: part,
                    file_total_parts: total_parts,
                    bytes,
                }
                .to_bytes();
                // Handle FILE_MIGRATE: all workers share the DC via
                // AtomicI32 so parts don't split across DCs.
                let mut retries = 0u32;
                let resp = loop {
                    let dc = current_dc.load(Ordering::Relaxed);
                    match pool.invoke_in_dc(dc, body.clone()).await {
                        Ok(r) => break r,
                        Err(grammers_client::InvocationError::Rpc(err))
                            if err.code == FILE_MIGRATE =>
                        {
                            retries += 1;
                            if retries > MAX_MIGRATE_RETRIES {
                                return Err(std::io::Error::other(format!(
                                    "upload part {part}: too many FILE_MIGRATE redirects"
                                )));
                            }
                            let new_dc =
                                err.value.unwrap_or(dc as u32) as i32;
                            let prev = current_dc.swap(new_dc, Ordering::Relaxed);
                            if prev != new_dc {
                                tracing::info!(
                                    part,
                                    old_dc = prev,
                                    new_dc,
                                    "FILE_MIGRATE: switching upload DC"
                                );
                            }
                        }
                        Err(grammers_client::InvocationError::Rpc(err))
                            if err.name == "AUTH_KEY_UNREGISTERED" =>
                        {
                            // The target DC has no auth key.  grammers'
                            // copy_auth_to_dc is pub(crate), so we cannot
                            // copy credentials here.  Return a clear error
                            // instead of looping or crashing opaquely.
                            return Err(std::io::Error::other(format!(
                                "upload part {part}: target DC {dc} has no \
                                 auth key (AUTH_KEY_UNREGISTERED). \
                                 The file may need migration to a DC \
                                 this session has not authenticated with."
                            )));
                        }
                        Err(e) => {
                            return Err(std::io::Error::other(format!(
                                "upload part {part} failed: {e}"
                            )));
                        }
                    }
                };
                let ok = bool::from_bytes(&resp).map_err(|e| {
                    std::io::Error::other(format!(
                        "upload part {part}: bad response: {e}"
                    ))
                })?;
                if !ok {
                    return Err(std::io::Error::other(format!(
                        "upload part {part}: server rejected the data"
                    )));
                }
            }
            Ok::<(), std::io::Error>(())
        }));
    }

    // Producer (runs inline): reads the input stream into 512 KiB chunks
    // and pushes them into the channel.  The `tx.send().await` yields to
    // the runtime so the spawned workers make progress while we read.
    {
        let mut part = 0i32;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut err: Option<std::io::Error> = None;
        'outer: loop {
            let mut filled = 0;
            while filled < CHUNK_SIZE {
                match tokio::io::AsyncReadExt::read(reader, &mut buf[filled..]).await {
                    Ok(0) => {
                        if part == total_parts - 1 {
                            break; // last part, may be short
                        }
                        err = Some(std::io::Error::other(
                            "stream ended before last part",
                        ));
                        break 'outer;
                    }
                    Ok(n) => filled += n,
                    Err(e) => {
                        err = Some(e);
                        break 'outer;
                    }
                }
            }
            buf.truncate(filled);
            if tx.send((part, buf)).await.is_err() {
                break; // all workers gone — error reported below
            }
            part += 1;
            if part >= total_parts {
                break;
            }
            buf = vec![0u8; CHUNK_SIZE];
        }
        drop(tx); // close the channel so workers see EOF
        if let Some(e) = err {
            return Err(e);
        }
    }

    // Wait for every worker to finish uploading its chunks.
    while let Some(result) = workers.next().await {
        result
            .map_err(|e| {
                std::io::Error::other(
                    format!("upload worker panicked: {e}"),
                )
            })??;
    }

    Ok(Uploaded::from_raw(
        tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name,
        }
        .into(),
    ))
}

impl TgManager {
    /// Pool handles (main + auxiliary) and the home DC id for the owner
    /// connection.  Upload workers round-robin across these handles to
    /// spread bytes over multiple TCP connections to the same DC.
    pub(super) async fn upload_pools(&self) -> Result<(i32, Vec<grammers_client::sender::SenderPoolHandle>), String> {
        let st = self.st.lock().await;
        let conn = st.conn.as_ref().ok_or_else(|| "not connected to Telegram".to_string())?;
        let dc_id = conn.dc_id;
        let pools: Vec<grammers_client::sender::SenderPoolHandle> =
            conn.all_pools().cloned().collect();
        Ok((dc_id, pools))
    }

    /// Streams `reader` up to Telegram and posts it as a document message in
    /// the given storage chat. Returns `(message id, name, mime, thumb)` —
    /// thumb is the tiny JPEG Telegram generates, when it made one.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)] // file size u64→usize is lossless on this 64-bit host
    #[allow(clippy::large_futures)] // the send_message future is acknowledged directly here; boxing adds no value
    pub async fn upload<S>(
        &self,
        reader: &mut S,
        size: u64,
        name: &str,
        mime: &str,
        chat: &str,
    ) -> Result<(i32, String, String, Option<Vec<u8>>), String>
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        // Parts go out through rotating bot sessions when any can serve this
        // chat: each bot is its own session, hence its own MTProto
        // connection with its own rate budget, so concurrent parts truly
        // run in parallel instead of pipelining over the single owner
        // connection. Bots may post into storage chats — wiring promotes
        // them with `post_messages` and makes them index the channel, so
        // id-based resolution from their side works (see
        // `add_bots_to_chat`). Chats bots cannot serve — Saved Messages,
        // an empty pool, every bot failing to resolve — fall back to the
        // owner session.
        let (client, peer, bot, dc_id, pools) = self.pool_target(chat).await?;
        tracing::info!(
            bot = bot.as_deref().unwrap_or("owner"),
            file = %name,
            chat = %chat,
            size,
            connections = pools.len(),
            "upload start"
        );

        // pool_target returns pools bound to the same session as the
        // client (bot or owner), so the file_id created by
        // parallel_upload_stream is always valid for send_message.
        let uploaded = parallel_upload_stream(&client, reader, size as usize, name.to_string(), dc_id, &pools)
            .await
            .map_err(|e| friendly(format!("upload to telegram failed: {e}")))?;

        let msg = client
            .send_message(
                peer,
                InputMessage::new()
                    .text("")
                    .document(uploaded)
                    .mime_type(mime),
            )
            .await
            .map_err(|e| friendly(format!("sending message failed: {e}")))?;

        let thumb = msg.media().and_then(|m| match m {
            grammers_client::media::Media::Document(doc) => {
                doc.thumbs().into_iter().find_map(|t| match t {
                    grammers_client::media::PhotoSize::Stripped(s) => {
                        let jpeg = stripped_thumb_jpeg(&s.bytes);
                        (!jpeg.is_empty()).then_some(jpeg)
                    }
                    _ => None,
                })
            }
            _ => None,
        });
        Ok((msg.id(), name.to_string(), mime.to_string(), thumb))
    }

    /// Admin diagnostic: pushes the same buffer to the given storage chat
    /// twice — once through the rotating bot pool, once forced through the
    /// owner session — and reports wall-clock timings. Same channel, same
    /// code path, so the only variable is which session carries the bytes.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)] // buffer size u64→usize is lossless on this 64-bit host
    #[allow(clippy::large_futures)] // the send_message and run futures are awaited directly; boxing adds no value
    pub async fn bench_upload(
        &self,
        size_mb: u64,
        chat: &str,
    ) -> Result<serde_json::Value, String> {
        // size_mb is an admin diagnostic, so clamp rather than overflow on a
        // huge bogus value.
        let size = size_mb.saturating_mul(1024).saturating_mul(1024);

        // Both bot and owner use the parallel uploader with their own
        // session's pools — pool_target returns pools bound to the same
        // auth key as the client, so file_id references are consistent.
        let run = |client: grammers_client::Client,
                   peer: super::PeerRef,
                   dc_id: i32,
                   pools: Vec<grammers_client::sender::SenderPoolHandle>| {
            let data = vec![0xAAu8; size as usize];
            let mut reader = std::io::Cursor::new(data);
            let start = std::time::Instant::now();
            async move {
                let uploaded = parallel_upload_stream(&client, &mut reader, size as usize, "bench.bin".to_string(), dc_id, &pools)
                    .await
                    .map_err(|e| friendly(format!("bench upload failed: {e}")))?;
                let msg = client
                    .send_message(peer, InputMessage::new().text("").document(uploaded))
                    .await
                    .map_err(|e| friendly(format!("bench send failed: {e}")))?;
                let secs = start.elapsed().as_secs_f64();
                Ok::<_, String>((msg.id(), secs))
            }
        };

        let (bot_msg, bot_secs) = {
            let (client, peer, _, dc_id, pools) = self.pool_target(chat).await?;
            run(client, peer, dc_id, pools).await?
        };
        let (own_msg, own_secs) = {
            let client = self.ensure().await?;
            let peer = self.storage_peer(chat).await?;
            let (dc_id, pools) = self.upload_pools().await?;
            run(client, peer, dc_id, pools).await?
        };

        self.delete_message(bot_msg, chat).await.ok();
        self.delete_message(own_msg, chat).await.ok();

        Ok(serde_json::json!({
            "size_mb": size_mb,
            "bot_secs": bot_secs,
            "owner_secs": own_secs,
        }))
    }

    pub async fn delete_message(&self, message_id: i32, chat: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        let peer = self.storage_peer(chat).await?;
        client
            .delete_messages(peer, &[message_id])
            .await
            .map_err(|e| format!("deleting telegram message failed: {e}"))?;
        Ok(())
    }
}

pub fn is_file_reference_error(err: &InvocationError) -> bool {
    if let InvocationError::Rpc(rpc) = err {
        rpc.is(FILE_REFERENCE_EXPIRED) || rpc.is(FILEREF_UPGRADE_NEEDED)
    } else {
        false
    }
}
