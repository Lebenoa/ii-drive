use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{Stream, unfold};
use mtprsto::client::Client;
use mtprsto::pool::SenderPool;
use mtprsto::rpc;
use mtprsto::types::{self, InputPeer, MsgId};

use crate::tg::{TgManager, get_messages_by_id};

/// `upload.getFile` alignment: Telegram requires offsets divisible by
/// 4 KiB. A range start in the middle of a block aligns down, and the
/// prefix bytes are dropped on the wire instead of being served.
const ALIGN: u64 = 4096;
/// Per-request chunk. 1 MiB is the server's cap per `upload.getFile`.
const CHUNK: usize = 1024 * 1024;
/// Parallel `getFile` workers per part download — the pool spreads the
/// requests over its bot connections, multiplying throughput on
/// high-RTT links (the gotd-downloader model).
const WORKERS: usize = 4;

/// A shared, refreshable `FileLocation`: workers clone the current
/// location; when a file reference expires, one worker refetches the
/// message and bumps the generation so the rest reuse its result
/// instead of stampeding `get_messages`.
#[derive(Default)]
struct LocSlot {
    generation: u64,
    loc: Option<types::FileLocation>,
}

type SharedLocSlot = Arc<std::sync::Mutex<LocSlot>>;

/// Pulls the download location and document size out of a part message.
async fn fetch_location(
    client: &Client,
    peer: &InputPeer,
    msg_id: i32,
) -> Result<(types::FileLocation, u64), String> {
    let msgs = get_messages_by_id(client, peer, &[MsgId(i64::from(msg_id))]).await?;
    let Some(msg) = msgs.into_iter().next() else {
        return Err("message no longer exists".to_string());
    };
    let Some(doc) = msg.document() else {
        return Err("message has no document media".to_string());
    };
    let size = match &doc {
        mtprsto::types::Document::Document { size, .. } => u64::try_from(*size).unwrap_or(0),
        mtprsto::types::Document::Empty { .. } => 0,
    };
    let loc = doc
        .location()
        .ok_or_else(|| "document is a placeholder with no media".to_string())?;
    Ok((loc, size))
}

/// Fetches one aligned window, refetching the location through `slot`
/// when the file reference expires. `limit` must already be aligned.
///
/// The refresh protocol: only the worker that observed the current
/// generation refetches the message; the rest loop around and pick up
/// the refreshed slot. The mutex is never held across an await.
async fn fetch_window(
    pool: &SenderPool,
    slot: &SharedLocSlot,
    client: &Client,
    peer: &InputPeer,
    msg_id: i32,
    offset: u64,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    loop {
        let (my_generation, loc) = {
            let guard = slot.lock().expect("loc slot poisoned");
            (guard.generation, guard.loc.clone())
        };
        let loc = match loc {
            Some(loc) => loc,
            None => {
                let (loc, _) = fetch_location(client, peer, msg_id)
                    .await
                    .map_err(std::io::Error::other)?;
                let mut guard = slot.lock().expect("loc slot poisoned");
                guard.generation += 1;
                guard.loc = Some(loc.clone());
                loc
            }
        };
        let payload = rpc::build_get_file(&loc, offset.cast_signed(), limit as i32);
        let raw = match pool.send_rpc(&payload).await {
            Ok(raw) => raw,
            Err(e) if e.is_file_reference() => {
                mark_stale(slot, my_generation, client, peer, msg_id).await;
                continue;
            }
            Err(e) => return Err(std::io::Error::other(format!("getFile: {e}"))),
        };
        return match mtprsto::file::parse_get_file(&raw) {
            Ok(mtprsto::file::GetFile::File { bytes, .. }) => Ok(bytes),
            Ok(_) => Err(std::io::Error::other(
                "file is served from a CDN, which is not supported",
            )),
            Err(e) if e.is_file_reference() => {
                mark_stale(slot, my_generation, client, peer, msg_id).await;
                continue;
            }
            Err(e) => Err(std::io::Error::other(format!("getFile parse: {e}"))),
        };
    }
}

/// Refetches the download location if `my_generation` is still current —
/// a stale-generation caller just adopts whoever refreshed it.
async fn mark_stale(
    slot: &SharedLocSlot,
    my_generation: u64,
    client: &Client,
    peer: &InputPeer,
    msg_id: i32,
) {
    {
        let guard = slot.lock().expect("loc slot poisoned");
        if guard.generation != my_generation {
            return; // someone already refreshed it
        }
    }
    match fetch_location(client, peer, msg_id).await {
        Ok((loc, _)) => {
            let mut guard = slot.lock().expect("loc slot poisoned");
            guard.generation += 1;
            guard.loc = Some(loc);
        }
        Err(e) => tracing::warn!("location refetch failed (msg {msg_id}): {e}"),
    }
}

