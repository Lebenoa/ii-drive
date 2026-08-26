use bytes::Bytes;
use futures::StreamExt;
use futures::stream::{Stream, unfold};

use crate::tg::{PeerRef, is_file_reference_error};

/// Telegram large-file chunk size (512 KiB = `MAX_CHUNK_SIZE`). Must be a
/// multiple of the protocol minimum; used to compute skip offsets on retry.
const CHUNK: i32 = 512 * 1024;

/// A byte stream backed by `iter_download` that transparently refetches the
/// message when the stored `file_reference` expires mid-stream, resuming from
/// the exact offset already served. Serves from byte `start` (HTTP Range
/// support): whole chunks are skipped server-side on Telegram, the
/// sub-chunk remainder is discarded on the wire.
#[allow(
    clippy::arithmetic_side_effects, // byte offsets bounded by the file size and cap
    clippy::as_conversions,          // i32/i64 chunk-offset bridging, bounded offset values
    clippy::cast_possible_truncation, // chunk indices always fit i32 (Telegram caps docs)
    clippy::cast_possible_wrap,       // u64 offset to i64: file sizes < i64::MAX
    clippy::indexing_slicing,         // slice/len arithmetic on the validated chunk buffer
)]
pub async fn file_stream_from(
    tg: &crate::tg::TgManager,
    message_id: i32,
    chat: &str,
    start: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    let (client, peer, _bot) = tg.pool_target(chat).await?;

    let st = StreamState {
        client,
        peer,
        msg_id: message_id,
        iter: None,
        pos: start.cast_signed(),
        discard: start % CHUNK as u64,
    };

    Ok(unfold(st, |mut st| async move {
        loop {
            if st.iter.is_none() {
                let msgs = match st.client.get_messages_by_id(st.peer, &[st.msg_id]).await {
                    Ok(m) => m,
                    Err(e) => {
                        return Some((
                            Err(std::io::Error::other(format!("fetch message: {e}"))),
                            st,
                        ));
                    }
                };
                let Some(msg) = msgs.into_iter().next().flatten() else {
                    return Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "message no longer exists",
                        )),
                        st,
                    ));
                };
                let Some(grammers_client::media::Media::Document(doc)) = msg.media() else {
                    return Some((
                        Err(std::io::Error::other("message has no document media")),
                        st,
                    ));
                };
                let mut it = st.client.iter_download(&doc).chunk_size(CHUNK);
                let chunks = st.pos / i64::from(CHUNK);
                // chunk count = stream position / 512KiB. Telegram caps a
                // single document at ~2 GiB, so this always fits i32.
                let skipped = i32::try_from(chunks).unwrap_or(0);
                if skipped > 0 {
                    it = it.skip_chunks(skipped);
                }
                st.iter = Some(it);
            }

            // The block above guarantees the iterator exists; fall through
            // to a stream error rather than panicking if that invariant
            // ever breaks.
            let Some(iter) = st.iter.as_mut() else {
                return Some((Err(std::io::Error::other("download iterator missing")), st));
            };
            match iter.next().await {
                Ok(Some(chunk)) => {
                    let mut chunk = Bytes::from(chunk);
                    if st.discard > 0 && !chunk.is_empty() {
                        let d = usize::try_from(st.discard).unwrap_or(chunk.len());
                        chunk = chunk.slice(d..);
                        st.discard -= d as u64;
                        if chunk.is_empty() {
                            // The entire chunk was consumed skipping the
                            // range prefix — read the next one rather than
                            // yield an empty Bytes (which the caller treats
                            // as EOF). Not needless: `continue` is followed
                            // by a `return` below.
                            #[allow(clippy::needless_continue)]
                            continue;
                        }
                    }
                    st.pos += chunk.len() as i64;
                    return Some((Ok(chunk), st));
                }
                Ok(None) => return None,
                Err(e) if is_file_reference_error(&e) => {
                    tracing::info!("file reference expired mid-download; refetching");
                    st.iter = None;
                }
                Err(e) => {
                    return Some((
                        Err(std::io::Error::other(format!("download failed: {e}"))),
                        st,
                    ));
                }
            }
        }
    }))
}

struct StreamState {
    client: grammers_client::Client,
    peer: PeerRef,
    msg_id: i32,
    iter: Option<grammers_client::client::DownloadIter>,
    pos: i64,
    /// Bytes still to drop from the first served chunk (start % CHUNK).
    discard: u64,
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

/// Serves a multi-part file as one contiguous byte stream: parts are fetched
/// in order, each through `file_stream` (which itself resumes on expired
/// file references), so the client sees a seamless download.
/// Serves a multi-part file as one contiguous byte stream, optionally from
/// byte `start` across part boundaries (HTTP Range support). Parts are
/// fetched in order, each through `file_stream` (which itself resumes on
/// expired file references), so the client sees a seamless download.
#[allow(
    clippy::arithmetic_side_effects, // skip/idx math bounded by sum of part sizes
    clippy::indexing_slicing, // parts[idx] has an explicit idx < parts.len() guard
)]
pub async fn parts_stream_from(
    tg: std::sync::Arc<crate::tg::TgManager>,
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
    let key: Option<std::sync::Arc<crate::crypt::Key>> =
        crate::config::get().crypt_key_unconditional().map(std::sync::Arc::new);
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
    tg: std::sync::Arc<crate::tg::TgManager>,
    parts: Vec<crate::db::FilePart>,
    idx: usize,
    cur: Option<BoxedPart>,
    key: Option<std::sync::Arc<crate::crypt::Key>>,
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
    tg: &crate::tg::TgManager,
    parts: &[crate::db::FilePart],
    idx: usize,
    skip: u64,
    key: Option<&crate::crypt::Key>,
) -> Result<BoxedPart, String> {
    let p = &parts[idx];
    let Some(nonce) = p.nonce.as_deref().and_then(crate::crypt::nonce_from_b64) else {
        // Plaintext part: serve the stored bytes as-is.
        return Ok(Box::pin(file_stream_from(tg, p.message_id, &p.chat, skip).await?));
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
