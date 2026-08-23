use super::{
    LOGIN_BLOCK_SECS, LoginFlow, MAX_LOGIN_ATTEMPTS, SignInOutcome, State, TgManager, friendly,
    is_auth_error,
};

impl TgManager {
    /// Rejects login attempts while a brute-force block is active.
    fn login_gate(st: &mut State) -> Result<(), String> {
        if let Some(until) = st.blocked_until {
            let now = std::time::Instant::now();
            if now < until {
                let secs = (until - now).as_secs() + 1;
                return Err(format!(
                    "too many failed login attempts; try again in {secs}s"
                ));
            }
            // Block has lapsed.
            st.blocked_until = None;
        }
        Ok(())
    }

    fn record_login_failure(st: &mut State) {
        st.failed_logins += 1;
        if st.failed_logins >= MAX_LOGIN_ATTEMPTS {
            st.blocked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_BLOCK_SECS));
            st.failed_logins = 0;
            tracing::warn!("login blocked for {LOGIN_BLOCK_SECS}s after repeated failures");
        }
    }

    pub(super) fn record_login_success(st: &mut State) {
        st.failed_logins = 0;
        st.blocked_until = None;
    }

    pub async fn send_code(&self, phone: &str) -> Result<(), String> {
        {
            // The flow is deliberately single-user: a new code request
            // replaces any pending one, so a stale browser tab can never
            // complete a login started by another.
            let st = self.st.lock().await;
            if !matches!(st.login, LoginFlow::None) {
                tracing::warn!("replacing a pending login flow with a new code request");
            }
        }
        match self.try_send_code(phone).await {
            // A session that holds an auth key Telegram does not know (partial
            // login, revoked session) rejects even SendCode; rebuild it once.
            Err(e) if is_auth_error(&e) => {
                self.reset_session().await?;
                self.try_send_code(phone).await
            }
            other => other,
        }
    }

    async fn try_send_code(&self, phone: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        let token = client
            .request_login_code(phone, &self.cfg.api_hash)
            .await
            .map_err(|e| friendly(format!("failed to send code: {e}")))?;
        let mut st = self.st.lock().await;
        st.login = LoginFlow::CodeSent {
            phone: phone.to_string(),
            token: Box::new(token),
        };
        Ok(())
    }

    pub async fn sign_in(&self, code: &str) -> Result<SignInOutcome, String> {
        let client = self.ensure().await?;
        {
            let mut st = self.st.lock().await;
            Self::login_gate(&mut st)?;
        }
        let (phone, token) = match std::mem::replace(&mut self.st.lock().await.login, LoginFlow::None)
        {
            LoginFlow::CodeSent { phone, token } => (phone, *token),
            _ => return Err("no code was requested".to_string()),
        };
        match client.sign_in(&token, code).await {
            Ok(_user) => {
                self.finish_auth(&client).await;
                Ok(SignInOutcome::Done)
            }
            Err(grammers_client::SignInError::PasswordRequired(pt)) => {
                let hint = pt.hint().map(|h| h.to_string());
                self.st.lock().await.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Ok(SignInOutcome::PasswordRequired { hint })
            }
            Err(grammers_client::SignInError::InvalidCode) => {
                // Keep the token so the user can retry with a new code entry.
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                st.login = LoginFlow::CodeSent {
                    phone,
                    token: Box::new(token),
                };
                Err("invalid confirmation code".to_string())
            }
            Err(grammers_client::SignInError::InvalidPassword(pt)) => {
                self.st.lock().await.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt),
                };
                Err("invalid password".to_string())
            }
            Err(grammers_client::SignInError::SignUpRequired) => {
                Err("this account needs sign-up, which is not supported".to_string())
            }
            Err(grammers_client::SignInError::Other(e)) => {
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                drop(st);
                Err(format!("sign-in failed: {e}"))
            }
        }
    }

    pub async fn check_password(&self, password: &str) -> Result<(), String> {
        let client = self.ensure().await?;
        {
            let mut st = self.st.lock().await;
            Self::login_gate(&mut st)?;
        }
        let pt = match std::mem::replace(&mut self.st.lock().await.login, LoginFlow::None) {
            LoginFlow::PasswordNeeded { token, .. } => *token,
            _ => return Err("no password step pending".to_string()),
        };
        match client.check_password(pt, password).await {
            Ok(_user) => {
                self.finish_auth(&client).await;
                Ok(())
            }
            Err(grammers_client::SignInError::InvalidPassword(pt2)) => {
                let mut st = self.st.lock().await;
                Self::record_login_failure(&mut st);
                st.login = LoginFlow::PasswordNeeded {
                    token: Box::new(pt2),
                };
                Err("invalid password".to_string())
            }
            Err(other) => Err(format!("password check failed: {other:?}")),
        }
    }
}
