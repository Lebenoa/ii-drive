use std::collections::HashMap;
use std::sync::Arc;

use mtprsto::client::Client;
use mtprsto::error::Error as TgError;
use mtprsto::pool::SenderPool;
use mtprsto::rpc;
use mtprsto::serialize::{TLReader, TLWriter};
use mtprsto::types::{self, Document, InputPeer, Message, MsgId, PhotoSize, Updates, User};
use tokio::sync::Mutex;

mod botfather;
mod bots;
mod channels;
mod hub;
mod login;
mod session;
mod transfer;

pub use bots::bot_token_regex;
pub use hub::{LoginStep, TgHub};
pub use transfer::is_file_reference_error;

/// Resolved storage-chat handle. Under grammers this was a session-layer
/// `PeerRef`; with mtprsto it is the input peer itself (it carries the
/// access hash every storage RPC needs).
pub type PeerRef = InputPeer;

/// Global round-robin counter, shared by upload-target selection and bot
/// rotation so both spread evenly.
static ROTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// RPC error markers that mean the session's auth key is dead.
const AUTH_ERROR_MARKERS: [&str; 5] = [
    "AUTH_KEY_UNREGISTERED",
    "AUTH_KEY_INVALID",
    "SESSION_EXPIRED",
    "SESSION_REVOKED",
    "USER_DEACTIVATED",
];

/// File-reference errors that a refetch of the owning message fixes.
const FILE_REFERENCE_MARKERS: [&str; 2] =
    ["FILE_REFERENCE_EXPIRED", "FILE_REFERENCE_INVALID"];

/// `channels.getMessages#e5906e3f channel:InputChannel id:Vector<int>` —
/// not among mtprsto's builders (it ships `messages.getMessages` only),
/// so the payload is assembled here. The constructor is stable across
/// layers.
const CHANNELS_GET_MESSAGES: u32 = 0xe5906e3f;
/// `channels.deleteMessages#84c1f4e6 channel:InputChannel id:Vector<int>`
/// — same story; `messages.deleteMessages` is refused on channel peers.
const CHANNELS_DELETE_MESSAGES: u32 = 0x84c1f4e6;

/// True when an RPC failure means the session's auth key is not bound to a
/// logged-in user (stale/partial login, revoked or expired session).
fn is_auth_error(err: &TgError) -> bool {
    match err {
        TgError::AuthKeyInvalid { .. } | TgError::AuthKeyUnregistered { .. } => true,
        other => is_auth_error_str(&other.to_string()),
    }
}

/// String-level twin of [`is_auth_error`] for errors that already left the
/// typed world (connection setup, wrapped messages).
fn is_auth_error_str(msg: &str) -> bool {
    AUTH_ERROR_MARKERS.iter().any(|m| msg.contains(m))
}

/// String check for file-reference errors surfaced as plain messages.
pub(crate) fn is_file_reference_str(msg: &str) -> bool {
    FILE_REFERENCE_MARKERS.iter().any(|m| msg.contains(m))
}

/// Stable copy for auth-dead errors; the API layer maps this exact string
/// to HTTP 401 so clients can react structurally, not by matching prose.
pub const SESSION_INVALID_MSG: &str = "Telegram session expired or was revoked — sign in again";

