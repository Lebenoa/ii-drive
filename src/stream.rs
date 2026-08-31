use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{Stream, unfold};
use mtprsto::client::Client;
use mtprsto::pool::SenderPool;
use mtprsto::rpc;
use mtprsto::types::{self, InputPeer, MsgId};

use crate::tg::{TgManager, get_messages_by_id, is_file_reference_error, message_document};

/// `upload.getFile` alignment: Telegram requires offsets divisible by
/// 4 KiB. A range start in the middle of a block aligns down, and the
/// prefix bytes are dropped on the wire instead of being served.
const ALIGN: u64 = 4096;
/// Per-request chunk. 1 MiB is the server's cap per `upload.getFile`.
const CHUNK: usize = 1024 * 1024;

/// A byte stream backed by `upload.getFile` that transparently refetches
/// the message when the stored `file_reference` expires mid-stream,
/// resuming from the exact offset already served. Serves from byte `start`
/// (HTTP Range support): whole blocks are skipped server-side on Telegram,
/// the sub-block remainder is discarded on the wire.
#[allow(
    clippy::arithmetic_side_effects, // byte offsets bounded by the file size and cap
    clippy::as_conversions,          // u64 offset to i64: file sizes < i64::MAX
    clippy::indexing_slicing,        // slice/len arithmetic on the validated chunk buffer
)]
pub async fn file_stream_from(
    tg: &TgManager,
    message_id: i32,
    chat: &str,
    start: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    let (client, peer, _bot, _dc_id, pool) = tg.pool_target(chat).await?;

    let st = StreamState {
        client,
        peer,
        pool,
        msg_id: message_id,
        loc: None,
        offset: start - start % ALIGN,
        discard: start % ALIGN,
        done: false,
    };

    Ok(unfold(st, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            if st.loc.is_none() {
                // Fetch (or refetch) the message: its document carries the
                // fresh file_reference every download needs.
                let msgs = {
                    let c = st.client.lock().await;
                    get_messages_by_id(&c, &st.peer, &[MsgId(i64::from(st.msg_id))]).await
                };
                let msgs = match msgs {
                    Ok(m) => m,
                    Err(e) => {
                        return Some((
                            Err(std::io::Error::other(format!("fetch message: {e}"))),
                            st,
                        ));
                    }
                };
                let Some(msg) = msgs.into_iter().next() else {
                    return Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "message no longer exists",
                        )),
                        st,
                    ));
                };
                let Some(doc) = message_document(&msg) else {
                    return Some((
                        Err(std::io::Error::other("message has no document media")),
                        st,
                    ));
                };
                let Some(loc) = doc.location() else {
                    return Some((
                        Err(std::io::Error::other("document is a placeholder with no media")),
                        st,
                    ));
                };
                st.loc = Some(loc);
            }

            let Some(loc) = st.loc.as_ref() else {
                return Some((
                    Err(std::io::Error::other("download location missing")),
                    st,
                ));
            };
            let payload = rpc::build_get_file(loc, st.offset.cast_signed(), CHUNK as i32);
            // Ok(None) means "file reference expired — location rebuilt,
            // fetch again at the same offset".
            let bytes = match st.pool.send_rpc(&payload).await {
                Ok(raw) => match mtprsto::file::parse_get_file(&raw) {
                    Ok(mtprsto::file::GetFile::File { bytes, .. }) => Some(bytes),
                    Ok(_) => {
                        return Some((
                            Err(std::io::Error::other(
                                "file is served from a CDN, which is not supported",
                            )),
                            st,
                        ));
                    }
                    Err(e) if is_file_reference_error(&e) => {
                        tracing::info!("file reference expired mid-download; refetching");
                        st.loc = None;
                        None
                    }
                    Err(e) => {
                        return Some((
                            Err(std::io::Error::other(format!("download failed: {e}"))),
                            st,
                        ));
                    }
                },
                Err(e) if is_file_reference_error(&e) => {
                    tracing::info!("file reference expired mid-download; refetching");
                    st.loc = None;
                    None
                }
                Err(e) => {
                    return Some((
                        Err(std::io::Error::other(format!("download failed: {e}"))),
                        st,
                    ));
                }
            };
            let Some(bytes) = bytes else { continue };
            if bytes.is_empty() {
                // Nothing at this offset: the file is over.
                return None;
            }
            let raw_len = bytes.len();
            // A short read means EOF: serve this chunk, then stop.
            if raw_len < CHUNK {
                st.done = true;
            }
            st.offset += raw_len as u64;
            let mut chunk = Bytes::from(bytes);
            if st.discard > 0 {
                let d = usize::try_from(st.discard)
                    .unwrap_or(chunk.len())
                    .min(chunk.len());
                chunk = chunk.slice(d..);
                st.discard -= d as u64;
                if chunk.is_empty() {
                    // The entire chunk was consumed skipping the range
                    // prefix — fetch the next one rather than yield an
                    // empty Bytes (which the caller treats as EOF).
                    continue;
                }
            }
            return Some((Ok(chunk), st));
        }
    }))
}

struct StreamState {
    client: Arc<tokio::sync::Mutex<Client>>,
    peer: InputPeer,
    pool: Arc<SenderPool>,
    msg_id: i32,
    /// Download location rebuilt from the message each time the file
    /// reference goes stale.
    loc: Option<types::FileLocation>,
    offset: u64,
    /// Bytes still to drop from the first served chunk (start % ALIGN).
    discard: u64,
    done: bool,
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
    let Some(nonce) = p.nonce.as_deref().and_then(crate::crypt::nonce_from_b64) else {
        // Plaintext part: serve the stored bytes as-is.
        return Ok(Box::pin(
            file_stream_from(tg, p.message_id, &p.chat, skip).await?,
        ));
    };
    // Encrypted part — the stored bytes are a container. Decryption needs a
    // key; without one the operator has disabled or removed the key while
    // leaving encrypted files behind, which must not silently serve garbage.
    let Some(key) = key else {
        return Err("file is encrypted but no crypt_password is configured".into());
    };
    if skip == 0 {
        let inner = file_stream_from(tg, p.message_id, &p.chat, 0).await?;
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
        let inner = file_stream_from(tg, p.message_id, &p.chat, ct_off).await?;
        let dec = crate::crypt::DecryptingStream::at_block(
            Box::pin(inner),
            key,
            nonce,
            blocks,
            intra,
        );
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
