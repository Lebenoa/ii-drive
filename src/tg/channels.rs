use std::collections::HashSet;

use grammers_client::tl;
use grammers_client::Client;

use super::{ChannelInfo, PeerAuth, PeerId, PeerRef, TgManager, friendly};

impl TgManager {
    /// Resolves (and caches) a chat key — "me", "@username" or "-100<id>" —
    /// to a peer reference.
    pub async fn storage_peer(&self, chat: &str) -> Result<PeerRef, String> {
        let key = chat.trim();
        let cache_key = key.to_ascii_lowercase();

        {
            let st = self.st.lock().await;
            if let Some(peer) = st.peers.get(&cache_key) {
                return Ok(*peer);
            }
        }

        let client = self.ensure().await?;
        let peer_ref = if key.is_empty()
            || key.eq_ignore_ascii_case("me")
            || key.eq_ignore_ascii_case("self")
        {
            client
                .get_me()
                .await
                .map_err(|e| friendly(format!("cannot resolve own peer: {e}")))?
                .to_ref()
                .await
                .map_err(|e| friendly(format!("cannot resolve own peer: {e}")))?
                .ok_or("cannot resolve own peer")?
        } else if let Ok(n) = key.parse::<i64>() {
            let pid = PeerId::from_bot_api_dialog_id(n)
                .ok_or_else(|| format!("chat id `{key}` is not a valid chat"))?;
            let pref = PeerRef {
                id: pid,
                auth: PeerAuth::default(),
            };
            let peer = client
                .resolve_peer(pref)
                .await
                .map_err(|e| friendly(format!("cannot resolve chat {key}: {e}")))?;
            peer.to_ref()
                .await
                .map_err(|e| friendly(format!("cannot resolve chat {key}: {e}")))?
                .ok_or_else(|| format!("chat `{key}` has no usable peer reference"))?
        } else {
            let name = key.trim_start_matches('@');
            let peer = client
                .resolve_username(name)
                .await
                .map_err(|e| friendly(format!("resolve failed: {e}")))?
                .ok_or_else(|| format!("storage chat `{key}` not found or not accessible"))?;
            peer.to_ref()
                .await
                .map_err(|e| format!("storage chat `{key}` peer lookup failed: {e}"))?
                .ok_or_else(|| format!("storage chat `{key}` has no usable peer reference"))?
        };

        self.st
            .lock()
            .await
            .peers
            .insert(cache_key, peer_ref);
        Ok(peer_ref)
    }

