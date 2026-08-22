use bytes::Bytes;
use futures::stream::{Stream, unfold};
use futures::StreamExt;

use crate::tg::{PeerRef, is_file_reference_error};

/// Telegram large-file chunk size (512 KiB = MAX_CHUNK_SIZE). Must be a
/// multiple of the protocol minimum; used to compute skip offsets on retry.
const CHUNK: i32 = 512 * 1024;

/// A byte stream backed by `iter_download` that transparently refetches the
/// message when the stored file_reference expires mid-stream, resuming from
/// the exact offset already served.
pub async fn file_stream(
    tg: &crate::tg::TgManager,
    message_id: i32,
    chat: &str,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    file_stream_from(tg, message_id, chat, 0).await
}

/// Same as `file_stream`, but serving from byte `start` (HTTP Range
/// support): whole chunks are skipped server-side on Telegram, the
/// sub-chunk remainder is discarded on the wire.
pub async fn file_stream_from(
    tg: &crate::tg::TgManager,
    message_id: i32,
    chat: &str,
    start: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    let (client, peer) = tg.download_target(chat).await?;

    let st = StreamState {
        client,
        peer,
        msg_id: message_id,
        iter: None,
        pos: start as i64,
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
                let skipped = (st.pos / CHUNK as i64) as i32;
                if skipped > 0 {
                    it = it.skip_chunks(skipped);
                }
                st.iter = Some(it);
            }

            let iter = st.iter.as_mut().expect("iter just set");
            match iter.next().await {
                Ok(Some(chunk)) => {
                    let mut chunk = Bytes::from(chunk);
                    if st.discard > 0 && !chunk.is_empty() {
                        let d = (st.discard as usize).min(chunk.len());
                        chunk = chunk.slice(d..);
                        st.discard -= d as u64;
                        if chunk.is_empty() {
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
                    continue;
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

type BoxedPart = std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Serves a multi-part file as one contiguous byte stream: parts are fetched
/// in order, each through `file_stream` (which itself resumes on expired
/// file references), so the client sees a seamless download.
/// Serves a multi-part file as one contiguous byte stream, optionally from
/// byte `start` across part boundaries (HTTP Range support). Parts are
/// fetched in order, each through `file_stream` (which itself resumes on
/// expired file references), so the client sees a seamless download.
pub async fn parts_stream_from(
    tg: std::sync::Arc<crate::tg::TgManager>,
    parts: Vec<crate::db::FilePart>,
    start: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>> + use<>, String> {
    // Find the part holding `start` and the offset within it.
    let mut skip = start;
    let mut idx = 0usize;
    while idx < parts.len() && skip >= parts[idx].size as u64 {
        skip -= parts[idx].size as u64;
        idx += 1;
    }
    if idx >= parts.len() {
        return Err("range start is beyond the file".into());
    }
    let first = &parts[idx];
    let cur: BoxedPart =
        Box::pin(file_stream_from(&tg, first.message_id, &first.chat, skip).await?);
    let st = PartsState { tg, parts, idx, cur: Some(cur) };
    Ok(unfold(st, |mut st| async move {
        loop {
            let item = match st.cur.as_mut()?.next().await {
                Some(item) => item,
                None => {
                    st.idx += 1;
                    if st.idx >= st.parts.len() {
                        return None;
                    }
                    let p = &st.parts[st.idx];
                    match file_stream_from(&st.tg, p.message_id, &p.chat, 0).await {
                        Ok(s) => st.cur = Some(Box::pin(s)),
                        Err(e) => return Some((Err(std::io::Error::other(e)), st)),
                    }
                    continue;
                }
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
}
