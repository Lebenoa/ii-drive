use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::pool::SenderPool;
use mtprsto::types::{self, InputChannel, InputPeer, InputUser};
use tokio::sync::Mutex;

use super::{BotHandle, BotSession, PeerRef, TgManager, friendly};

impl TgManager {
    /// Deterministic per bot and per account: the owning account's
    /// session row key, plus the numeric prefix of the token.
    fn bot_session_key(&self, token: &str) -> String {
        let key = token.split(':').next().unwrap_or("bot");
        format!("{}-bot-{key}", self.session_key())
    }

    /// Signs a bot in (or restores its persisted session) and adds it to
    /// the download pool.
    pub async fn configure_bot(&self, token: &str) -> Result<(String, i64), String> {
        if !self.cfg.tg_configured() {
            return Err(
                "Telegram is not configured: set api_id and api_hash in config.toml".to_string(),
            );
        }
        let key = self.bot_session_key(token);
        let conn = self.open_conn(&key, crate::db::SessionKind::Bot).await?;
        {
            let mut c = conn.client.lock().await;
            c.connect()
                .await
                .map_err(|e| friendly(format!("bot connection failed: {e}")))?;
            // mtprsto handles bot home-DC migration inside authorize_bot
            // and persists the bot's user id, so restarts skip re-auth.
            c.authorize_bot(token)
                .await
                .map_err(|e| friendly(format!("bot sign-in failed: {e}")))?;
        }
        let (id, access_hash, username) = {
            let c = conn.client.lock().await;
            match c
                .get_me()
                .await
                .map_err(|e| friendly(format!("bot profile failed: {e}")))?
            {
                types::User::User {
                    id,
                    access_hash,
                    username,
                    ..
                } => (
                    id.0,
                    access_hash.map(|h| h.0),
                    username.unwrap_or_else(|| format!("bot{}", id.0)),
                ),
                types::User::Empty { .. } => return Err("bot account unavailable".to_string()),
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
    /// own, so concurrent transfers each get a separate connection with
    /// separate rate limits instead of queueing on the owner session.
    ///
    /// Returns (client, peer, bot_name, dc_id, pool). The pool is bound to
    /// the same session as the client, so file ids uploaded through it are
    /// valid for the follow-up send, whichever target was picked.
    pub async fn pool_target(
        &self,
        chat: &str,
    ) -> Result<
        (
            Arc<Mutex<Client>>,
            PeerRef,
            Option<String>,
            i32,
            Arc<SenderPool>,
        ),
        String,
    > {
        let key = chat.trim();
        if key.parse::<i64>().is_ok() {
            let bots = self.st.lock().await.bots.len();
            if bots > 0 {
                let skip = self.next_rotation() % bots;
                // Snapshot everything we need from the bot sessions
                // (including their connection pieces, which need a brief
                // lock each), then rotate without holding any locks.
                let snapshots: Vec<(Arc<Mutex<Client>>, String)> = {
                    let st = self.st.lock().await;
                    st.bots
                        .values()
                        .map(|b| (b.conn.client.clone(), b.username.clone()))
                        .collect()
                };
                let mut sessions = Vec::with_capacity(snapshots.len());
                for (client, username) in snapshots {
                    let (dc_id, pool) = {
                        let c = client.lock().await;
                        if !c.is_connected() {
                            continue;
                        }
                        (c.dc_id(), c.pool())
                    };
                    sessions.push(BotHandle {
                        client,
                        username,
                        dc_id,
                        pool,
                    });
                }
                if sessions.is_empty() {
                    tracing::warn!("all bot sessions are disconnected; using user session");
                } else {
                    sessions.sort_by(|a, b| a.username.cmp(&b.username));
                    let count = sessions.len();
                    let mut last_err = String::new();
                    for bs in sessions.iter().cycle().skip(skip).take(count) {
                        // Only ids the bot can actually see resolve: wiring
                        // makes each bot index the channel, and its session
                        // file keeps the access hash for later boots.
                        let mut c = bs.client.lock().await;
                        match c.resolve_peer(key).await {
                            Ok(peer) => {
                                tracing::info!(
                                    bot = %bs.username,
                                    chat = %key,
                                    "transfer via bot"
                                );
                                return Ok((
                                    bs.client.clone(),
                                    peer,
                                    Some(bs.username.clone()),
                                    bs.dc_id,
                                    bs.pool.clone(),
                                ));
                            }
                            Err(e) => {
                                last_err = format!("bot {}: {e}", bs.username);
                            }
                        }
                    }
                    tracing::warn!("all bots failed for `{chat}` ({last_err}); using user session");
                }
            }
        }
        let client = self.ensure_connected().await?;
        let peer = self.storage_peer(chat).await?;
        let (dc_id, pool) = {
            let c = client.lock().await;
            (c.dc_id(), c.pool())
        };
        Ok((client, peer, None, dc_id, pool))
    }

    /// Invites every configured bot into the given storage chat and
    /// promotes it to admin, so downloads work through the pool.
    #[allow(clippy::too_many_lines)] // a single multi-step async orchestration; splitting adds indirection for no clarity gain
    pub async fn add_bots_to_chat(&self, chat: &str) -> Vec<(String, Result<(), String>)> {
        // A bot's stored identity: client handle, username, account id and
        // the access hash captured at its own sign-in (often unusable, but
        // the last resort when username resolution fails).
        type BotSnapshot = (Arc<Mutex<Client>>, String, i64, Option<i64>);
        let snapshots: Vec<BotSnapshot> = {
            let st = self.st.lock().await;
            st.bots
                .values()
                .map(|b| {
                    (
                        b.conn.client.clone(),
                        b.username.clone(),
                        b.id,
                        b.access_hash,
                    )
                })
                .collect()
        };
        let mut bots: Vec<(BotHandle, Option<InputUser>)> = Vec::with_capacity(snapshots.len());
        for (client, username, id, hash) in snapshots {
            let (dc_id, pool) = {
                let c = client.lock().await;
                if !c.is_connected() {
                    continue;
                }
                (c.dc_id(), c.pool())
            };
            bots.push((
                BotHandle {
                    client,
                    username,
                    dc_id,
                    pool,
                },
                hash.map(|h| InputUser::User {
                    user_id: types::UserId(id),
                    access_hash: types::AccessHash(h),
                }),
            ));
        }
        if bots.is_empty() {
            return Vec::new();
        }

        let Ok(peer) = self.storage_peer(chat).await else {
            return bots
                .into_iter()
                .map(|(b, _)| (b.username, Err(format!("cannot resolve `{chat}`"))))
                .collect();
        };
        let InputPeer::Channel {
            channel_id,
            access_hash,
        } = peer
        else {
            return bots
                .into_iter()
                .map(|(b, _)| {
                    (b.username, Err(format!("`{chat}` is not a channel; bots cannot be wired into it")))
                })
                .collect();
        };
        let input_channel = InputChannel::Channel {
            channel_id,
            access_hash,
        };

        // The bot's InputUser must be valid from THIS account's view. The
        // hash captured during the bot's own sign-in often is not (it can
        // even be 0), which makes channels.inviteToChannel answer
        // USER_ID_INVALID — so resolve each bot by username here, through
        // the user client, right before inviting.
        let user_client = {
            let st = self.st.lock().await;
            st.conn.as_ref().map(|c| c.client.clone())
        };

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
                let input_user = {
                    let mut uc = user_client.lock().await;
                    match uc.resolve_username(&username).await {
                        Ok(InputPeer::User {
                            user_id,
                            access_hash,
                        }) => InputUser::User {
                            user_id,
                            access_hash,
                        },
                        Ok(other) => {
                            let _ = other;
                            stored_user.clone().ok_or_else(|| {
                                format!("@{username} resolved to a non-user account")
                            })?
                        }
                        // Unresolvable usually means the account never talked
                        // to the bot; the stored hash is the last resort.
                        Err(e) => stored_user.clone().ok_or_else(|| {
                            format!("bot @{username} not found — open a chat with it once in Telegram first ({e})")
                        })?,
                    }
                };
                match user_client
                    .lock()
                    .await
                    .invite_to_channel(&input_channel, std::slice::from_ref(&input_user))
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
                // post_messages (flags.1) + invite_users (flags.5): enough
                // for the bot to post uploads and stay invitable.
                user_client
                    .lock()
                    .await
                    .edit_admin(&input_channel, &input_user, (1 << 1) | (1 << 5), "storage")
                    .await
                    .map_err(|e| friendly(format!("promoting failed: {e}")))?;

                // Downloads resolve this channel from the BOT's side by id
                // alone, which Telegram refuses (CHANNEL_INVALID) unless the
                // bot's own session already holds the channel. Invite and
                // promote happen from the user account, so a fresh channel
                // never reaches the bot's cache on its own — make the bot
                // index it once now, while the membership is brand new.
                bot.client
                    .lock()
                    .await
                    .get_channels(std::slice::from_ref(&input_channel))
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
