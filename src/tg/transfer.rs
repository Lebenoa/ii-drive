#![allow(clippy::large_futures)] // send_message/run futures are awaited directly here; boxing adds no value
use grammers_client::InvocationError;
use grammers_client::message::InputMessage;

use super::{
    FILE_REFERENCE_EXPIRED, FILEREF_UPGRADE_NEEDED, TgManager, friendly, stripped_thumb_jpeg,
};

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

        // upload_stream_parallel is always valid for send_message.
        let uploaded = client
            .upload_stream_parallel(reader, size as usize, name.to_string(), dc_id, &pools)
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
                let uploaded = client
                    .upload_stream_parallel(&mut reader, size as usize, "bench.bin".to_string(), dc_id, &pools)
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
