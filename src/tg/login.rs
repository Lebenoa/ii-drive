use std::path::PathBuf;
use std::sync::Arc;

use grammers_client::client::{LoginToken, PasswordToken};

use crate::config::Config;

use super::{TgManager, UNKNOWN_USER, friendly, get_me_info};

/// How far a sign-in has got. Each step consumes the previous one's token,
/// so a flow can never be replayed.
enum Flow {
    /// No code requested yet, or the flow is spent.
    Idle,
    CodeSent {
        phone: String,
        token: Box<LoginToken>,
    },
    PasswordNeeded {
        token: Box<PasswordToken>,
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
        LoginFailure {
            message: message.into(),
            wrong_secret: true,
        }
    }

    fn misuse(message: impl Into<String>) -> Self {
        LoginFailure {
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
        Pending {
            manager: Arc::new(manager),
            session_path,
            flow: Flow::Idle,
        }
    }

    /// Asks Telegram to deliver a confirmation code. The session file is
    /// brand new, so there is no stale auth key to recover from here.
    pub(super) async fn send_code(&mut self, phone: &str) -> Result<(), String> {
        let client = self.manager.ensure().await?;
        let token = client
            .request_login_code(phone, &self.manager.cfg.api_hash)
            .await
            .map_err(|e| friendly(format!("failed to send code: {e}")))?;
        self.flow = Flow::CodeSent {
            phone: phone.to_string(),
            token: Box::new(token),
        };
        Ok(())
    }

    pub(super) async fn sign_in(&mut self, code: &str) -> Result<CodeStep, LoginFailure> {
        let client = self.manager.ensure().await.map_err(LoginFailure::misuse)?;
        let (phone, token) = match std::mem::replace(&mut self.flow, Flow::Idle) {
            Flow::CodeSent { phone, token } => (phone, *token),
            other => {
                self.flow = other;
                return Err(LoginFailure::misuse("no code was requested"));
            }
        };
        match client.sign_in(&token, code).await {
            Ok(_user) => match get_me_info(&client).await {
                Some(me) => Ok(CodeStep::Done(me.id)),
                // Signed in, yet the account is unknown: without an id the
                // session cannot be filed under its owner.
                None => Err(LoginFailure::misuse(
                    "signed in but Telegram did not report the account; try again",
                )),
            },
            Err(grammers_client::SignInError::PasswordRequired(pt)) => {
                let hint = pt.hint().map(|h| h.to_string());
                self.flow = Flow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Ok(CodeStep::PasswordRequired { hint })
            }
            Err(grammers_client::SignInError::InvalidCode) => {
                // Keep the token so the user can retry with a new code entry.
                self.flow = Flow::CodeSent {
                    phone,
                    token: Box::new(token),
                };
                Err(LoginFailure::wrong("invalid confirmation code"))
            }
            Err(grammers_client::SignInError::InvalidPassword(pt)) => {
                self.flow = Flow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Err(LoginFailure::wrong("invalid password"))
            }
            Err(grammers_client::SignInError::SignUpRequired) => Err(LoginFailure::misuse(
                "this account needs sign-up, which is not supported",
            )),
            Err(grammers_client::SignInError::Other(e)) => {
                Err(LoginFailure::wrong(format!("sign-in failed: {e}")))
            }
        }
    }

    pub(super) async fn check_password(&mut self, password: &str) -> Result<i64, LoginFailure> {
        let client = self.manager.ensure().await.map_err(LoginFailure::misuse)?;
        let pt = match std::mem::replace(&mut self.flow, Flow::Idle) {
            Flow::PasswordNeeded { token } => *token,
            other => {
                self.flow = other;
                return Err(LoginFailure::misuse("no password step pending"));
            }
        };
        match client.check_password(pt, password).await {
            Ok(_user) => match get_me_info(&client).await {
                Some(me) => Ok(me.id),
                None => Err(LoginFailure::misuse(
                    "signed in but Telegram did not report the account; try again",
                )),
            },
            Err(grammers_client::SignInError::InvalidPassword(pt2)) => {
                self.flow = Flow::PasswordNeeded {
                    token: Box::new(pt2),
                };
                Err(LoginFailure::wrong("invalid password"))
            }
            Err(other) => Err(LoginFailure::wrong(format!(
                "password check failed: {other:?}"
            ))),
        }
    }
}
