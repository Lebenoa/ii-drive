use grammers_client::Client;
use grammers_client::message::InputMessage;
use grammers_client::tl;

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
            tokio::time::sleep(std::time::Duration::from_millis(if attempt < 4 {
                300
            } else {
                600
            }))
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

    /// Flat callback-button list `(text, data)` of a reply markup — empty
    /// for plain-text messages and other markup kinds.
    fn markup_buttons(rm: Option<tl::enums::ReplyMarkup>) -> Vec<(String, Vec<u8>)> {
        let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = rm else {
            return Vec::new();
        };
        markup
            .rows
            .into_iter()
            .flat_map(|row| match row {
                tl::enums::KeyboardButtonRow::Row(r) => r.buttons,
            })
            .filter_map(|b| match b {
                tl::enums::KeyboardButton::Callback(cb) => Some((cb.text, cb.data)),
                _ => None,
            })
            .collect()
    }

    /// Callback buttons `(text, data)` of the newest message in the
    /// BotFather chat, whatever direction it has.
    async fn last_buttons(
        client: &Client,
        peer_ref: PeerRef,
    ) -> Result<(i32, Vec<(String, Vec<u8>)>), String> {
        let mut it = client.iter_messages(peer_ref).limit(1);
        if let Ok(Some(msg)) = it.next().await {
            let msg_id = msg.id();
            return Ok((msg_id, Self::markup_buttons(msg.reply_markup())));
        }
        Err("BotFather did not answer".into())
    }

    /// Waits for BotFather's first INCOMING message newer than `after_id`
    /// and returns its id plus callback buttons — his answer to a command.
    /// Polling is required: the reply arrives asynchronously, and reading
    /// too early would see our own outgoing request (which carries no
    /// keyboard) and mistake it for an empty answer. A text-only reply
    /// (e.g. "no bots yet") returns `Ok` with no buttons; silence errors.
    async fn await_reply_buttons(
        client: &Client,
        peer_ref: PeerRef,
        after_id: i32,
    ) -> Result<(i32, Vec<(String, Vec<u8>)>), String> {
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(if attempt < 4 {
                300
            } else {
                600
            }))
            .await;
            let mut it = client.iter_messages(peer_ref).limit(1);
            if let Ok(Some(msg)) = it.next().await
                && !msg.outgoing()
                && msg.id() > after_id
            {
                let msg_id = msg.id();
                return Ok((msg_id, Self::markup_buttons(msg.reply_markup())));
            }
        }
        Err("BotFather did not answer — try again shortly".into())
    }

    /// Presses the callback button matching `needle` on the latest
    /// BotFather menu.
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
        Self::press_callback(client, peer_ref, msg_id, &data).await
    }

    /// Raw TL callback press — grammers has no click helper. BotFather
    /// answers by editing the same message, so `msg_id` stays valid across
    /// pages of a paginated menu.
    async fn press_callback(
        client: &Client,
        peer_ref: PeerRef,
        msg_id: i32,
        data: &[u8],
    ) -> Result<(), String> {
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
            data: Some(data.to_vec()),
            password: None,
        };
        client
            .invoke(&req)
            .await
            .map_err(|e| friendly(format!("button press failed: {e}")))?;
        Ok(())
    }

    /// Lists the bots this account owns according to @BotFather's `/mybots`
    /// menu (the button labels are the bot names). Waits for his reply,
    /// skips menu chrome such as pagination arrows and page counters, and
    /// follows `»` so owners of many bots get every page, not just one.
    pub async fn botfather_my_bots(&self) -> Result<Vec<String>, String> {
        const MAX_PAGES: usize = 12;

        let client = self.ensure().await?;
        let peer_ref = self.storage_peer("botfather").await?;
        let sent = client
            .send_message(peer_ref, InputMessage::new().text("/mybots"))
            .await
            .map_err(|e| friendly(format!("botfather send failed: {e}")))?;
        let (mut msg_id, mut buttons) =
            Self::await_reply_buttons(&client, peer_ref, sent.id()).await?;

        let mut names: Vec<String> = Vec::new();
        let mut listed = std::collections::HashSet::new();
        let mut visited = std::collections::HashSet::new();
        for _ in 0..MAX_PAGES {
            for (text, _) in &buttons {
                if !is_menu_chrome(text) && listed.insert(text.clone()) {
                    names.push(text.clone());
                }
            }
            // Follow the next-page arrow when there is one; the last page
            // only offers `«`. BotFather edits the same message per page.
            let Some((_, data)) = buttons.iter().find(|(t, _)| t.trim() == "»") else {
                break;
            };
            if !visited.insert(data.clone()) {
                break;
            }
            if let Err(e) = Self::press_callback(&client, peer_ref, msg_id, data).await {
                tracing::warn!("botfather /mybots stopped at a page boundary: {e}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let (id, next) = Self::last_buttons(&client, peer_ref).await?;
            msg_id = id;
            buttons = next;
        }
        Ok(names)
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
        // Regex compiled once; a None here means the pattern itself is
        // broken, so bail out with an error instead of matching anything.
        let Some(token_re) = bot_token_regex() else {
            return Err("bot token matcher unavailable".into());
        };
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(if attempt < 4 {
                300
            } else {
                600
            }))
            .await;
            let mut it = client.iter_messages(peer_ref).limit(3);
            while let Ok(Some(msg)) = it.next().await {
                if let Some(m) = (!msg.outgoing())
                    .then(|| token_re.find(msg.text()))
                    .flatten()
                {
                    return Ok(m.as_str().to_string());
                }
            }
        }
        Err("no token received from BotFather".into())
    }
}

/// True for @BotFather menu chrome rather than a bot entry: pagination
/// arrows (`«`, `»`, …) and page counters like `2/3` on multi-page lists.
/// A real entry is always a bot name or @username, so anything made purely
/// of navigation glyphs, digits, slashes and spaces is chrome.
fn is_menu_chrome(text: &str) -> bool {
    text.trim().chars().all(|c| {
        matches!(
            c,
            '«' | '»' | '‹' | '›' | '←' | '→' | '↔' | '◀' | '▶' | '⏪' | '⏩' | '/' | '0'
                ..='9' | ' ' | '\u{a0}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::is_menu_chrome;

    #[test]
    fn pagination_buttons_are_chrome() {
        assert!(is_menu_chrome("»"));
        assert!(is_menu_chrome("«"));
        assert!(is_menu_chrome(" ‹ "));
        assert!(is_menu_chrome("2/3"));
        assert!(is_menu_chrome("« 2 / 3 »"));
        assert!(is_menu_chrome(""));
    }

    #[test]
    fn bot_labels_are_not_chrome() {
        assert!(!is_menu_chrome("@my_bot"));
        assert!(!is_menu_chrome("My Drive Bot"));
        // Non-ASCII display names are still bot entries, not glyphs.
        assert!(!is_menu_chrome("机器人"));
    }
}
