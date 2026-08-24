use grammers_client::InvocationError;
use grammers_client::message::InputMessage;

use super::{
    FILE_REFERENCE_EXPIRED, FILEREF_UPGRADE_NEEDED, TgManager, friendly, stripped_thumb_jpeg,
};

impl TgManager {
    /// Streams `reader` up to Telegram and posts it as a document message in
    /// the given storage chat. Returns `(message id, name, mime, thumb)` —
    /// thumb is the tiny JPEG Telegram generates, when it made one.
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
        let (client, peer, bot) = self.pool_target(chat).await?;
        tracing::info!(
            bot = bot.as_deref().unwrap_or("owner"),
            file = %name,
            chat = %chat,
            size,
            "upload start"
        );

        let uploaded = client
            .upload_stream(reader, size as usize, name.to_string())
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

pub(crate) fn is_file_reference_error(err: &InvocationError) -> bool {
    if let InvocationError::Rpc(rpc) = err {
        rpc.is(FILE_REFERENCE_EXPIRED) || rpc.is(FILEREF_UPGRADE_NEEDED)
    } else {
        false
    }
}
