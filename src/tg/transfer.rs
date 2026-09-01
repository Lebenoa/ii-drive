#![allow(clippy::large_futures)] // send/run futures are awaited directly here; boxing adds no value
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mtprsto::client::Client;
use mtprsto::pool::SenderPool;
use mtprsto::rpc::{self, InputMedia};
use mtprsto::types::{self, InputFile, MsgId, Updates};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, mpsc};

use super::{PeerRef, TgManager, friendly, get_messages_by_id};

/// `upload.saveFilePart` part size (512 KiB is the only allowed value).
const PART_SIZE: usize = 512 * 1024;
/// Parts uploaded concurrently over the pool's connections. mtprsto keeps
/// several TCP connections per pool; this many RPCs stay in flight.
const UPLOAD_WORKERS: usize = 4;

impl TgManager {
    /// Streams `reader` up to Telegram and posts it as a document message in
    /// the given storage chat. Returns `(message id, name, mime, thumb)` —
    /// thumb is the tiny JPEG Telegram generates, when it made one.
    #[allow(
        clippy::cast_possible_truncation, // file size u64→usize is lossless on this 64-bit host
        clippy::as_conversions            // ditto
    )]
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
        // chat: each bot is its own session with its own rate budget, and
        // its pool spreads the parts over several TCP connections. Chats
        // bots cannot serve — Saved Messages, an empty pool, every bot
        // failing to resolve — fall back to the owner session.
        let (client, peer, bot, _dc_id, pool) = self.pool_target(chat).await?;
        tracing::info!(
            bot = bot.as_deref().unwrap_or("owner"),
            file = %name,
            chat = %chat,
            size,
            connections = pool.connection_count(),
            "upload start"
        );

        let uploaded = upload_stream(pool, reader, size, name.to_string())
            .await
            .map_err(|e| friendly(format!("upload to telegram failed: {e}")))?;

        let media = InputMedia::UploadedDocument {
            file: uploaded,
            mime_type: mime.to_string(),
            file_name: name.to_string(),
        };
        let payload = rpc::build_send_media(&peer, &media, "", None, false, false, None);
        let raw = client
            .invoke_raw(payload)
            .await
            .map_err(|e| friendly(format!("sending message failed: {e}")))?;
        let updates = Updates::parse(&raw).map_err(|e| format!("send response unreadable: {e}"))?;
        let (msg_id, doc) = updates.message_and_document();

        // A short answer carries the id but no media; fetch the message
        // once so its stripped thumbnail is not lost.
        let doc = match doc {
            Some(doc) => Some(doc),
            None => match msg_id {
                Some(id) => {
                    let msgs = get_messages_by_id(&client, &peer, &[id]).await?;
                    msgs.iter().find_map(types::Message::document)
                }
                None => None,
            },
        };
        let thumb = doc.as_ref().and_then(types::Document::stripped_thumb_jpeg);
        let id = msg_id.ok_or("server returned no message id")?.0 as i32;
        Ok((id, name.to_string(), mime.to_string(), thumb))
    }

    /// Admin diagnostic: pushes the same buffer to the given storage chat
    /// twice — once through the rotating bot pool, once forced through the
    /// owner session — and reports wall-clock timings. Same channel, same
    /// code path, so the only variable is which session carries the bytes.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)] // buffer size u64→usize is lossless on this 64-bit host
    pub async fn bench_upload(
        &self,
        size_mb: u64,
        chat: &str,
    ) -> Result<serde_json::Value, String> {
        // size_mb is an admin diagnostic, so clamp rather than overflow on a
        // huge bogus value.
        let size = size_mb.saturating_mul(1024).saturating_mul(1024);
        let (bot_msg, bot_secs) = {
            let (client, peer, _, _, pool) = self.pool_target(chat).await?;
            run_bench(client, peer, pool, size).await?
        };
        let (own_msg, own_secs) = {
            let client = self.ensure_connected().await?;
            let peer = self.storage_peer(chat).await?;
            let pool = client.pool();
            run_bench(client, peer, pool, size).await?
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
        let client = self.ensure_connected().await?;
        let peer = self.storage_peer(chat).await?;
        let payload = match peer {
            // Channel peers refuse the plain method (CHANNEL_INVALID).
            types::InputPeer::Channel {
                channel_id,
                access_hash,
            } => rpc::build_channels_delete_messages(
                &types::InputChannel::Channel {
                    channel_id,
                    access_hash,
                },
                &[MsgId(i64::from(message_id))],
            ),
            _ => rpc::build_delete_messages(&[MsgId(i64::from(message_id))], false),
        };
        client
            .invoke_raw(payload)
            .await
            .map_err(|e| format!("deleting telegram message failed: {e}"))?;
        Ok(())
    }
}