fn friendly(err: String) -> String {
    if is_auth_error_str(&err) {
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
    client: Arc<Mutex<Client>>,
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

/// A live `MTProto` client over one account's session file. The session
/// (a JSON file) is only touched atomically on save, so unlike the
/// grammers runner nothing here holds the file open — closing a
/// connection is dropping the client.
struct Conn {
    client: Arc<Mutex<Client>>,
}

impl Conn {
    /// Releases the connection. mtprsto persists its session atomically
    /// and never keeps the file open, so there is nothing to wait for.
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
/// session file, so several accounts can be served side by side.
pub struct TgManager {
    cfg: Config,
    /// Session file backing this account only.
    session_path: String,
    /// Account this manager serves, or [`UNKNOWN_USER`] while signing in.
    user_id: i64,
    st: Mutex<State>,
}

fn user_info(u: &User) -> UserInfo {
    UserInfo {
        id: u.id().0,
        name: full_name(u),
        username: u.username().map(ToString::to_string),
        // Always present for one's own account, which is the only user
        // this is ever called for.
        phone: u.phone().map(ToString::to_string),
    }
}

/// grammers' `full_name`: first and last name joined by a space, either
/// half optional. mtprsto exposes no accessor for the last name, so the
/// generated field is matched directly.
fn full_name(u: &User) -> String {
    let last = match u {
        User::User { last_name, .. } => last_name.as_deref().unwrap_or(""),
        User::Empty { .. } => "",
    };
    match (u.first_name().unwrap_or(""), last) {
        ("", "") => String::new(),
        (first, "") => first.to_string(),
        ("", last) => last.to_string(),
        (first, last) => format!("{first} {last}"),
    }
}

async fn get_me_info(client: &Client) -> Option<UserInfo> {
    match client.get_me().await {
        Ok(u) => Some(user_info(&u)),
        Err(e) => {
            tracing::warn!("get_me failed: {e}");
            None
        }
    }
}

/// Sends one text message and returns its id.
pub(crate) async fn send_text(
    client: &Client,
    peer: &PeerRef,
    text: &str,
) -> Result<MsgId, String> {
    let raw = client
        .invoke_raw(rpc::build_send_message(peer, text, None, None))
        .await
        .map_err(|e| friendly(format!("sending message failed: {e}")))?;
    let updates =
        Updates::parse(&raw).map_err(|e| format!("send response unreadable: {e}"))?;
    updates
        .message_id()
        .ok_or_else(|| "server returned no message id".to_string())
}

/// The newest `limit` messages of a chat, newest first. `messages.getHistory`
/// accepts user chats and channels alike.
pub(crate) async fn last_messages(
    client: &Client,
    peer: &PeerRef,
    limit: i32,
) -> Result<Vec<Message>, String> {
    let raw = client
        .invoke_raw(rpc::build_get_history(peer, 0, 0, 0, limit, 0, 0))
        .await
        .map_err(|e| friendly(format!("reading history failed: {e}")))?;
    parse_messages_container(&raw)
}

/// Fetches messages by id. Channel peers must go through
/// `channels.getMessages`; the plain method answers `CHANNEL_INVALID`.
pub(crate) async fn get_messages_by_id(
    client: &Client,
    peer: &PeerRef,
    ids: &[MsgId],
) -> Result<Vec<Message>, String> {
    let payload = match peer {
        InputPeer::Channel { .. } => build_channels_get_messages(peer, ids),
        _ => rpc::build_get_messages(ids),
    };
    let raw = client
        .invoke_raw(payload)
        .await
        .map_err(|e| friendly(format!("fetch message: {e}")))?;
    parse_messages_container(&raw)
}

fn build_channels_get_messages(peer: &PeerRef, ids: &[MsgId]) -> Vec<u8> {
    let mut w = TLWriter::new();
    w.write_u32(CHANNELS_GET_MESSAGES);
    if let InputPeer::Channel {
        channel_id,
        access_hash,
    } = peer
    {
        w.write_u32(types::INPUT_CHANNEL);
        w.write_i64(channel_id.0);
        w.write_i64(access_hash.0);
    }
    w.write_u32(types::VECTOR);
    w.write_i32(ids.len() as i32);
    for id in ids {
        w.write_i32(id.0 as i32);
    }
    w.into_bytes()
}

/// `messages.getDialogs` with a folder — mtprsto's builder has no
/// `folder_id`, and storage channels typically live in the archive.
pub(super) fn build_get_dialogs(
    folder_id: Option<i32>,
    offset_date: i32,
    offset_id: i32,
    offset_peer: &PeerRef,
    limit: i32,
) -> Vec<u8> {
    // messages.getDialogs#a0f4cb4f flags:# exclude_pinned:flags.0?true
    //   folder_id:flags.1?int offset_date:int offset_id:int
    //   offset_peer:InputPeer limit:int hash:long
    let mut flags = 0i32;
    if folder_id.is_some() {
        flags |= 1 << 1;
    }
    let mut w = TLWriter::new();
    w.write_u32(types::MESSAGES_GET_DIALOGS);
    w.write_i32(flags);
    w.write_i32(offset_date);
    w.write_i32(offset_id);
    offset_peer.write_to(&mut w);
    w.write_i32(limit);
    w.write_i64(0); // hash:long — no hash check
    w.into_bytes()
}

/// Decodes any `messages.Messages*` answer.
fn parse_messages_container(data: &[u8]) -> Result<Vec<Message>, String> {
    let mut r = TLReader::new(data);
    let ctor = r.read_u32().map_err(|e| e.to_string())?;
    match ctor {
        types::MESSAGES_MESSAGES => read_messages_tail(&mut r),
        types::MESSAGES_MESSAGES_SLICE => {
            // messagesSlice#5f206716 flags:# inexact:flags.1?true count:int
            //   next_rate:flags.0?int offset_id_offset:flags.2?int
            //   search_flood:flags.3?SearchPostsFlood messages topics chats users
            let flags = r.read_i32().map_err(|e| e.to_string())?;
            let _count = r.read_i32().map_err(|e| e.to_string())?;
            if flags & (1 << 0) != 0 {
                let _ = r.read_i32().map_err(|e| e.to_string())?;
            }
            if flags & (1 << 2) != 0 {
                let _ = r.read_i32().map_err(|e| e.to_string())?;
            }
            if flags & (1 << 3) != 0 {
                return Err("messagesSlice carries search_flood — unsupported".into());
            }
            read_messages_tail(&mut r)
        }
        types::MESSAGES_CHANNEL_MESSAGES => {
            // channelMessages#c776ba4e flags:# inexact:flags.1?true pts:int
            //   count:int offset_id_offset:flags.2?int messages topics chats users
            let flags = r.read_i32().map_err(|e| e.to_string())?;
            let _pts = r.read_i32().map_err(|e| e.to_string())?;
            let _count = r.read_i32().map_err(|e| e.to_string())?;
            if flags & (1 << 2) != 0 {
                let _ = r.read_i32().map_err(|e| e.to_string())?;
            }
            read_messages_tail(&mut r)
        }
        types::MESSAGES_MESSAGES_NOT_MODIFIED => Ok(Vec::new()),
        other => Err(format!("unexpected messages container {other:#x}")),
    }
}

/// Shared tail of `messages.Messages*`: messages, topics, chats, users.
fn read_messages_tail(r: &mut TLReader) -> Result<Vec<Message>, String> {
    let count = r.read_vector_header().map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(Message::read_from(r).map_err(|e| e.to_string())?);
    }
    // topics:Vector<ForumTopic> — must be consumed before chats/users.
    let topic_count = r.read_vector_header().map_err(|e| e.to_string())?;
    for _ in 0..topic_count {
        let tctor = r.read_u32().map_err(|e| e.to_string())?;
        if tctor != types::FORUM_TOPIC_DELETED {
            return Err(format!("unsupported forum topic {tctor:#x}"));
        }
        let _ = r.read_i32().map_err(|e| e.to_string())?;
    }
    types::read_chat_vector_public(r).map_err(|e| e.to_string())?;
    types::read_user_vector_public(r).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decodes a `messages.Dialogs`/`DialogsSlice` answer (mtprsto's own
/// parser is lossy for these, so the fields are walked here).
pub(super) fn parse_dialogs_response(data: &[u8]) -> Result<types::Dialogs, String> {
    let mut r = TLReader::new(data);
    let ctor = r.read_u32().map_err(|e| e.to_string())?;
    match ctor {
        types::MESSAGES_DIALOGS | types::MESSAGES_DIALOGS_SLICE => {
            // dialogs come FIRST, then messages, chats, users.
            if ctor == types::MESSAGES_DIALOGS_SLICE {
                let _count = r.read_i32().map_err(|e| e.to_string())?;
            }
            let n = r.read_vector_header().map_err(|e| e.to_string())?;
            let mut dialogs = Vec::with_capacity(n as usize);
            for _ in 0..n {
                dialogs.push(types::Dialog::read_from(&mut r).map_err(|e| e.to_string())?);
            }
            let n = r.read_vector_header().map_err(|e| e.to_string())?;
            let mut messages = Vec::with_capacity(n as usize);
            for _ in 0..n {
                messages.push(Message::read_from(&mut r).map_err(|e| e.to_string())?);
            }
            let chats = types::read_chat_vector_public(&mut r).map_err(|e| e.to_string())?;
            let users = types::read_user_vector_public(&mut r).map_err(|e| e.to_string())?;
            Ok(types::Dialogs {
                dialogs,
                messages,
                users,
                chats,
            })
        }
        types::MESSAGES_DIALOGS_NOT_MODIFIED => Ok(types::Dialogs {
            dialogs: Vec::new(),
            messages: Vec::new(),
            users: Vec::new(),
            chats: Vec::new(),
        }),
        other => Err(format!("unexpected getDialogs response {other:#x}")),
    }
}

/// Pulls the sent message (id + document) out of a send answer. A short
/// `updateShortSentMessage` carries the id but no media; the caller then
/// fetches the message once to read its thumbnail.
pub(crate) fn updates_message_and_id(u: &Updates) -> (Option<MsgId>, Option<Document>) {
    let empty: Vec<types::Update> = Vec::new();
    let items: &[types::Update] = match u {
        Updates::Updates { updates, .. } | Updates::UpdatesCombined { updates, .. } => updates,
        Updates::UpdateShort { update, .. } => std::slice::from_ref(update),
        Updates::UpdateShortSentMessage { .. } => &empty,
    };
    let mut id = u.message_id();
    let mut doc = None;
    for update in items {
        if let types::Update::NewMessage { message, .. }
        | types::Update::NewChannelMessage { message, .. } = update
            && let Message::Message(full) = message
            && let Some(types::MessageMedia::Document { document, .. }) = &full.media
        {
            doc = Some(document.clone());
            if id.is_none() {
                id = Some(full.id);
            }
        }
    }
    (id, doc)
}

/// The document behind a message, when it has one.
pub(crate) fn message_document(msg: &Message) -> Option<Document> {
    if let Message::Message(full) = msg
        && let Some(types::MessageMedia::Document { document, .. }) = &full.media
    {
        return Some(document.clone());
    }
    None
}

/// Telegram's stripped thumbnail of a document as displayable JPEG bytes.
pub(crate) fn doc_stripped_thumb(doc: &Document) -> Option<Vec<u8>> {
    if let Document::Document {
        thumbs: Some(thumbs),
        ..
    } = doc
    {
        for thumb in thumbs {
            if let PhotoSize::PhotoStrippedSize { bytes, .. } = thumb {
                let jpeg = stripped_thumb_jpeg(bytes);
                if !jpeg.is_empty() {
                    return Some(jpeg);
                }
            }
        }
    }
    None
}

/// Rebuilds a displayable JPEG from Telegram's stripped thumbnail bytes
/// <https://core.tlgr.org/api/files#stripped-thumbnails>. Same header,
/// dimensions from bytes[1..3], scan data from bytes[3..], JPEG EOI footer.
#[allow(clippy::indexing_slicing)] // guarded by the `len() < 4` return; out[] indices are within the fixed 623-byte HEADER
pub fn stripped_thumb_jpeg(bytes: &[u8]) -> Vec<u8> {
    const HEADER: [u8; 623] = [
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x28, 0x1c, 0x1e, 0x23, 0x1e,
        0x19, 0x28, 0x23, 0x21, 0x23, 0x2d, 0x2b, 0x28, 0x30, 0x3c, 0x64, 0x41, 0x3c, 0x37, 0x37,
        0x3c, 0x7b, 0x58, 0x5d, 0x49, 0x64, 0x91, 0x80, 0x99, 0x96, 0x8f, 0x80, 0x8c, 0x8a, 0xa0,
        0xb4, 0xe6, 0xc3, 0xa0, 0xaa, 0xda, 0xad, 0x8a, 0x8c, 0xc8, 0xff, 0xcb, 0xda, 0xee, 0xf5,
        0xff, 0xff, 0xff, 0x9b, 0xc1, 0xff, 0xff, 0xff, 0xfa, 0xff, 0xe6, 0xfd, 0xff, 0xf8, 0xff,
        0xdb, 0x00, 0x43, 0x01, 0x2b, 0x2d, 0x2d, 0x3c, 0x35, 0x3c, 0x76, 0x41, 0x41, 0x76, 0xf8,
        0xa5, 0x8c, 0xa5, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
        0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
        0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8,
        0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x00,
        0x00, 0x00, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00,
        0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08,
        0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a,
        0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
        0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6,
        0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
        0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7,
        0xf8, 0xf9, 0xfa, 0xff, 0xc4, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01, 0x02,
        0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02,
        0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22,
        0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15,
        0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
        0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47,
        0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
        0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84,
        0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a,
        0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
        0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4,
        0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
        0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01,
        0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00,
    ];
    if bytes.len() < 4 || bytes[0] != 0x01 {
        return Vec::new();
    }
    let mut out = HEADER.to_vec();
    out[164] = bytes[1];
    out[166] = bytes[2];
    out.extend_from_slice(&bytes[3..]);
    out.extend_from_slice(&[0xff, 0xd9]);
    out
}

use crate::config::Config;
