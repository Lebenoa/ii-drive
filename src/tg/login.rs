use std::path::PathBuf;
use std::sync::Arc;

use mtprsto::api::TelegramClient;
use mtprsto::error::Error as TgError;
use mtprsto::serialize::TLReader;
use mtprsto::session::{SessionStore, SessionStorage};
use mtprsto::types;

use crate::config::Config;

use super::{TgManager, UNKNOWN_USER, friendly, get_me_info};

/// How far a sign-in has got. Each step consumes the previous one's
/// exchange client, so a flow can never be replayed. Under grammers this
/// state lived in `LoginToken`/`PasswordToken`; mtprsto's phone flow is
/// driven through its one-shot `TelegramClient` with a
/// `phone_code_hash`, so the client itself rides in the flow.
enum Flow {
    /// No code requested yet, or the flow is spent.
    Idle,
    CodeSent {
        phone: String,
        phone_code_hash: Vec<u8>,
        tg: Box<TelegramClient>,
    },
    PasswordNeeded {
        tg: Box<TelegramClient>,
    },
}

/// Where a sign-in stands after Telegram accepted a confirmation code.
pub(super) enum CodeStep {
    /// Signed in; the account behind the throwaway session is now known.
    Done(i64),
    PasswordRequired {
        hint: Option<String>,
    },
}

/// A rejected login step.
pub(super) struct LoginFailure {
    pub(super) message: String,
    /// True when a wrong secret was submitted, as opposed to a client using
    /// the flow out of order. Only wrong secrets count towards the
    /// brute-force block.
    pub(super) wrong_secret: bool,
}

impl LoginFailure {
    fn wrong(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            wrong_secret: true,
        }
    }

    fn misuse(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            wrong_secret: false,
        }
    }
}

/// One sign-in in flight. It carries its own manager over its own throwaway
/// session file, because the account — and therefore the file it will end up
/// in — is unknown until Telegram accepts the last secret. Several of these
/// run side by side without touching each other or any signed-in account.
pub(super) struct Pending {
    pub(super) manager: Arc<TgManager>,
    /// Throwaway session file backing `manager` until the account is known.
    pub(super) session_path: PathBuf,
    flow: Flow,
}

impl Pending {
    pub(super) fn new(cfg: Config, session_path: PathBuf) -> Self {
        let manager = TgManager::new(
            cfg,
            session_path.to_string_lossy().into_owned(),
            UNKNOWN_USER,
        );
        Self {
            manager: Arc::new(manager),
            session_path,
            flow: Flow::Idle,
        }
    }

    /// Asks Telegram to deliver a confirmation code. The session file is
    /// brand new, so there is no stale auth key to recover from here.
    pub(super) async fn send_code(&mut self, phone: &str) -> Result<(), String> {
        // The auth key must exist before the code request: connect once so
        // the DH handshake runs and the throwaway session file is written.
        let client = self.manager.ensure_connected().await?;
        drop(client);
        // The code exchange goes over mtprsto's one-shot auth client,
        // driven by the session's freshly persisted auth key — the pool
        // connection would work too, but the auth flow is what the
        // library ships it for.
        let mut store = SessionStore::new(self.manager.session_path());
        let data = SessionStorage::load(&mut store)
            .map_err(|e| friendly(format!("cannot load login session: {e}")))?
            .ok_or_else(|| "login session vanished before the code request".to_string())?;
        let auth_key = data
            .decode_auth_key()
            .map_err(|e| friendly(format!("cannot decode login session: {e}")))?;
        let mut tg = Box::new(TelegramClient::with_session(
            data.dc_id,
            auth_key,
            data.server_salt,
            Some(self.manager.cfg.api_id),
            Some(self.manager.cfg.api_hash.clone()),
        ));
        let sent = tg
            .auth_send_code(phone)
            .await
            .map_err(|e| friendly(format!("failed to send code: {e}")))?;
        self.flow = Flow::CodeSent {
            phone: phone.to_string(),
            phone_code_hash: sent.phone_code_hash,
            tg,
        };
        Ok(())
    }

    pub(super) async fn sign_in(&mut self, code: &str) -> Result<CodeStep, LoginFailure> {
        let (phone, phone_code_hash, mut tg) = match std::mem::replace(&mut self.flow, Flow::Idle)
        {
            Flow::CodeSent {
                phone,
                phone_code_hash,
                tg,
            } => (phone, phone_code_hash, tg),
            other => {
                self.flow = other;
                return Err(LoginFailure::misuse("no code was requested"));
            }
        };
        match tg.auth_sign_in(&phone, &phone_code_hash, code).await {
            Ok(()) => {
                let user_id = match tg.user_id.filter(|id| *id != 0) {
                    Some(id) => id,
                    None => self.known_user_id().await.map_err(LoginFailure::misuse)?,
                };
                self.flow = Flow::Idle;
                Ok(CodeStep::Done(user_id))
            }
            Err(TgError::InvalidCode { .. }) => {
                // Keep the flow so the user can retry with a new code entry.
                self.flow = Flow::CodeSent {
                    phone,
                    phone_code_hash,
                    tg,
                };
                Err(LoginFailure::wrong("invalid confirmation code"))
            }
            Err(TgError::CodeResent { phone_code_hash }) => {
                // The code window expired while typing and the server has
                // already delivered a fresh code; continue with its hash.
                self.flow = Flow::CodeSent {
                    phone,
                    phone_code_hash,
                    tg,
                };
                Err(LoginFailure::wrong(
                    "the code expired — a new one was sent; enter that",
                ))
            }
            Err(TgError::Rpc { ref error_message, .. })
                if error_message.contains("SESSION_PASSWORD_NEEDED") =>
            {
                let hint = password_hint(&mut tg).await;
                self.flow = Flow::PasswordNeeded { tg };
                Ok(CodeStep::PasswordRequired { hint })
            }
            Err(TgError::SignUpRequired) => Err(LoginFailure::misuse(
                "this account needs sign-up, which is not supported",
            )),
            Err(e) => Err(LoginFailure::wrong(format!("sign-in failed: {e}"))),
        }
    }

