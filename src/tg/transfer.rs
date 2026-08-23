use grammers_client::message::InputMessage;
use grammers_client::InvocationError;

use super::{FILEREF_UPGRADE_NEEDED, FILE_REFERENCE_EXPIRED, TgManager, friendly, stripped_thumb_jpeg};

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
        // Upload through a rotating bot session when available: concurrent
        // part tasks then run on separate MTProto connections (one per
        // bot), instead of pipelining everything over the user session.
        // Falls back to the user session when no bot can serve this chat.
        let (client, peer) = self.download_target(chat).await?;

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
            grammers_client::media::Media::Document(doc) => doc
                .thumbs()
                .into_iter()
                .find_map(|t| match t {
                    grammers_client::media::PhotoSize::Stripped(s) => {
                        let jpeg = stripped_thumb_jpeg(&s.bytes);
                        (!jpeg.is_empty()).then_some(jpeg)
                    }
                    _ => None,
                }),
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
