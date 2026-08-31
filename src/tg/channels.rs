use std::collections::HashSet;
use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::rpc;
use mtprsto::types::{self, Chat, Dialogs, InputPeer};
use tokio::sync::Mutex;

use super::{ChannelInfo, PeerRef, TgManager, friendly};

impl TgManager {
    /// Resolves (and caches) a chat key — "me", "@username" or "-100<id>" —
    /// to a peer reference.
    pub async fn storage_peer(&self, chat: &str) -> Result<PeerRef, String> {
        let key = chat.trim();
        let cache_key = key.to_ascii_lowercase();

        {
            let st = self.st.lock().await;
            if let Some(peer) = st.peers.get(&cache_key) {
                return Ok(peer.clone());
            }
        }

        let peer = if key.is_empty()
            || key.eq_ignore_ascii_case("me")
            || key.eq_ignore_ascii_case("self")
        {
            InputPeer::Self_
        } else {
            let client = self.ensure_connected().await?;
            // mtprsto resolves numeric bot-api ids ("-100…"), plain ids and
            // usernames (with or without the "@") against the session's
            // access-hash cache and channels.getChannels.
            let mut c = client.lock().await;
            c.resolve_peer(key)
                .await
                .map_err(|e| friendly(format!("cannot resolve chat {key}: {e}")))?
        };

        self.st.lock().await.peers.insert(cache_key, peer.clone());
        Ok(peer)
    }