    /// Channels and groups this account could use as storage targets:
    /// both the main dialog list AND the archived folder (storage channels
    /// are typically muted and archived, so they only show up there).
    /// "Saved Messages" is always first.
    pub async fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let client = self.ensure().await?;
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
        client: &Client,
        folder_id: Option<i32>,
        out: &mut Vec<ChannelInfo>,
        seen: &mut HashSet<String>,
    ) -> Result<(), String> {
        let mut offset_date: i32 = 0;
        let mut offset_id: i32 = 0;
        let mut offset_peer = tl::enums::InputPeer::Empty;

        for _page in 0..20 {
            let resp = client
                .invoke(&tl::functions::messages::GetDialogs {
                    exclude_pinned: false,
                    folder_id,
                    offset_date,
                    offset_id,
                    offset_peer: offset_peer.clone(),
                    limit: 100,
                    hash: 0,
                })
                .await
                .map_err(|e| friendly(format!("listing dialogs failed: {e}")))?;

            let (dialogs, messages, chats) = match resp {
                tl::enums::messages::Dialogs::Dialogs(d) => (d.dialogs, d.messages, d.chats),
                tl::enums::messages::Dialogs::Slice(s) => (s.dialogs, s.messages, s.chats),
                tl::enums::messages::Dialogs::NotModified(_) => return Ok(()),
            };

            Self::harvest_chats(&chats, out, seen);
            if dialogs.len() < 100 {
                return Ok(());
            }

            // Advance the cursor to the last dialog of this page. The offset
            // date comes from that dialog's top message (the Dialog struct no
            // longer carries its own date).
            let Some(tl::enums::Dialog::Dialog(dd)) = dialogs.last() else {
                return Ok(());
            };
            let raw_id = match &dd.peer {
                tl::enums::Peer::Channel(p) => p.channel_id,
                tl::enums::Peer::Chat(p) => p.chat_id,
                tl::enums::Peer::User(p) => p.user_id,
            };
            let mut msg_date = 0i32;
            for m in &messages {
                if let tl::enums::Message::Message(mm) = m {
                    if mm.id != dd.top_message {
                        continue;
                    }
                    let mid = match &mm.peer_id {
                        tl::enums::Peer::Channel(p) => p.channel_id,
                        tl::enums::Peer::Chat(p) => p.chat_id,
                        tl::enums::Peer::User(p) => p.user_id,
                    };
                    if mid == raw_id {
                        msg_date = mm.date;
                        break;
                    }
                }
            }
            offset_date = msg_date;
            offset_id = dd.top_message;
            offset_peer = Self::input_peer_for(raw_id, &chats);
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
    fn harvest_chats(
        chats: &[tl::enums::Chat],
        out: &mut Vec<ChannelInfo>,
        seen: &mut HashSet<String>,
    ) {
        for c in chats {
            let tl::enums::Chat::Channel(ch) = c else {
                continue;
            };
            if ch.left && !ch.creator {
                continue; // we left it — cannot post
            }
            if !ch.creator && !Self::can_wire_bots(ch.admin_rights.as_ref()) {
                // Read-only for us — bots cannot be wired in.
                tracing::debug!(
                    title = %ch.title,
                    megagroup = ch.megagroup,
                    admin = ch.admin_rights.is_some(),
                    "skipping chat: no rights to wire download bots"
                );
                continue;
            }
            let key = format!("-100{}", ch.id);
            if seen.insert(key.clone()) {
                out.push(ChannelInfo {
                    chat: key,
                    title: ch.title.clone(),
                });
            }
        }
    }

    /// Whether an admin-rights grant covers inviting a download bot and
    /// promoting it to admin. Both are required: an un-promoted bot cannot read
    /// the channel's files back.
    fn can_wire_bots(rights: Option<&tl::enums::ChatAdminRights>) -> bool {
        let Some(tl::enums::ChatAdminRights::Rights(r)) = rights else {
            return false;
        };
        r.invite_users && r.add_admins
    }

    /// Builds an InputPeer for pagination offsets from the same response's
    /// chats array (which carries the access hashes).
    fn input_peer_for(raw_id: i64, chats: &[tl::enums::Chat]) -> tl::enums::InputPeer {
        for c in chats {
            match c {
                tl::enums::Chat::Channel(ch) if ch.id == raw_id => {
                    return tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                        channel_id: ch.id,
                        access_hash: ch.access_hash.unwrap_or(0),
                    });
                }
                tl::enums::Chat::Chat(g) if g.id == raw_id => {
                    return tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: g.id });
                }
                _ => {}
            }
        }
        tl::enums::InputPeer::Empty
    }

    /// Creates a new broadcast channel owned by the signed-in account and
    /// returns it as a storage target.
    pub async fn create_channel(&self, title: &str, about: &str) -> Result<ChannelInfo, String> {
        use grammers_client::tl;

        let client = self.ensure().await?;
        let updates = client
            .invoke(&tl::functions::channels::CreateChannel {
                broadcast: true,
                megagroup: false,
                for_import: false,
                forum: false,
                title: title.to_string(),
                about: about.to_string(),
                geo_point: None,
                address: None,
                ttl_period: None,
            })
            .await
            .map_err(|e| friendly(format!("creating channel failed: {e}")))?;

        // The new channel comes back inside the updates' chat list; convert
        // its raw id into the bot-api form ("-100…") used as our chat key.
        let chats = match updates {
            tl::enums::Updates::Updates(u) => u.chats,
            tl::enums::Updates::Combined(u) => u.chats,
            _ => Vec::new(),
        };
        for chat in chats {
            if let tl::enums::Chat::Channel(c) = chat {
                tracing::info!("created storage channel `{}`", c.title);
                return Ok(ChannelInfo {
                    chat: format!("-100{}", c.id),
                    title: c.title,
                });
            }
        }
        Err("channel was created but its identifier could not be read".to_string())
    }
}
