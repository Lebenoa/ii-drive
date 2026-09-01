use std::collections::HashMap;
use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::pool::SenderPool;
use mtprsto::types::{InputPeer, Message, MsgId};
use tokio::sync::Mutex;

// Embedded-store handle shared by the hub and every manager. Private
// here: only this module's children touch it.
type Db = surrealdb::Surreal<surrealdb::engine::local::Db>;

mod botfather;
mod bots;
mod channels;
mod hub;
mod login;
mod session;
mod transfer;

pub use bots::bot_token_regex;
pub use hub::{LoginStep, TgHub};

/// Resolved storage-chat handle. Under grammers this was a session-layer
/// `PeerRef`; with mtprsto it is the input peer itself (it carries the
/// access hash every storage RPC needs).
pub type PeerRef = InputPeer;

/// Global round-robin counter, shared by upload-target selection and bot
/// rotation so both spread evenly.
static ROTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Stable copy for auth-dead errors; the API layer maps this exact string
/// to HTTP 401 so clients can react structurally, not by matching prose.
pub const SESSION_INVALID_MSG: &str = "Telegram session expired or was revoked — sign in again";

fn friendly(err: String) -> String {
    if mtprsto::error::is_auth_error_message(&err) {
        SESSION_INVALID_MSG.to_string()
    } else {
        err
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UserInfo {
    pub id: i64,
    pub name: String,
    pub username: Option<String>,
    /// The number this account signed in with, used for the operator gate.
    /// Never serialized: the web client has no use for it, and it is the one
    /// field here that identifies a person outside Telegram.
    #[serde(skip_serializing)]
    pub phone: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TgStatus {
    pub connected: bool,
    pub authorized: bool,
    pub user: Option<UserInfo>,
    pub error: Option<String>,
    /// True when the only way forward is a fresh Telegram sign-in
    /// (revoked/expired/unregistered session key).
    pub relogin: bool,
}

/// A chat offered as a storage target by the picker UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelInfo {
    /// Stable key resolvable by `storage_peer`: "me" or a bot-api chat id.
    pub chat: String,
    pub title: String,
}

/// Cheap snapshot of a bot's live pieces for rotation/invite loops.
struct BotHandle {
    client: Arc<Client>,
    username: String,
    /// Upload DC id, cached so workers can talk about it without locking
    /// the bot's client again.
    dc_id: i32,
    /// The bot's connection pool — mtprsto keeps several TCP connections
    /// inside one pool, which is where upload parallelism comes from now.
    pool: Arc<SenderPool>,
}

struct BotSession {
    conn: Conn,
    username: String,
    id: i64,
    access_hash: Option<i64>,
}

/// A live `MTProto` client over one account's session. The session is a
/// row in the embedded store, written atomically by mtprsto through the
/// `DbSessions` bridge — closing a connection is dropping the client.
struct Conn {
    client: Arc<Client>,
}

impl Conn {
    /// Releases the connection. mtprsto persists its session atomically
    /// and never keeps the file open, so there is nothing to wait for.
    // Stays async so call sites keep their `.await` shape whatever the
    // storage backend needs in the future.
    #[allow(clippy::unused_async)]
    async fn close(self) {
        let _ = self;
    }
}

struct State {
    conn: Option<Conn>,
    config_error: Option<String>,
    peers: HashMap<String, PeerRef>,
    me: Option<UserInfo>,
    bots: HashMap<i64, BotSession>,
}

/// Placeholder id for a manager whose account is not known yet — a login
/// still in flight. Real Telegram ids are always positive.
const UNKNOWN_USER: i64 = 0;

/// Everything one Telegram account can do. Each instance owns exactly one
/// session row, so several accounts can be served side by side.
pub struct TgManager {
    cfg: Config,
    /// Embedded store holding this account's session row.
    pub(super) db: Db,
    /// Session row key this account owns (e.g. `user-7` for a signed-in
    /// account, `pending-<login id>` while a sign-in is in flight).
    session_key: String,
    /// Account this manager serves, or [`UNKNOWN_USER`] while signing in.
    user_id: i64,
    st: Mutex<State>,
}

/// Sends one text message and returns its id. Thin wrapper over
/// [`Client::send_to_peer`] mapping errors into this crate's `String`
/// convention (auth-dead errors collapse to [`SESSION_INVALID_MSG`]).
pub async fn send_text(client: &Client, peer: &PeerRef, text: &str) -> Result<MsgId, String> {
    client
        .send_to_peer(peer, text)
        .await
        .map_err(|e| friendly(format!("sending message failed: {e}")))
}

/// The newest `limit` messages of a chat, newest first.
pub async fn last_messages(
    client: &Client,
    peer: &PeerRef,
    limit: i32,
) -> Result<Vec<Message>, String> {
    client
        .get_recent_messages(peer, limit)
        .await
        .map_err(|e| friendly(format!("reading history failed: {e}")))
}

/// Fetches messages by id. Channel peers are routed through
/// `channels.getMessages` inside mtprsto; the plain method answers
/// `CHANNEL_INVALID` on channel peers.
pub async fn get_messages_by_id(
    client: &Client,
    peer: &PeerRef,
    ids: &[MsgId],
) -> Result<Vec<Message>, String> {
    client
        .get_messages(peer, ids)
        .await
        .map_err(|e| friendly(format!("fetch message: {e}")))
}

use crate::config::Config;
