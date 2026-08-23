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
/// Each kind also has a distinct segment count and payload prefix, so one
/// kind can never verify as another.
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
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Issues a session token for `user_id`: `<uid>.<expiry_unix>.<nonce>.<hex signature>`.
    pub fn issue(&self, user_id: i64) -> String {
        let exp = now_unix() + self.ttl_secs;
        let nonce: [u8; 8] = rand::random();
        let nonce_hex = hex::encode(nonce);
        let sig = self.sign(&format!("u/{user_id}/{exp}/{nonce_hex}"));
        format!("{user_id}.{exp}.{nonce_hex}.{sig}")
    }

    /// Constant-time verification; returns the token's user id, or `None` when
    /// the token is malformed, expired, tampered with, or signed by another key.
    pub fn verify(&self, token: &str) -> Option<i64> {
        let mut parts = token.split('.');
        let (Some(uid_str), Some(exp_str), Some(nonce), Some(sig), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let uid = uid_str.parse::<i64>().ok()?;
        if exp_str.parse::<u64>().ok()? < now_unix() {
            return None;
        }
        let expected = self.sign(&format!("u/{uid_str}/{exp_str}/{nonce}"));
        // Both sides are fixed-length hex digests of the signature, so any
        // length difference means tampering; ct_eq would panic on mismatch.
        if expected.len() != sig.len() || sig.is_empty() {
            return None;
        }
        let ok: bool = expected.as_bytes().ct_eq(sig.as_bytes()).into();
        ok.then_some(uid)
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
        let (Some(exp_str), Some(mac), None) =
            (parts.next(), parts.next(), parts.next())
        else {
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
pub async fn guard(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let caller = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|tok| state.tokens.verify(tok));

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
        let tok = t.issue(7);
        assert_eq!(t.verify(&tok), Some(7));
        assert_eq!(t.verify(&format!("{tok}x")), None);
        assert_eq!(t.verify("garbage"), None);
        assert_eq!(t.verify("a.b.c.d"), None);
    }

    #[test]
    fn token_carries_issued_uid() {
        let t = Tokens::new("hunter2", 60);
        for uid in [1i64, -42, i64::MAX, i64::MIN] {
            assert_eq!(t.verify(&t.issue(uid)), Some(uid));
        }
    }

    #[test]
    fn swapped_uid_rejected() {
        let t = Tokens::new("hunter2", 60);
        let tok = t.issue(7);
        let tail = tok.split_once('.').expect("token has segments").1;
        // Re-pointing the token at another account breaks the MAC.
        assert_eq!(t.verify(&format!("8.{tail}")), None);
    }

    #[test]
    fn expired_token_rejected() {
        let t = Tokens::new("k", 0);
        let tok = t.issue(7); // exp = now + 0 => already expired on verify
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
        let session = t.issue(11);
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
        let tok = Tokens::new("k1", 3600).issue(7);
        assert_eq!(Tokens::new("k2", 3600).verify(&tok), None);
        let media = Tokens::new("k1", 3600).sign_media(7, 60);
        assert_eq!(Tokens::new("k2", 3600).verify_media(&media), None);
    }
}

