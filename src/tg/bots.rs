use grammers_client::Client;
use grammers_client::tl;

use super::{BotHandle, BotSession, PeerAuth, PeerId, PeerRef, TgManager, friendly};

impl TgManager {
    /// Deterministic per bot and per account: the owning account's session
    /// file, plus the numeric prefix of the token.
    fn bot_session_path(&self, token: &str) -> String {
        let key = token.split(':').next().unwrap_or("bot");
        let base = self.session_path().trim_end_matches(".db");
        format!("{base}_bot_{key}.db")
    }

    /// Signs a bot in (or restores its persisted session) and adds it to
    /// the download pool.
    pub async fn configure_bot(&self, token: &str) -> Result<(String, i64), String> {
        if !self.cfg.tg_configured() {
            return Err(
                "Telegram is not configured: set api_id and api_hash in config.toml".to_string(),
            );
        }
        let path = self.bot_session_path(token);
        let conn = self.open_conn(&path).await?;
        let user = conn
            .client
            .bot_sign_in(token, &self.cfg.api_hash)
            .await
            .map_err(|e| friendly(format!("bot sign-in failed: {e}")))?;
        let (id, access_hash, username) = match &user.raw {
            tl::enums::User::User(u) => (
                u.id,
                u.access_hash,
                u.username.clone().unwrap_or_else(|| format!("bot{}", u.id)),
            ),
            tl::enums::User::Empty(_) => {
                return Err("bot account unavailable".to_string());
            }
        };
        let replaced = self.st.lock().await.bots.insert(
            id,
            BotSession {
                conn,
                username: username.clone(),
                id,
                access_hash,
            },
        );
        if let Some(previous) = replaced {
            previous.conn.close().await;
        }
        tracing::info!(%id, %username, "bot added to download pool");
        Ok((username, id))
    }

    pub async fn drop_bot(&self, id: i64) {
        let dropped = self.st.lock().await.bots.remove(&id);
        if let Some(bot) = dropped {
            // Stop its runner too, or the bot's session file stays open for
            // the rest of the process' life.
            bot.conn.close().await;
        }
    }

    /// Snapshot of the pool for the settings UI (no tokens).
    #[allow(clippy::significant_drop_tightening)] // `st` guard lives only to snapshot bots; collected before any await
    pub async fn bot_list(&self) -> Vec<(i64, String)> {
        let st = self.st.lock().await;
        let mut v: Vec<(i64, String)> = st
            .bots
            .values()
            .map(|b| (b.id, b.username.clone()))
            .collect();
        v.sort();
        v
    }

    /// Picks a client for channel traffic in either direction — uploads and
    /// downloads alike: rotating through the bot pool for channel-stored
    /// files, falling back to the user session (also used for Saved
    /// Messages, which bots cannot read). Every bot is a session of its
    /// own, so concurrent transfers each get a separate `MTProto` connection
    /// with separate rate limits instead of queueing on one.
    #[allow(clippy::arithmetic_side_effects)] // `bots > 0` is guarded before the modulo, so it cannot divide by zero
    #[allow(clippy::significant_drop_tightening)] // `st` is block-scoped to the snapshot; no cross-await hold
    pub async fn pool_target(
        &self,
        chat: &str,
    ) -> Result<(Client, PeerRef, Option<String>), String> {
        let key = chat.trim();
        if let Ok(n) = key.parse::<i64>() {
            let bots = self.st.lock().await.bots.len();
            if bots > 0 {
                let skip = self.next_rotation() % bots;
                let sessions: Vec<BotHandle> = {
                    let st = self.st.lock().await;
                    let mut v: Vec<BotHandle> = st
                        .bots
                        .values()
                        .map(|b| BotHandle {
                            client: b.conn.client.clone(),
                            username: b.username.clone(),
                        })
                        .collect();
                    v.sort_by_key(|b| b.username.clone());
                    v
                };
                let mut last_err = String::new();
                for bs in sessions.iter().cycle().skip(skip).take(bots) {
                    let Some(pid) = PeerId::from_bot_api_dialog_id(n) else { break };
                    let pref = PeerRef {
                        id: pid,
                        auth: PeerAuth::default(),
                    };
                    match bs.client.resolve_peer(pref).await {
                        Ok(peer) => {
                            let pref = peer
                                .to_ref()
                                .await
                                .map_err(|e| format!("peer ref failed: {e}"))?
                                .ok_or_else(|| format!("chat `{key}` has no usable peer ref"))?;
                            tracing::info!(bot = %bs.username, chat = %key, "transfer via bot");
                            return Ok((bs.client.clone(), pref, Some(bs.username.clone())));
                        }
                        Err(e) => {
                            last_err = format!("bot {}: {e}", bs.username);
                        }
                    }
                }
                tracing::warn!("all bots failed for `{chat}` ({last_err}); using user session");
            }
        }
        let client = self.ensure().await?;
        let peer = self.storage_peer(chat).await?;
        Ok((client, peer, None))
    }