/// Fills one grid chunk of `expected` bytes at `offset`. The window
/// never overshoots the document end: `limit` is the exact remaining
/// byte count (the server rejects any window extending past the
/// content, aligned or not). When a DC still rejects a valid-looking
/// window with `LIMIT_INVALID` (observed on legacy 2-GiB parts,
/// deterministically per offset), the fetch degrades to 4096-byte
/// steps for this chunk rather than failing the whole file.
async fn fetch_chunk(
    pool: &SenderPool,
    slot: &SharedLocSlot,
    client: &Client,
    peer: &InputPeer,
    msg_id: i32,
    offset: u64,
    expected: usize,
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(expected);
    let mut at = offset;
    let mut min_limit = false;
    let mut ladder_tries = 0u32;
    while buf.len() < expected {
        let want = expected - buf.len();
        let limit = if min_limit {
            (ALIGN as usize).min(want)
        } else {
            want
        };
        match fetch_window(pool, slot, client, peer, msg_id, at, limit).await {
            Ok(bytes) if bytes.is_empty() => {
                // Past the content end: the declared document size lies.
                // Zero-fill the remainder so the byte positions the HTTP
                // range promised still line up.
                tracing::warn!(
                    "getFile empty at {at} (msg {msg_id}); padding {} bytes",
                    expected - buf.len()
                );
                buf.resize(expected, 0);
                break;
            }
            Ok(bytes) => {
                let take = bytes.len().min(want);
                buf.extend_from_slice(&bytes[..take]);
                at += take as u64;
                // The window came back short of what was asked: EOF.
                if bytes.len() < limit && buf.len() < expected {
                    buf.resize(expected, 0);
                    break;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if !min_limit && msg.contains("LIMIT_INVALID") {
                    // Degrade to aligned 4-KiB steps for this chunk.
                    min_limit = true;
                    ladder_tries += 1;
                    if ladder_tries > 64 {
                        return Err(std::io::Error::other(format!(
                            "document window at {at} keeps failing; \
                             part content is likely truncated"
                        )));
                    }
                    continue;
                }
                return Err(e);
            }
        }
    }
    Ok(buf)
}

/// A byte stream backed by `upload.getFile` that transparently refetches
/// the message when the stored `file_reference` expires mid-stream, and
/// fetches chunks through a bounded worker pool (gotd-downloader style):
/// each worker grabs the next grid offset, the merger yields results in
/// order, and HTTP Range prefixes are dropped from the first chunk.
// The spawn/merge plumbing lives in one function: the worker contract
// (index → ordered bytes) is only meaningful next to the merger.
#[allow(
    clippy::arithmetic_side_effects, // byte offsets bounded by the file size and cap
    clippy::as_conversions,          // u64 offset to i64: file sizes < i64::MAX
    clippy::too_many_lines
)]
pub async fn file_stream_from(
    tg: &TgManager,
    message_id: i32,
    chat: &str,
    start: u64,
) -> Result<(impl Stream<Item = std::io::Result<Bytes>> + use<>, u64), String> {
    let (client, peer, _bot, _dc_id, pool) = tg.pool_target(chat).await?;

    // Grid: chunks of CHUNK bytes counted from the DOCUMENT start, so
    // every window offset is 1 MiB-aligned — the server rejects
    // full-size windows at merely-4-KiB-aligned offsets with
    // LIMIT_INVALID. `base` is the grid cell holding `start`; the
    // prefix before `start` is dropped from the first served chunk.
    let first_idx = start / CHUNK as u64;
    let base = first_idx * CHUNK as u64;

    let slot: SharedLocSlot = Arc::default();
    let (loc, doc_size) = fetch_location(&client, &peer, message_id).await?;
    {
        let mut guard = slot.lock().expect("loc slot poisoned");
        guard.loc = Some(loc);
    }
    // The part's actual content may fall short of the size the upload
    // declared (a truncated upload). Everything past the real content
    // is gone — the caller must surface that instead of desyncing the
    // byte stream with made-up offsets.
    let capacity = doc_size.saturating_sub(start);
    if doc_size == 0 || capacity == 0 {
        return Err(format!(
            "part content truncated: document holds {doc_size} bytes, \
             range starts at {start}"
        ));
    }
    let total_chunks = doc_size.div_ceil(CHUNK as u64) - first_idx;
    let first_discard = start - base;

    let (tx, rx) = tokio::sync::mpsc::channel::<(u64, std::io::Result<Vec<u8>>)>(WORKERS * 2);
    let next = Arc::new(std::sync::atomic::AtomicU64::new(0));
    for _ in 0..WORKERS.min(total_chunks as usize) {
        let tx = tx.clone();
        let next = next.clone();
        let slot = slot.clone();
        let client = client.clone();
        let peer = peer.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            loop {
                let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if idx >= total_chunks {
                    return;
                }
                let offset = base + idx * CHUNK as u64;
                let expected = (CHUNK as u64).min(doc_size - offset) as usize;
                let res =
                    fetch_chunk(&pool, &slot, &client, &peer, message_id, offset, expected).await;
                if tx.send((idx, res)).await.is_err() {
                    return; // reader went away
                }
            }
        });
    }
    drop(tx);

    let pending: std::collections::BTreeMap<u64, Vec<u8>> = Default::default();
    let stream = unfold(
        (rx, pending, 0u64, first_discard, total_chunks),
        |mut st| async move {
            let (rx, pending, next_idx, discard, total_chunks) = &mut st;
            loop {
                let bytes = match pending.remove(next_idx) {
                    Some(bytes) => bytes,
                    None => match rx.recv().await {
                        Some((idx, Ok(bytes))) => {
                            if &idx != next_idx {
                                pending.insert(idx, bytes);
                                continue;
                            }
                            bytes
                        }
                        Some((_, Err(e))) => {
                            return Some((Err(std::io::Error::other(e)), st));
                        }
                        None => {
                            // Workers drained: only legitimate at EOF.
                            return if next_idx >= total_chunks {
                                None
                            } else {
                                Some((Err(std::io::Error::other("download worker died")), st))
                            };
                        }
                    },
                };
                let mut chunk = Bytes::from(bytes);
                if *discard > 0 {
                    let d = usize::try_from(*discard)
                        .unwrap_or(chunk.len())
                        .min(chunk.len());
                    chunk = chunk.slice(d..);
                    *discard -= d as u64;
                    if chunk.is_empty() {
                        *next_idx += 1;
                        continue;
                    }
                }
                *next_idx += 1;
                return Some((Ok(chunk), st));
            }
        },
    );
    Ok((stream, capacity))
}

