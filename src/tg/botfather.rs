use mtprsto::client::Client;
use mtprsto::rpc;
use mtprsto::types::{IncomingReplyMarkup, KeyboardButtonKind, Message};

use super::{PeerRef, TgManager, bot_token_regex, friendly, last_messages, send_text};

impl TgManager {
    /// Sends `text` to @`BotFather` and returns its reply text. The
    /// conversation state lives on `BotFather`'s side of this account's chat,
    /// so every signed-in account can run its own wizard through this one
    /// relay primitive without any state on our side.
    pub async fn botfather_send(&self, text: &str) -> Result<String, String> {
        let client = self.ensure_connected().await?;
        let peer = self.storage_peer("botfather").await?;
        let sent = send_text(&mut *client.lock().await, &peer, text).await?;

        // BotFather usually answers within a second; poll the dialog for a
        // newer message. Give up after ~8s so the HTTP request cannot hang.
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(if attempt < 4 {
                300
            } else {
                600
            }))
            .await;
            let msgs = last_messages(&*client.lock().await, &peer, 1).await?;
            if let Some(msg) = msgs.first()
                && msg.id().0 > sent.0
            {
                return Ok(msg.text().to_string());
            }
        }
        Err("BotFather did not answer — try again shortly".to_string())
    }

    /// Flat callback-button list `(text, data)` of a message's reply markup —
    /// empty for plain-text messages and other markup kinds.
    fn markup_buttons(msg: &Message) -> Vec<(String, Vec<u8>)> {
        let Message::Message(full) = msg else {
            return Vec::new();
        };
        let Some(IncomingReplyMarkup::Inline { rows }) = full.reply_markup.as_ref() else {
            return Vec::new();
        };
        rows.iter()
            .flat_map(|row| row.buttons.iter())
            .filter_map(|b| match b {
                KeyboardButtonKind::Callback { text, data } => {
                    Some((text.clone(), data.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Callback buttons `(text, data)` of the newest message in the
    /// `BotFather` chat, whatever direction it has.
    async fn last_buttons(
        client: &Client,
        peer_ref: &PeerRef,
    ) -> Result<(i32, Vec<(String, Vec<u8>)>), String> {
        let msgs = last_messages(client, peer_ref, 1).await?;
        let Some(msg) = msgs.first() else {
            return Err("BotFather did not answer".into());
        };
        let msg_id = msg.id().0 as i32;
        Ok((msg_id, Self::markup_buttons(msg)))
    }

    /// Waits for `BotFather`'s first message newer than `after_id` and
    /// returns its id plus callback buttons — his answer to a command.
    /// Polling is required: the reply arrives asynchronously, and reading
    /// too early would see our own outgoing request (which carries no
    /// keyboard) and mistake it for an empty answer. A text-only reply
    /// (e.g. "no bots yet") returns `Ok` with no buttons; silence errors.
    async fn await_reply_buttons(
        client: &Client,
        peer_ref: &PeerRef,
        after_id: i32,
    ) -> Result<(i32, Vec<(String, Vec<u8>)>), String> {
        for attempt in 0u32..16 {
            tokio::time::sleep(std::time::Duration::from_millis(if attempt < 4 {
                300
            } else {
                600
            }))
            .await;
            let msgs = last_messages(client, peer_ref, 1).await?;
            if let Some(msg) = msgs.first()
                && msg.id().0 > i64::from(after_id)
            {
                let msg_id = msg.id().0 as i32;
                return Ok((msg_id, Self::markup_buttons(msg)));
            }
        }
        Err("BotFather did not answer — try again shortly".into())
    }

    /// Presses the callback button matching `needle` on the latest
    /// `BotFather` menu.
    async fn press_botfather_button(
        &self,
        client: &Client,
        peer_ref: &PeerRef,
        needle: &str,
    ) -> Result<(), String> {
        let (msg_id, buttons) = Self::last_buttons(client, peer_ref).await?;
        let (_, data) = buttons
            .into_iter()
            .find(|(text, _)| text.to_lowercase().contains(&needle.to_lowercase()))
            .ok_or_else(|| format!("BotFather menu has no `{needle}` button"))?;
        Self::press_callback(client, peer_ref, msg_id, &data).await
    }

    /// Raw `messages.getBotCallbackAnswer` press. `BotFather` answers by
    /// editing the same message, so `msg_id` stays valid across pages of a
    /// paginated menu.
    async fn press_callback(
        client: &Client,
        peer_ref: &PeerRef,
        msg_id: i32,
        data: &[u8],
    ) -> Result<(), String> {
        let req = rpc::build_get_bot_callback_answer(peer_ref, msg_id, data);
        client
            .invoke_raw(req)
            .await
            .map_err(|e| friendly(format!("button press failed: {e}")))?;
        Ok(())
    }

    /// Lists the bots this account owns according to @`BotFather`'s `/mybots`
    /// menu (the button labels are the bot names). Waits for his reply,
    /// skips menu chrome such as pagination arrows and page counters, and
    /// follows `»` so owners of many bots get every page, not just one.
    pub async fn botfather_my_bots(&self) -> Result<Vec<String>, String> {
        const MAX_PAGES: usize = 12;

        let client = self.ensure_connected().await?;
        let peer_ref = self.storage_peer("botfather").await?;
        let sent = send_text(&mut *client.lock().await, &peer_ref, "/mybots").await?;
        let (mut msg_id, mut buttons) =
            Self::await_reply_buttons(&*client.lock().await, &peer_ref, sent.0 as i32).await?;

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
            if let Err(e) = Self::press_callback(&*client.lock().await, &peer_ref, msg_id, data)
                .await
            {
                tracing::warn!("botfather /mybots stopped at a page boundary: {e}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let (id, next) = Self::last_buttons(&*client.lock().await, &peer_ref).await?;
            msg_id = id;
            buttons = next;
        }
        Ok(names)
    }

    /// Retrieves the API token for `bot` by walking @`BotFather`'s menus:
    /// /mybots → pick the bot → "API Token". The token arrives as plain
    /// text in the follow-up message.
    pub async fn botfather_bot_token(&self, bot: &str) -> Result<String, String> {
        let client = self.ensure_connected().await?;
        let peer_ref = self.storage_peer("botfather").await?;
        send_text(&mut *client.lock().await, &peer_ref, "/mybots").await?;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        self.press_botfather_button(&*client.lock().await, &peer_ref, bot)
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        self.press_botfather_button(&*client.lock().await, &peer_ref, "API Token")
            .await?;

        // The token lands in a fresh message; poll for it. Regex compiled
        // once; a None here means the pattern itself is broken, so bail out
        // with an error instead of matching anything.
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
            let msgs = last_messages(&*client.lock().await, &peer_ref, 3).await?;
            for msg in &msgs {
                if let Some(m) = token_re.find(msg.text()) {
                    return Ok(m.as_str().to_string());
                }
            }
        }
        Err("no token received from BotFather".into())
    }
}

/// True for @`BotFather` menu chrome rather than a bot entry: pagination
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
