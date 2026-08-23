mod dto;
mod folders;
mod listing;
mod meta;
mod serve;
mod thumbs;
mod upload;

// The submodules below are a pure split of what used to be one flat
// `files` module; re-exporting each one's public surface keeps every
// `files::<item>` path resolving exactly as before.
pub use dto::*;
pub use folders::*;
pub use listing::*;
pub use meta::*;
pub use serve::*;
pub use thumbs::*;
pub use upload::*;

use crate::state::AppState;

/// Upper bound on how much of an aborted upload body we swallow just to
/// deliver the error response; beyond this the connection is simply closed.
const DRAIN_CAP: u64 = 32 * 1024 * 1024;

/// How much of the stream head to buffer for cover-art extraction.
const HEAD_CAP: usize = 512 * 1024;

/// Human-readable byte count for error messages ("2.0 GiB").
fn bytes_repr(n: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else {
        format!("{n} B")
    }
}

/// Best-effort removal of already-posted part messages after a failure.
async fn cleanup_parts(state: &AppState, parts: &[crate::db::FilePart]) {
    for p in parts {
        if let Err(e) = state.tg.delete_message(p.message_id, &p.chat).await {
            tracing::error!(message_id = p.message_id, "orphaned telegram message: {e}");
        }
    }
}

/// True when a delete failure only means the message was already gone —
/// retrying the file delete must still be able to succeed over those.
fn is_message_gone(err: &str) -> bool {
    let norm = err.to_lowercase().replace('_', "");
    norm.contains("messageidinvalid") || norm.contains("messageinvalid")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parses `bytes=<start>-<end>` / `bytes=<start>-`; None otherwise.
fn parse_range(v: &str) -> Option<(u64, u64)> {
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range unsupported
    }
    let (s, e) = spec.split_once('-')?;
    let start: u64 = s.parse().ok()?;
    let end: u64 = if e.is_empty() {
        u64::MAX
    } else {
        e.parse().ok()?
    };
    (start <= end).then_some((start, end))
}