/// Caps a byte stream at `limit` bytes: Range responses must send exactly
/// the declared Content-Length or browsers abort the transfer.
#[allow(
    clippy::arithmetic_side_effects, // left/take math bounded by the u64 limit
    clippy::as_conversions,          // usize/u64 bridging of byte counts
    clippy::cast_possible_truncation, // left fits usize (capped by limit, chunk sizes small)
)]
pub fn cap<S>(s: S, limit: u64) -> impl Stream<Item = std::io::Result<Bytes>> + use<S>
where
    S: Stream<Item = std::io::Result<Bytes>>,
{
    unfold((Box::pin(s), limit, true), |(s, mut left, alive)| {
        let mut s = s;
        async move {
            if !alive || left == 0 {
                return None; // end the stream, not just this item
            }
            match s.next().await {
                Some(Ok(b)) => {
                    let take = left.min(b.len() as u64) as usize;
                    left -= take as u64;
                    let out = if take == b.len() { b } else { b.slice(..take) };
                    Some((Ok(out), (s, left, left > 0)))
                }
                Some(Err(e)) => Some((Err(e), (s, left, false))),
                None => None,
            }
        }
    })
}

type BoxedPart = std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Serves a multi-part file as one contiguous byte stream, optionally from
/// byte `start` across part boundaries (HTTP Range support). Parts are
/// fetched in order, each through `file_stream` (which itself resumes on
/// expired file references), so the client sees a seamless download.
#[allow(
    clippy::arithmetic_side_effects, // skip/idx math bounded by sum of part sizes
    clippy::indexing_slicing, // parts[idx] has an explicit idx < parts.len() guard
)]
pub async fn parts_stream_from(
    tg: Arc<TgManager>,
    parts: Vec<crate::db::FilePart>,
    start: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    // Find the part holding `start` and the offset within it. Offsets are
    // plaintext sizes, which is what the DB stores per part regardless of
    // whether the part is encrypted.
    let mut skip = start;
    let mut idx = 0usize;
    while idx < parts.len() && skip >= parts[idx].size.cast_unsigned() {
        skip -= parts[idx].size.cast_unsigned();
        idx += 1;
    }
    if idx >= parts.len() {
        return Err("range start is beyond the file".into());
    }
    let key: Option<Arc<crate::crypt::Key>> =
        crate::config::get().crypt_key_unconditional().map(Arc::new);
    let cur: BoxedPart = part_stream(&tg, &parts, idx, skip, key.as_deref()).await?;
    let st = PartsState {
        tg,
        parts,
        idx,
        cur: Some(cur),
        key: key.clone(),
    };
    Ok(unfold(st, |mut st| async move {
        loop {
            let Some(item) = st.cur.as_mut()?.next().await else {
                st.idx += 1;
                if st.idx >= st.parts.len() {
                    return None;
                }
                match part_stream(&st.tg, &st.parts, st.idx, 0, st.key.as_deref()).await {
                    Ok(s) => st.cur = Some(s),
                    Err(e) => return Some((Err(std::io::Error::other(e)), st)),
                }
                continue;
            };
            return Some((item, st));
        }
    }))
}