    /// Channels and groups this account could use as storage targets:
    /// both the main dialog list AND the archived folder (storage channels
    /// are typically muted and archived, so they only show up there).
    /// "Saved Messages" is always first.
    pub async fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let client = self.ensure_connected().await?;
        let mut out = vec![ChannelInfo {
            chat: "me".to_string(),
            title: "Saved Messages (my account)".to_string(),
        }];
        let mut seen: HashSet<String> = HashSet::new();
        Self::collect_folder(&client, None, &mut out, &mut seen).await?;
        Self::collect_folder(&client, Some(1), &mut out, &mut seen).await?;
        Ok(out)
    }

    /// Harvests storage-capable chats (broadcast channels and supergroups) from
    /// one dialogs folder via raw `messages.getDialogs`, paginating until
    /// exhausted. Basic groups are skipped: bots cannot be wired into them.
    async fn collect_folder(
        client: &Arc<Mutex<Client>>,
        folder_id: Option<i32>,
        out: &mut Vec<ChannelInfo>,
        seen: &mut HashSet<String>,
    ) -> Result<(), String> {
        let mut offset_date: i32 = 0;
        let mut offset_id: i32 = 0;
        let mut offset_peer = InputPeer::InputPeerEmpty;

        for _page in 0..20 {
            let resp = {
                let c = client.lock().await;
                let payload =
                    rpc::build_get_dialogs(folder_id, offset_date, offset_id, &offset_peer, 100);
                c.invoke_raw(payload)
                    .await
                    .map_err(|e| friendly(format!("listing dialogs failed: {e}")))?
            };
            let dialogs = Dialogs::parse(&resp).map_err(|e| e.to_string())?;

            Self::harvest_chats(&dialogs.chats, out, seen);
            if dialogs.dialogs.len() < 100 {
                return Ok(());
            }

            // Advance the cursor to the last dialog of this page. The offset
            // date comes from that dialog's top message.
            let Some(dd) = dialogs.dialogs.last() else {
                return Ok(());
            };
            let raw_id = match &dd.peer {
                types::Peer::Channel { channel_id } => channel_id.0,
                types::Peer::Chat { chat_id } => chat_id.0,
                types::Peer::User { user_id } => user_id.0,
                types::Peer::None => return Ok(()),
            };
            let mut msg_date = 0i32;
            for m in &dialogs.messages {
                if let types::Message::Message(mm) = m {
                    if mm.id.0 != dd.top_message.0 {
                        continue;
                    }
                    let mid = match &mm.peer_id {
                        types::Peer::Channel { channel_id } => channel_id.0,
                        types::Peer::Chat { chat_id } => chat_id.0,
                        types::Peer::User { user_id } => user_id.0,
                        types::Peer::None => continue,
                    };
                    if mid == raw_id {
                        msg_date = mm.date;
                        break;
                    }
                }
            }
            offset_date = msg_date;
            // Telegram message ids are int32 on the wire; the i64 MsgId is
            // a library-side widening.
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let narrowed = dd.top_message.0 as i32;
            offset_id = narrowed;
            offset_peer = Self::input_peer_for(raw_id, &dialogs.chats);
        }
        Ok(())
    }

    /// Adds every chat this account can actually use for storage to the output.
    ///
    /// Membership is not enough. Storage wiring invites each download bot
    /// (`channels.inviteToChannel`) and promotes it to admin
    /// (`channels.editAdmin`), so the account needs `invite_users` and
    /// `add_admins` there. Creators hold every right implicitly. Chats we only
    /// read — someone else's group, a channel we merely joined — are dropped so
    /// they never reach the picker.
    fn harvest_chats(chats: &[Chat], out: &mut Vec<ChannelInfo>, seen: &mut HashSet<String>) {
        for c in chats {
            let Chat::Channel {
                creator,
                left,
                admin_rights,
                title,
                id,
                megagroup,
                ..
            } = c
            else {
                continue;
            };
            if *left && !*creator {
                continue; // we left it — cannot post
            }
            if !*creator && !Self::can_wire_bots(admin_rights.as_ref()) {
                // Read-only for us — bots cannot be wired in.
                tracing::debug!(
                    title = %title,
                    megagroup = *megagroup,
                    admin = admin_rights.is_some(),
                    "skipping chat: no rights to wire download bots"
                );
                continue;
            }
            // Codegen quirk: the generated channel carries its id as the
            // `ChatId` newtype — the value is the channel id either way.
            let key = format!("-100{}", id.0);
            if seen.insert(key.clone()) {
                out.push(ChannelInfo {
                    chat: key,
                    title: title.clone(),
                });
            }
        }
    }

    /// Whether an admin-rights grant covers inviting a download bot and
    /// promoting it to admin. Both are required: an un-promoted bot cannot read
    /// the channel's files back.
    const fn can_wire_bots(rights: Option<&types::ChatAdminRights>) -> bool {
        let Some(r) = rights else {
            return false;
        };
        r.invite_users && r.add_admins
    }

    /// Builds an `InputPeer` for pagination offsets from the same response's
    /// chats array (which carries the access hashes).
    fn input_peer_for(raw_id: i64, chats: &[Chat]) -> InputPeer {
        for c in chats {
            match c {
                Chat::Channel { id, access_hash, .. } if id.0 == raw_id => {
                    return InputPeer::Channel {
                        channel_id: types::ChannelId(id.0),
                        access_hash: types::AccessHash(access_hash.map_or(0, |h| h.0)),
                    };
                }
                Chat::Chat { id, .. } if id.0 == raw_id => {
                    return InputPeer::Chat {
                        chat_id: types::ChatId(id.0),
                    };
                }
                _ => {}
            }
        }
        InputPeer::InputPeerEmpty
    }

    /// Creates a new broadcast channel owned by the signed-in account and
    /// returns it as a storage target.
    pub async fn create_channel(&self, title: &str, about: &str) -> Result<ChannelInfo, String> {
        let client = self.ensure_connected().await?;
        // The wrapper also persists the fresh channel's access hash, which
        // is exactly what later "-100…" id resolution needs.
        let chats = client
            .lock()
            .await
            .create_channel(title, about, true, false)
            .await
            .map_err(|e| friendly(format!("creating channel failed: {e}")))?;

        for chat in chats {
            let Chat::Channel {
                id,
                title: ch_title,
                ..
            } = chat
            else {
                continue;
            };
            tracing::info!("created storage channel `{ch_title}`");
            return Ok(ChannelInfo {
                chat: format!("-100{}", id.0),
                title: ch_title,
            });
        }
        Err("channel was created but its identifier could not be read".to_string())
    }
}
