use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Signs and verifies drive session tokens.
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

    /// Issues a token: `<expiry_unix>.<nonce>.<hex signature>`.
    pub fn issue(&self) -> String {
        let exp = now_unix() + self.ttl_secs;
        let nonce: [u8; 8] = rand::random();
        let nonce_hex = hex::encode(nonce);
        let payload = format!("{exp}.{nonce_hex}");
        let sig = self.sign(&payload);
        format!("{payload}.{sig}")
    }

    /// Constant-time verification; rejects expired or tampered tokens.
    pub fn verify(&self, token: &str) -> bool {
        let mut parts = token.split('.');
        let (Some(exp_str), Some(nonce), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let Ok(exp) = exp_str.parse::<u64>() else {
            return false;
        };
        if exp < now_unix() {
            return false;
        }
        let payload = format!("{exp_str}.{nonce}");
        let expected = self.sign(&payload);
        // Both sides are fixed-length hex digests of the signature, so any
        // length difference means tampering; ct_eq would panic on mismatch.
        if expected.len() != sig.len() || sig.is_empty() {
            return false;
        }
        expected.as_bytes().ct_eq(sig.as_bytes()).into()
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bearer-token guard applied to the protected API subrouter.
pub async fn guard(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|tok| state.tokens.verify(tok));

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip() {
        let t = Tokens::new("hunter2", 60);
        let tok = t.issue();
        assert!(t.verify(&tok));
        assert!(!t.verify(&format!("{tok}x")));
        assert!(!t.verify("garbage"));
        assert!(!t.verify("a.b.c.d"));
    }

    #[test]
    fn expired_token_rejected() {
        let t = Tokens::new("k", 0);
        let tok = t.issue(); // exp = now + 0 => already expired on verify
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!t.verify(&tok));
    }

    #[test]
    fn wrong_key_rejected() {
        let tok = Tokens::new("k1", 3600).issue();
        assert!(!Tokens::new("k2", 3600).verify(&tok));
    }
}