struct PartsState {
    tg: Arc<TgManager>,
    parts: Vec<crate::db::FilePart>,
    idx: usize,
    cur: Option<BoxedPart>,
    key: Option<Arc<crate::crypt::Key>>,
}

/// Builds the byte stream for one part, decrypting it when the part carries
/// a crypto nonce (an encrypted upload) and a key is configured. `skip` is
/// the plaintext offset within THIS part:
/// - encrypted part at `skip > 0` (range start): resume at the containing
///   ciphertext block via `at_block`, discarding the intra-block remainder;
/// - encrypted part at `skip == 0`: read the whole container header.
///
#[allow(
    clippy::arithmetic_side_effects, // at_block offset math bounded by part size
    clippy::as_conversions,          // u64 block/size bridging for Telegram offsets
    clippy::indexing_slicing,        // parts[idx] from a caller-verified index
)]
async fn part_stream(
    tg: &TgManager,
    parts: &[crate::db::FilePart],
    idx: usize,
    skip: u64,
    key: Option<&crate::crypt::Key>,
) -> Result<BoxedPart, String> {
    let p = &parts[idx];
    // A truncated part (the Telegram document holds fewer bytes than
    // the upload declared) cannot serve its full share: detect it up
    // front and refuse with a diagnosis instead of desyncing output.
    let declared = p.size.cast_unsigned().saturating_sub(skip);
    let Some(nonce) = p.nonce.as_deref().and_then(crate::crypt::nonce_from_b64) else {
        // Plaintext part: serve the stored bytes as-is.
        let (stream, capacity) = file_stream_from(tg, p.message_id, &p.chat, skip).await?;
        if capacity < declared {
            return Err(format!(
                "part {} is truncated: {capacity} of {declared} bytes \
                 remain — the file needs re-uploading",
                p.message_id
            ));
        }
        return Ok(Box::pin(stream));
    };
    // Encrypted part — the stored bytes are a container. Decryption needs a
    // key; without one the operator has disabled or removed the key while
    // leaving encrypted files behind, which must not silently serve garbage.
    let Some(key) = key else {
        return Err("file is encrypted but no crypt_password is configured".into());
    };
    if skip == 0 {
        let (inner, _) = file_stream_from(tg, p.message_id, &p.chat, 0).await?;
        let dec = crate::crypt::DecryptingStream::from_header(Box::pin(inner), key);
        Ok(Box::pin(dec))
    } else {
        use crate::crypt::{BLOCK_DATA, BLOCK_SIZE, HEADER_SIZE};
        let block_data = BLOCK_DATA as u64;
        let block_size = BLOCK_SIZE as u64;
        let blocks = skip / block_data;
        let intra = skip % block_data;
        // Skip whole 64 KiB-blocks of ciphertext in Telegram's stream, then
        // discard the intra-block plaintext remainder inside the decryptor.
        let ct_off = HEADER_SIZE + blocks * block_size;
        let (inner, _) = file_stream_from(tg, p.message_id, &p.chat, ct_off).await?;
        let dec =
            crate::crypt::DecryptingStream::at_block(Box::pin(inner), key, nonce, blocks, intra);
        Ok(Box::pin(dec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn chunk(n: u8) -> std::io::Result<Bytes> {
        Ok(Bytes::from(vec![n; 4]))
    }

    #[tokio::test]
    async fn cap_bounds_stream_to_limit() {
        let src = futures::stream::iter(vec![chunk(1), chunk(2), chunk(3)]);
        let out: Vec<_> = cap(src, 10).map(|r| r.unwrap().to_vec()).collect().await;
        // 10 of 12 bytes: first two chunks whole, third sliced to 2.
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].len(), 2);
        assert_eq!(out.iter().map(|v| v.len()).sum::<usize>(), 10);
    }

    #[tokio::test]
    async fn cap_at_zero_ends_immediately() {
        let src = futures::stream::iter(vec![chunk(1)]);
        let out: Vec<_> = cap(src, 0).map(|r| r.unwrap().to_vec()).collect().await;
        assert!(out.is_empty());
    }
}
