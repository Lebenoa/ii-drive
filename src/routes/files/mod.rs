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
/// Runs on the account that posted them, which is the file's owner.
async fn cleanup_parts(tg: &crate::tg::TgManager, parts: &[crate::db::FilePart]) {
    for p in parts {
        if let Err(e) = tg.delete_message(p.message_id, &p.chat).await {
            tracing::error!(message_id = p.message_id, "orphaned telegram message: {e}");
        }
    }
}

/// The `Authorization: Bearer` token a request carries, if any.
fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Whether a read of `row` is allowed on the public raw/thumb endpoints,
/// which carry no `Caller` because they are reachable without a session.
///
/// Every identity-bearing credential must resolve to the row's OWNER, so a
/// session or media token minted for one account can never open another
/// account's private file. The share sig is deliberately identity-free: it
/// is scoped to this single uid, which is the whole point of a share link.
fn may_read(
    tokens: &crate::auth::Tokens,
    row: &crate::db::FileRow,
    headers: &axum::http::HeaderMap,
    q: &std::collections::HashMap<String, String>,
) -> bool {
    row.public
        || q.get("sig")
            .is_some_and(|s| tokens.verify_file(&row.uid, s))
        || q.get("mt")
            .is_some_and(|t| tokens.verify_media(t) == Some(row.owner))
        || bearer(headers).is_some_and(|t| tokens.verify(t) == Some(row.owner))
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: i64 = 111;
    const B: i64 = 222;

    fn row(owner: i64, public: bool) -> crate::db::FileRow {
        crate::db::FileRow {
            owner,
            uid: "01FILE".to_string(),
            name: "x.bin".to_string(),
            mime: "application/octet-stream".to_string(),
            size: 1,
            message_id: 1,
            chat: "c".to_string(),
            created_at: 0,
            folder: String::new(),
            parts: Vec::new(),
            public,
            thumb: None,
        }
    }

    fn bearer_headers(token: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().expect("header value"),
        );
        h
    }

    fn query(k: &str, v: &str) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([(k.to_string(), v.to_string())])
    }

    /// The point of tenant scoping: A's credentials never open B's file.
    #[test]
    fn credentials_only_open_their_own_tenant() {
        let t = crate::auth::Tokens::new("k", 3600);
        let none = std::collections::HashMap::new();
        let empty = axum::http::HeaderMap::new();

        let session_a = t.issue(A);
        assert!(may_read(&t, &row(A, false), &bearer_headers(&session_a), &none));
        assert!(!may_read(&t, &row(B, false), &bearer_headers(&session_a), &none));

        let media_a = t.sign_media(A, 60);
        assert!(may_read(&t, &row(A, false), &empty, &query("mt", &media_a)));
        assert!(!may_read(&t, &row(B, false), &empty, &query("mt", &media_a)));
    }

    /// A share sig is file-scoped, so it works for any owner — but only for
    /// the exact uid it was minted for.
    #[test]
    fn share_sig_is_file_scoped() {
        let t = crate::auth::Tokens::new("k", 3600);
        let empty = axum::http::HeaderMap::new();
        let sig = t.sign_file("01FILE", 60);
        assert!(may_read(&t, &row(B, false), &empty, &query("sig", &sig)));

        let other = t.sign_file("01OTHER", 60);
        assert!(!may_read(&t, &row(B, false), &empty, &query("sig", &other)));
    }

    /// A public file needs no credential at all.
    #[test]
    fn public_files_need_no_credential() {
        let t = crate::auth::Tokens::new("k", 3600);
        let empty = axum::http::HeaderMap::new();
        let none = std::collections::HashMap::new();
        assert!(may_read(&t, &row(A, true), &empty, &none));
        assert!(!may_read(&t, &row(A, false), &empty, &none));
    }

    /// The public read endpoints must not become an existence oracle: with
    /// several accounts behind one URL space, a "forbidden" that differs from
    /// "not found" would let anybody enumerate another account's file ids.
    #[tokio::test]
    async fn a_foreign_file_is_indistinguishable_from_a_missing_one() {
        let db = crate::db::open_mem().await.expect("open test db");
        let state = crate::app::shared_state(db);
        crate::db::insert(&state.db, &row(B, false))
            .await
            .expect("plant B's private row");

        let probe = |id: &str| {
            let state = state.clone();
            let id = id.to_string();
            async move {
                let req = axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .expect("request");
                let err = super::raw_file(
                    axum::extract::State(state),
                    axum::extract::Path(id),
                    axum::extract::Query(std::collections::HashMap::new()),
                    req,
                )
                .await
                .err()
                .expect("anonymous read is refused");
                (err.0, err.1)
            }
        };

        let foreign = probe("01FILE").await;
        let missing = probe("01NOSUCHFILE").await;
        assert_eq!(foreign, missing, "status and body must not disclose existence");
        assert_eq!(foreign.0, axum::http::StatusCode::NOT_FOUND);
    }
}
