use grammers_client::message::InputMessage;
use grammers_client::tl;
use grammers_client::Client;

use super::{PeerRef, TgManager, bot_token_regex, friendly};

impl TgManager {
    /// Sends `text` to @BotFather and returns its reply text. The
    /// conversation state lives on BotFather's side of this account's chat,
    /// so every signed-in account can run its own wizard through this one
    /// relay primitive without any state on our side.
    pub async fn botfather_send(&self, text: &str) -> Result<String, String> {
        let client = self.ensure().await?;
        let peer = self.storage_peer("botfather").await?;
        let sent = client
            .send_message(peer, InputMessage::new().text(text))
            .await
            .map_err(|e| friendly(format!("botfather send failed: {e}")))?;

        // BotFather usually answers within a second; poll the dialog for a
        // newer incoming message. Give up after ~8s so the HTTP request
        // cannot hang.
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(
                if attempt < 4 { 300 } else { 600 },
            ))
            .await;
            let mut it = client.iter_messages(peer).limit(1);
            if let Ok(Some(msg)) = it.next().await
                && !msg.outgoing()
                && msg.id() > sent.id()
            {
                return Ok(msg.text().to_string());
            }
        }
        Err("BotFather did not answer — try again shortly".to_string())
    }

    /// Callback buttons `(text, data)` of the newest incoming BotFather
    /// message — the labels of its inline menu, if any.
    async fn last_buttons(
        client: &Client,
        peer_ref: PeerRef,
    ) -> Result<(i32, Vec<(String, Vec<u8>)>), String> {
        let mut it = client.iter_messages(peer_ref).limit(1);
        if let Ok(Some(msg)) = it.next().await {
            let msg_id = msg.id();
            let Some(rm) = msg.reply_markup() else {
                return Ok((msg_id, Vec::new()));
            };
            let tl::enums::ReplyMarkup::ReplyInlineMarkup(markup) = rm else {
                return Ok((msg_id, Vec::new()));
            };
            let buttons = markup
                .rows
                .into_iter()
                .flat_map(|row| match row {
                    tl::enums::KeyboardButtonRow::Row(r) => r.buttons,
                })
                .filter_map(|b| match b {
                    tl::enums::KeyboardButton::Callback(cb) => Some((cb.text, cb.data)),
                    _ => None,
                })
                .collect();
            return Ok((msg_id, buttons));
        }
        Err("BotFather did not answer".into())
    }

    /// Presses the callback button matching `needle` on the latest
    /// BotFather menu (raw TL invoke — grammers has no click helper).
    async fn press_botfather_button(
        &self,
        client: &Client,
        peer_ref: PeerRef,
        needle: &str,
    ) -> Result<(), String> {
        let (msg_id, buttons) = Self::last_buttons(client, peer_ref).await?;
        let (_, data) = buttons
            .into_iter()
            .find(|(text, _)| text.to_lowercase().contains(&needle.to_lowercase()))
            .ok_or_else(|| format!("BotFather menu has no `{needle}` button"))?;
        let peer = client
            .resolve_peer(peer_ref)
            .await
            .map_err(|e| format!("resolve botfather failed: {e}"))?;
        let input_peer = match &peer {
            grammers_client::peer::Peer::User(u) => match u.raw {
                tl::enums::User::User(ref usr) => {
                    tl::enums::InputPeer::User(tl::types::InputPeerUser {
                        user_id: usr.id,
                        access_hash: usr.access_hash.unwrap_or(0),
                    })
                }
                _ => return Err("botfather resolved to an empty account".into()),
            },
            _ => return Err("botfather is not a user".into()),
        };
        let req = tl::functions::messages::GetBotCallbackAnswer {
            game: false,
            peer: input_peer,
            msg_id,
            data: Some(data),
            password: None,
        };
        client
            .invoke(&req)
            .await
            .map_err(|e| friendly(format!("button press failed: {e}")))?;
        Ok(())
    }

    /// Lists the bots this account owns according to @BotFather's `/mybots`
    /// menu (the button labels are the bot names).
    pub async fn botfather_my_bots(&self) -> Result<Vec<String>, String> {
        let client = self.ensure().await?;
        let peer_ref = self.storage_peer("botfather").await?;
        client
            .send_message(peer_ref, InputMessage::new().text("/mybots"))
            .await
            .map_err(|e| friendly(format!("botfather send failed: {e}")))?;
        let (_, buttons) = Self::last_buttons(&client, peer_ref).await?;
        Ok(buttons.into_iter().map(|(text, _)| text).collect())
    }

    /// Retrieves the API token for `bot` by walking @BotFather's menus:
    /// /mybots → pick the bot → "API Token". The token arrives as plain
    /// text in the follow-up message.
    pub async fn botfather_bot_token(&self, bot: &str) -> Result<String, String> {
        let client = self.ensure().await?;
        let peer_ref = self.storage_peer("botfather").await?;
        client
            .send_message(peer_ref, InputMessage::new().text("/mybots"))
            .await
            .map_err(|e| friendly(format!("botfather send failed: {e}")))?;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        self.press_botfather_button(&client, peer_ref, bot).await?;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        self.press_botfather_button(&client, peer_ref, "API Token")
            .await?;

        // The token lands in a fresh incoming message; poll for it.
        let token_re = bot_token_regex();
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(
                if attempt < 4 { 300 } else { 600 },
            ))
            .await;
            let mut it = client.iter_messages(peer_ref).limit(3);
            while let Ok(Some(msg)) = it.next().await {
                if !msg.outgoing() && token_re.is_match(msg.text()) {
                    return Ok(token_re.find(msg.text()).unwrap().as_str().to_string());
                }
            }
        }
        Err("no token received from BotFather".into())
    }
}