    /// Invites every configured bot into the given storage chat and
    /// promotes it to admin, so downloads work through the pool.
    #[allow(clippy::too_many_lines)] // a single multi-step async orchestration; splitting adds indirection for no clarity gain
    pub async fn add_bots_to_chat(&self, chat: &str) -> Vec<(String, Result<(), String>)> {
        // One snapshot pass pairs each bot's live client with its stored
        // identity, so the whole flow below sees a consistent pool.
        let bots: Vec<(BotHandle, Option<tl::enums::InputUser>)> = {
            let st = self.st.lock().await;
            st.bots
                .values()
                .map(|b| {
                    (
                        BotHandle {
                            client: b.conn.client.clone(),
                            username: b.username.clone(),
                        },
                        b.access_hash.map(|h| {
                            tl::enums::InputUser::User(tl::types::InputUser {
                                user_id: b.id,
                                access_hash: h,
                            })
                        }),
                    )
                })
                .collect()
        };
        if bots.is_empty() {
            return Vec::new();
        }

        let Ok(peer_ref) = self.storage_peer(chat).await else {
            return bots
                .into_iter()
                .map(|(b, _)| (b.username, Err(format!("cannot resolve `{chat}`"))))
                .collect();
        };
        let channel_id = peer_ref.id.bare_id_unchecked();
        let access_hash = peer_ref.auth.hash();
        let input_channel = tl::enums::InputChannel::Channel(tl::types::InputChannel {
            channel_id,
            access_hash,
        });

        // The bot's InputUser must be valid from THIS account's view. The
        // hash captured during the bot's own sign-in often is not (it can
        // even be 0), which makes channels.inviteToChannel answer
        // USER_ID_INVALID — so resolve each bot by username here, through
        // the user client, right before inviting.
        let st = self.st.lock().await;
        let user_client = st.conn.as_ref().map(|c| c.client.clone());
        drop(st);

        let Some(user_client) = user_client else {
            return bots
                .into_iter()
                .map(|(b, _)| (b.username, Err("user session not connected".into())))
                .collect();
        };

        let mut results = Vec::new();
        for (bot, stored_user) in bots {
            let username = bot.username.clone();
            let res = (|| async {
                let input_user = match user_client.resolve_username(&username).await {
                    Ok(Some(peer)) => match peer {
                        grammers_client::peer::Peer::User(wrapped) => match wrapped.raw {
                            tl::enums::User::User(u) => {
                                tl::enums::InputUser::User(tl::types::InputUser {
                                    user_id: u.id,
                                    access_hash: u.access_hash.unwrap_or(0),
                                })
                            }
                            tl::enums::User::Empty(_) => stored_user.clone().ok_or_else(|| {
                                format!("bot @{username} resolved to an empty account")
                            })?,
                        },
                        _ => stored_user
                            .clone()
                            .ok_or_else(|| format!("@{username} is not a user account"))?,
                    },
                    Ok(None) => stored_user.clone().ok_or_else(|| {
                        format!(
                            "bot @{username} not found — open a chat with it once in Telegram first"
                        )
                    })?,
                    Err(e) => return Err(friendly(format!("resolve failed: {e}"))),
                };
                match user_client
                    .invoke(&tl::functions::channels::InviteToChannel {
                        channel: input_channel.clone(),
                        users: vec![input_user.clone()],
                    })
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        let s = e.to_string();
                        // Already a member / privacy limits: admin promotion
                        // below is what actually matters for downloads.
                        // USER_BOT: bots cannot join channels as plain
                        // members at all — the EditAdmin below adds and
                        // promotes them in one step, which is all we need.
                        if s.contains("USER_ALREADY_PARTICIPANT")
                            || s.contains("USER_NOT_MUTUAL_CONTACT")
                            || s.contains("USER_CHANNELS_TOO_MUCH")
                            || s.contains("USER_BOT")
                        {
                            Ok(())
                        } else {
                            Err(friendly(format!("invite failed: {s}")))
                        }
                    }
                }?;
                user_client
                    .invoke(&tl::functions::channels::EditAdmin {
                        channel: input_channel.clone(),
                        user_id: input_user,
                        admin_rights: tl::enums::ChatAdminRights::Rights(
                            tl::types::ChatAdminRights {
                                change_info: false,
                                post_messages: true,
                                edit_messages: false,
                                delete_messages: false,
                                ban_users: false,
                                invite_users: true,
                                pin_messages: false,
                                add_admins: false,
                                anonymous: false,
                                manage_call: false,
                                other: false,
                                manage_topics: false,
                                post_stories: false,
                                edit_stories: false,
                                delete_stories: false,
                                manage_direct_messages: false,
                                manage_ranks: false,
                            },
                        ),
                        rank: Some("storage".to_string()),
                    })
                    .await
                    .map_err(|e| friendly(format!("promoting failed: {e}")))?;

                // Downloads resolve this channel from the BOT's side by id
                // alone, which Telegram refuses (CHANNEL_INVALID) unless the
                // bot's own session already holds the channel. Invite and
                // promote happen from the user account, so a fresh channel
                // never reaches the bot's cache on its own — make the bot
                // index it once now, while the membership is brand new.
                bot.client
                    .invoke(&tl::functions::channels::GetChannels {
                        id: vec![input_channel.clone()],
                    })
                    .await
                    .map_err(|e| {
                        format!("wired, but @{username} could not index the channel: {e}")
                    })?;
                Ok(())
            })()
            .await;
            results.push((username, res));
        }
        results
    }
}

/// Bot API tokens look like `123456789:AA...` (digits, colon, ~35 chars).
/// Compiled once; `None` only if the pattern itself were invalid, which
pub fn bot_token_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::LazyLock<Option<regex::Regex>> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\d{6,12}:[A-Za-z0-9_-]{30,}").ok());
    RE.as_ref()
}