    pub(super) async fn check_password(&mut self, password: &str) -> Result<i64, LoginFailure> {
        let mut tg = match std::mem::replace(&mut self.flow, Flow::Idle) {
            Flow::PasswordNeeded { tg } => tg,
            other => {
                self.flow = other;
                return Err(LoginFailure::misuse("no password step pending"));
            }
        };
        match tg.auth_check_password(password).await {
            Ok(()) => {
                let user_id = match tg.user_id.filter(|id| *id != 0) {
                    Some(id) => id,
                    None => self.known_user_id().await.map_err(LoginFailure::misuse)?,
                };
                self.flow = Flow::Idle;
                Ok(user_id)
            }
            Err(TgError::InvalidPassword { .. }) => {
                self.flow = Flow::PasswordNeeded { tg };
                Err(LoginFailure::wrong("invalid password"))
            }
            Err(other) => {
                // Keep the password step alive across transient failures.
                self.flow = Flow::PasswordNeeded { tg };
                Err(LoginFailure::wrong(format!(
                    "password check failed: {other}"
                )))
            }
        }
    }

    /// The signed-in account's id via `get_me` on the throwaway session —
    /// the fallback for when the auth exchange itself did not report it.
    async fn known_user_id(&self) -> Result<i64, String> {
        let client = self.manager.ensure_connected().await?;
        let me = get_me_info(&*client.lock().await).await;
        me.map(|info| info.id)
            .ok_or_else(|| "signed in but Telegram did not report the account; try again".into())
    }
}

/// The 2FA hint (`account.getPassword`'s `hint:flags.3?string`) shown on
/// the password step. Best-effort: a parse miss simply hides the hint.
async fn password_hint(tg: &mut TelegramClient) -> Option<String> {
    let raw = tg.invoke(types::ACCOUNT_GET_PASSWORD, &[]).await.ok()?;
    parse_password_hint(&raw)
}

/// Pulls `hint` out of an `account.password` payload, walking the same
/// field order as mtprsto's own parser and stopping right after the hint.
fn parse_password_hint(raw: &[u8]) -> Option<String> {
    let mut r = TLReader::new(raw);
    let ctor = r.read_u32().ok()?;
    if ctor != types::ACCOUNT_GET_PASSWORD_RESPONSE {
        return None;
    }
    let flags = r.read_i32().ok()?;
    if flags & (1 << 2) != 0 {
        // current_algo:passwordKdfAlgoSHA256…ModPow salt1 salt2 g p,
        // followed by the unconditional srp_B/srp_id pair.
        let algo = r.read_u32().ok()?;
        if algo != types::PASSWORD_KDF_ALGO_SHA256_SHA256_PBKDF2_HMACSHA512_100K_MODPOW {
            return None;
        }
        let _salt1 = r.read_bytes().ok()?;
        let _salt2 = r.read_bytes().ok()?;
        let _g = r.read_i32().ok()?;
        let _p = r.read_bytes().ok()?;
        let _srp_b = r.read_bytes().ok()?;
        let _srp_id = r.read_i64().ok()?;
    }
    if flags & (1 << 3) != 0 {
        return String::from_utf8(r.read_bytes().ok()?).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint parser must read exactly up to `hint` on a minimal
    /// `account.password` with a password set — miscounting the algo block
    /// would garble the hint or hide it.
    #[test]
    fn password_hint_is_read_from_the_challenge() {
        use mtprsto::serialize::TLWriter;
        let mut w = TLWriter::new();
        w.write_u32(types::ACCOUNT_GET_PASSWORD_RESPONSE);
        w.write_i32(1 << 2 | 1 << 3); // has_password + hint
        w.write_u32(types::PASSWORD_KDF_ALGO_SHA256_SHA256_PBKDF2_HMACSHA512_100K_MODPOW);
        w.write_bytes(&[1, 2]); // salt1
        w.write_bytes(&[3]); // salt2
        w.write_i32(3); // g
        w.write_bytes(&[4; 256]); // p
        w.write_bytes(&[5; 128]); // srp_B
        w.write_i64(99); // srp_id
        w.write_bytes(b"pets name"); // hint

        assert_eq!(
            parse_password_hint(&w.into_bytes()).as_deref(),
            Some("pets name")
        );
    }

    /// Without a password there is no challenge and no hint.
    #[test]
    fn no_password_means_no_hint() {
        use mtprsto::serialize::TLWriter;
        let mut w = TLWriter::new();
        w.write_u32(types::ACCOUNT_GET_PASSWORD_RESPONSE);
        w.write_i32(0);
        assert_eq!(parse_password_hint(&w.into_bytes()), None);
    }
}