/// One bench leg: push `size` zero bytes through `pool` and post the
/// resulting document message, returning (message id, elapsed seconds).
#[allow(
    clippy::as_conversions,           // bench buffer size u64→usize: diagnostic-only path
    clippy::cast_possible_truncation, // ditto; message id is int32 on the wire
)]
async fn run_bench(
    client: Arc<Client>,
    peer: PeerRef,
    pool: Arc<SenderPool>,
    size: u64,
) -> Result<(i32, f64), String> {
    let data = vec![0xAAu8; size as usize];
    let mut reader: &[u8] = &data;
    let start = std::time::Instant::now();
    let uploaded = upload_stream(pool, &mut reader, size, "bench.bin".to_string())
        .await
        .map_err(|e| friendly(format!("bench upload failed: {e}")))?;
    let media = InputMedia::UploadedDocument {
        file: uploaded,
        mime_type: "application/octet-stream".to_string(),
        file_name: "bench.bin".to_string(),
    };
    let payload = rpc::build_send_media(&peer, &media, "", None, false, false, None);
    let raw = client
        .invoke_raw(payload)
        .await
        .map_err(|e| friendly(format!("bench send failed: {e}")))?;
    let updates =
        Updates::parse(&raw).map_err(|e| format!("bench send response unreadable: {e}"))?;
    let (msg_id, _) = updates.message_and_document();
    let id = msg_id.ok_or("bench send returned no message id")?.0 as i32;
    Ok((id, start.elapsed().as_secs_f64()))
}

/// Streams `reader` to Telegram in 512 KiB parts, keeping
/// [`UPLOAD_WORKERS`] `upload.save{,Big}File` RPCs in flight while the
/// reader is drained sequentially. mtprsto's own `file::upload` buffers
/// the whole file in memory behind a blocking `std::io::Read`; this
/// variant stays async and bounded so web uploads can stream through.
// One producer + N workers pipeline; splitting it scatters the
// short-read, failure and ordering invariants across functions.
#[allow(
    clippy::cast_possible_truncation, // part counts bounded by the 4000-part Telegram cap
    clippy::cast_possible_wrap,       // part/total_parts < 4000, far below i32 range
    clippy::arithmetic_side_effects,  // filled < PART_SIZE and part < total_parts bound every sum
    clippy::indexing_slicing,         // filled <= buf.len() is the read loop's exit condition
    clippy::as_conversions,           // PART_SIZE is a fixed 512 KiB const; parts < 4000
    clippy::too_many_lines
)]
async fn upload_stream<R>(
    pool: Arc<SenderPool>,
    reader: &mut R,
    size: u64,
    name: String,
) -> Result<InputFile, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if size == 0 {
        return Err("cannot upload an empty file".into());
    }
    let big = size > mtprsto::file::BIG_FILE_THRESHOLD;
    let total_parts = size.div_ceil(PART_SIZE as u64) as usize;
    if big && total_parts > 4000 {
        return Err(format!(
            "file too large: {size} bytes needs {total_parts} parts (max 4000)"
        ));
    }
    let file_id: i64 = rand::random();
    let workers = UPLOAD_WORKERS.min(total_parts).max(1);

    let (tx, rx) = mpsc::channel::<(usize, Vec<u8>)>(8);
    let rx = Arc::new(Mutex::new(rx));
    let failed = Arc::new(AtomicUsize::new(0));

    // Producer: read the parts in order (the source is a stream; order is
    // defined by the bytes) and hand them to the workers. Bounded channel
    // keeps at most a few parts in memory at a time.
    let producer = async {
        let mut buf = vec![0u8; PART_SIZE];
        for part in 0..total_parts {
            if failed.load(Ordering::Relaxed) > 0 {
                break; // a worker already failed; stop reading
            }
            let mut filled = 0usize;
            while filled < PART_SIZE {
                let n = reader
                    .read(&mut buf[filled..])
                    .await
                    .map_err(|e| format!("read failed: {e}"))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 || (part + 1 < total_parts && filled < PART_SIZE) {
                return Err(format!(
                    "upload stream ended early: part {part} of {total_parts} is short"
                ));
            }
            if tx.send((part, buf[..filled].to_vec())).await.is_err() {
                break; // all workers gone
            }
        }
        Ok::<(), String>(())
    };

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let rx = Arc::clone(&rx);
        let pool = Arc::clone(&pool);
        let failed = Arc::clone(&failed);
        handles.push(tokio::spawn(async move {
            loop {
                // mpsc receivers are single-consumer; the workers share
                // theirs behind a mutex and hold it only across one recv.
                let next = rx.lock().await.recv().await;
                let Some((part, data)) = next else {
                    return Ok::<(), String>(());
                };
                let payload = if big {
                    rpc::build_save_big_file_part(file_id, part as i32, total_parts as i32, &data)
                } else {
                    rpc::build_save_file_part(file_id, part as i32, &data)
                };
                if let Err(e) = pool.send_rpc(&payload).await {
                    failed.fetch_add(1, Ordering::Relaxed);
                    return Err(format!("part {part} upload failed: {e}"));
                }
            }
        }));
    }

    let producer_result = producer.await;
    drop(tx); // workers end when the queue drains
    let mut first_err = producer_result.err();
    for handle in handles {
        let joined = handle
            .await
            .map_err(|e| format!("upload worker panicked: {e}"))
            .and_then(|r| r);
        if let Err(e) = joined {
            first_err.get_or_insert(e);
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    Ok(if big {
        InputFile::Big {
            id: file_id,
            parts: total_parts as i32,
            name,
        }
    } else {
        // md5 is optional on the wire; mtprsto's own uploader sends none.
        InputFile::Id {
            id: file_id,
            parts: total_parts as i32,
            name,
            md5_checksum: String::new(),
        }
    })
}
