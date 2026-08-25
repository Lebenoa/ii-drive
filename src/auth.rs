use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Signs and verifies drive session, media and share-link tokens.
///
/// Session and media tokens carry the Telegram user id they were minted for,
/// covered by the MAC, so a token can only ever act as that one account.
/// Session tokens additionally carry the account's token epoch, so logout can
/// retire every outstanding one — see [`crate::state::AppState::session_user`],
/// which is where "is this epoch still current" is answered.
/// Each kind also has a distinct segment count — session 5, media 3, file 2 —
/// and a distinct payload prefix, so one kind can never verify as another.
#[derive(Clone)]
pub struct Tokens {
    key: Vec<u8>,
    pub ttl_secs: u64,
}

impl Tokens {
    pub fn new(secret: &str, ttl_secs: u64) -> Self {
        Tokens {
            key: secret.as_bytes().to_vec(),
            ttl_secs,
        }
    }

    fn sign(&self, payload: &str) -> String {
        // HMAC-SHA256 accepts every key length, so `new_from_slice` cannot
        // fail today; the zeroed-key fallback keeps the server alive and
        // signing (all tokens share the fallback) if that ever changes.
        // An invariant breach here is logged loudly — see below.
        let mut mac = match HmacSha256::new_from_slice(&self.key) {
            Ok(mac) => mac,
            // HMAC-SHA256 accepts every key length, so this arm cannot fire
            // today; a zeroed key keeps the server signing (all tokens share
            // it) instead of taking the process down — but it is a security
            // downgrade, so scream into the logs if it ever happens.
            Err(_) => {
                tracing::error!(
                    "HMAC key rejected by new_from_slice; signing with public fallback key"
                );
                HmacSha256::new(&Default::default())
            }
        };
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Issues a session token for `user_id` under `epoch`:
    /// `<uid>.<epoch>.<expiry_unix>.<nonce>.<hex signature>`.
    ///
    /// The epoch is inside the MAC, so a stolen token cannot be rewound to an
    /// epoch that is still current.
    pub fn issue(&self, user_id: i64, epoch: u64) -> String {
        let exp = now_unix() + self.ttl_secs;
        let nonce: [u8; 8] = rand::random();
        let nonce_hex = hex::encode(nonce);
        let sig = self.sign(&format!("u/{user_id}/{epoch}/{exp}/{nonce_hex}"));
        format!("{user_id}.{epoch}.{exp}.{nonce_hex}.{sig}")
    }

    /// Constant-time verification of the MAC and expiry only; returns the
    /// token's user id and the epoch it was minted under, or `None` when the
    /// token is malformed, expired, tampered with, or signed by another key.
    ///
    /// Whether that epoch is still current needs the account's state, so it is
    /// decided one layer up in [`crate::state::AppState::session_user`].
    pub fn verify(&self, token: &str) -> Option<(i64, u64)> {
        let mut parts = token.split('.');
        let (Some(uid_str), Some(epoch_str), Some(exp_str), Some(nonce), Some(sig), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let uid = uid_str.parse::<i64>().ok()?;
        let epoch = epoch_str.parse::<u64>().ok()?;
        if exp_str.parse::<u64>().ok()? < now_unix() {
            return None;
        }
        let expected = self.sign(&format!("u/{uid_str}/{epoch_str}/{exp_str}/{nonce}"));
        // Both sides are fixed-length hex digests of the signature, so any
        // length difference means tampering; ct_eq would panic on mismatch.
        if expected.len() != sig.len() || sig.is_empty() {
            return None;
        }
        let ok: bool = expected.as_bytes().ct_eq(sig.as_bytes()).into();
        ok.then_some((uid, epoch))
    }

    /// Signs a share link for one file: `<expiry>.<hex sig over "f/{uid}/{exp}">`.
    /// Unlike session tokens, this grants access to a single file only.
    pub fn sign_file(&self, uid: &str, ttl_secs: u64) -> String {
        let exp = now_unix() + ttl_secs;
        let payload = format!("f/{uid}/{exp}");
        format!("{exp}.{}", self.sign(&payload))
    }

    /// Verifies a `sign_file` signature for `uid`.
    pub fn verify_file(&self, uid: &str, sig: &str) -> bool {
        let mut parts = sig.split('.');
        let (Some(exp_str), Some(mac), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let Ok(exp) = exp_str.parse::<u64>() else {
            return false;
        };
        if exp < now_unix() {
            return false;
        }
        let expected = self.sign(&format!("f/{uid}/{exp_str}"));
        expected.len() == mac.len()
            && !mac.is_empty()
            && expected.as_bytes().ct_eq(mac.as_bytes()).into()
    }

    /// Signs a short-lived media-read token for `user_id`:
    /// `<uid>.<expiry>.<hex sig over "m/{uid}/{exp}">`.
    /// Grants read access to that user's private files' raw/thumb endpoints —
    /// meant for `<img>`/`<video>` srcs so the long-lived session token never
    /// appears in a URL (logs, history, Referer). The uid is signed in so a
    /// media token cannot be pointed at another tenant's files.
    ///
    /// Deliberately NOT epoch-covered, unlike session tokens: the TTL is
    /// minutes-to-an-hour, so the window a logout would close is already
    /// closed by expiry, and keeping the epoch out means in-flight `<img>`
    /// loads need no revocation lookup. The asymmetry is a decision, not an
    /// oversight — widen it only if media TTLs ever grow.
    pub fn sign_media(&self, user_id: i64, ttl_secs: u64) -> String {
        let exp = now_unix() + ttl_secs;
        let sig = self.sign(&format!("m/{user_id}/{exp}"));
        format!("{user_id}.{exp}.{sig}")
    }

    /// Verifies a `sign_media` token and returns the user id it was minted for.
    pub fn verify_media(&self, token: &str) -> Option<i64> {
        let mut parts = token.split('.');
        let (Some(uid_str), Some(exp_str), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let uid = uid_str.parse::<i64>().ok()?;
        if exp_str.parse::<u64>().ok()? < now_unix() {
            return None;
        }
        let expected = self.sign(&format!("m/{uid_str}/{exp_str}"));
        if expected.len() != mac.len() || mac.is_empty() {
            return None;
        }
        let ok: bool = expected.as_bytes().ct_eq(mac.as_bytes()).into();
        ok.then_some(uid)
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The authenticated Telegram user id behind the current request, published to
/// handlers as `axum::Extension<Caller>` so every query can be scoped to it.
#[derive(Clone, Copy, Debug)]
pub struct Caller(pub i64);

/// Bearer-token guard applied to the protected API subrouter.
///
/// Revocation lives in [`crate::state::AppState::session_user`] so the public
/// routes, which never run this middleware, decide it identically.
pub async fn guard(mut req: axum::http::Request<axum::body::Body>, next: Next) -> Response {
    let state = crate::state::get();
    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);

    let caller = match token {
        Some(tok) => state.session_user(&tok).await,
        None => None,
    };

    let Some(uid) = caller else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    };
    req.extensions_mut().insert(Caller(uid));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = Tokens::new("hunter2", 60);
        let tok = t.issue(7, 0);
        assert_eq!(t.verify(&tok), Some((7, 0)));
        assert_eq!(t.verify(&format!("{tok}x")), None);
        assert_eq!(t.verify("garbage"), None);
        // Four segments is the old session shape; it must not verify.
        assert_eq!(t.verify("a.b.c.d"), None);
        assert_eq!(t.verify("a.b.c.d.e"), None);
    }

    #[test]
    fn token_carries_issued_uid_and_epoch() {
        let t = Tokens::new("hunter2", 60);
        for uid in [1i64, -42, i64::MAX, i64::MIN] {
            assert_eq!(t.verify(&t.issue(uid, 3)), Some((uid, 3)));
        }
        assert_eq!(t.verify(&t.issue(7, u64::MAX)), Some((7, u64::MAX)));
    }

    #[test]
    fn swapped_uid_rejected() {
        let t = Tokens::new("hunter2", 60);
        let tok = t.issue(7, 0);
        let tail = tok.split_once('.').expect("token has segments").1;
        // Re-pointing the token at another account breaks the MAC.
        assert_eq!(t.verify(&format!("8.{tail}")), None);
    }

    /// The epoch is the revocation counter, so it has to be inside the MAC:
    /// otherwise a holder of a retired token just rewrites the segment.
    #[test]
    fn rewritten_epoch_rejected() {
        let t = Tokens::new("hunter2", 60);
        let tok = t.issue(7, 1);
        let mut seg: Vec<&str> = tok.split('.').collect();
        assert_eq!(seg.len(), 5, "session tokens have five segments");
        for forged in ["0", "2", "99"] {
            seg[1] = forged;
            assert_eq!(t.verify(&seg.join(".")), None);
        }
    }

    #[test]
    fn expired_token_rejected() {
        let t = Tokens::new("k", 0);
        let tok = t.issue(7, 0); // exp = now + 0 => already expired on verify
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(t.verify(&tok), None);
    }

    #[test]
    fn media_token_is_per_user() {
        let t = Tokens::new("k", 3600);
        let a = t.sign_media(11, 60);
        assert_eq!(t.verify_media(&a), Some(11));
        // A's token never resolves to B, and cannot be re-pointed at B.
        assert_ne!(t.verify_media(&a), Some(22));
        let tail = a.split_once('.').expect("token has segments").1;
        assert_eq!(t.verify_media(&format!("22.{tail}")), None);
        assert_eq!(t.verify_media("garbage"), None);
    }

    #[test]
    fn token_kinds_do_not_cross_verify() {
        let t = Tokens::new("k", 3600);
        let session = t.issue(11, 0);
        let media = t.sign_media(11, 60);
        let file = t.sign_file("01ABC", 60);
        assert_eq!(t.verify_media(&session), None);
        assert_eq!(t.verify(&media), None);
        assert!(!t.verify_file("01ABC", &session));
        assert!(!t.verify_file("01ABC", &media));
        assert_eq!(t.verify(&file), None);
        assert_eq!(t.verify_media(&file), None);
    }

    #[test]
    fn file_link_roundtrip() {
        let t = Tokens::new("k", 3600);
        let sig = t.sign_file("01ABC", 60);
        assert!(t.verify_file("01ABC", &sig));
        // Wrong uid, garbage, and cross-use as a session token all fail.
        assert!(!t.verify_file("01XYZ", &sig));
        assert!(!t.verify_file("01ABC", "garbage"));
        assert_eq!(t.verify(&sig), None);
    }

    #[test]
    fn expired_file_link_rejected() {
        let t = Tokens::new("k", 0);
        let sig = t.sign_file("01ABC", 0);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!t.verify_file("01ABC", &sig));
    }

    #[test]
    fn wrong_key_rejected() {
        let tok = Tokens::new("k1", 3600).issue(7, 0);
        assert_eq!(Tokens::new("k2", 3600).verify(&tok), None);
        let media = Tokens::new("k1", 3600).sign_media(7, 60);
        assert_eq!(Tokens::new("k2", 3600).verify_media(&media), None);
    }
}
