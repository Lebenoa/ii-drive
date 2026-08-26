//! First-run setup wizard: when no config file exists, the server boots the
//! web UI's `/setup` route instead of the drive. The user pastes their
//! Telegram app credentials and phone number; the wizard validates them,
//! generates the session secret, writes `config.toml`, and exits — the
//! operator then starts ii-drive normally. Exiting (rather than hot-swapping
//! the config) keeps behavior identical on every platform and supervisor.
//!
//! The form itself lives in the frontend (`web/src/routes/setup`), so it
//! looks like the rest of the app and gets its translations for free. This
//! module only serves that bundle and owns the two endpoints behind it.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tower_http::services::{ServeDir, ServeFile};

/// Runs when `config_path` does not exist. Serves the wizard until a valid
/// submission lands, writes the file, then returns — `main()` exits with a
/// "start again" message.
pub async fn run(config_path: PathBuf) -> color_eyre::Result<()> {
    // The wizard binds loopback by default so credentials are never
    // submitted over an unauthenticated LAN hop.
    let cfg = crate::config::get();
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .map_err(|e| color_eyre::eyre::eyre!("invalid host/port from defaults: {e}"))?;

    // `config::init` has not run — there is no file to load yet — but these
    // two paths never came from the file: they are located from the binary,
    // so they already resolve exactly as they will on every later boot.

    // Unlike the running server, which degrades to API-only, the wizard *is*
    // the web UI: without a build there is no form to fill in.
    let dist = Path::new(&cfg.web_dist);
    if !dist.is_dir() {
        color_eyre::eyre::bail!(
            "web UI not found at `{}`, so there is no setup form to serve — \
             run `nub install && nub run build` in web/, or use a release bundle",
            dist.display()
        );
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("no config found — setup wizard running on http://{addr}/setup");

    let spa = ServeDir::new(dist).fallback(ServeFile::new(dist.join("index.html")));
    let app = Router::new()
        // Land the operator on the wizard rather than the drive shell, which
        // would only bounce off endpoints that do not exist yet.
        .route("/", get(|| async { Redirect::temporary("/setup") }))
        .route("/api/setup", get(probe).post(submit))
        // The SPA blocks its first paint on a dictionary. A plain static
        // serve is enough here: the wizard only ever asks for one file, and
        // the language picker it would filter for is not on this page.
        .nest_service("/locales", ServeDir::new(&cfg.locales_dir))
        .fallback_service(spa)
        .with_state(SetupState { config_path });
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct SetupState {
    config_path: PathBuf,
}

/// GET /api/setup — present only while the wizard is running, so the page can
/// tell "first run" from someone typing /setup at a configured server.
async fn probe(State(state): State<SetupState>) -> Json<serde_json::Value> {
    Json(json!({ "config_path": state.config_path.display().to_string() }))
}

#[derive(serde::Deserialize)]
struct SetupBody {
    api_id: i64,
    api_hash: String,
    /// One phone per line or comma-separated; both tolerated.
    phones: String,
}

/// POST /api/setup — validates, writes `config.toml`, then exits the process.
///
/// The error string is what the form shows the user, so it names the field and
// The `exit` is the whole point of the wizard: it terminates the process after
// writing the config so the operator restarts into the real server.
#[allow(clippy::exit)]
async fn submit(State(state): State<SetupState>, Json(body): Json<SetupBody>) -> Response {
    match validate_and_write(&state.config_path, &body) {
        Ok(written) => {
            // Let the response reach the browser before exiting.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                std::process::exit(0);
            });
            Json(json!({ "ok": true, "config_path": written })).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 6 && digits.len() <= 15).then_some(digits)
}

fn validate_and_write(config_path: &Path, body: &SetupBody) -> Result<String, String> {
    let api_hash = body.api_hash.trim().to_string();
    if api_hash.len() != 32 || !api_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("api_hash must be the 32-character hex string from my.telegram.org".into());
    }
    if body.api_id <= 0 {
        return Err("api_id must be a positive number from my.telegram.org".into());
    }

    let mut phones = Vec::new();
    for raw in body.phones.split([',', '\n', ';']) {
        if raw.trim().is_empty() {
            continue;
        }
        let normalized =
            normalize_phone(raw).ok_or_else(|| format!("`{raw}` is not a valid phone number"))?;
        phones.push(normalized);
    }
    if phones.is_empty() {
        return Err("at least one phone number is required".into());
    }

    // The secret is generated here rather than at next boot so the written
    // config is complete and restart-stable.
    let bytes: [u8; 32] = rand::random();
    let secret = hex::encode(bytes);

    let phones_toml = phones
        .iter()
        .map(|p| format!("    \"+{p}\","))
        .collect::<Vec<_>>()
        .join("\n");
    let toml = format!(
        r#"# Generated by the first-run setup wizard.
# See config.example.toml for every option.

api_id = {api_id}
api_hash = "{api_hash}"
secret = "{secret}"

allowed_phones = [
{phones_toml}
]

# The wizard's phone doubles as the operator (admin endpoints); edit to taste.
admin_phones = [
{phones_toml}
]

# At-rest encryption of uploads is off by default. To enable it, add
# crypt_enabled = true and a long random crypt_password (see
# config.example.toml). Old plaintext files are still served as-is.
"#,
        api_id = body.api_id,
    );

    std::fs::write(config_path, toml)
        .map_err(|e| format!("could not write {}: {e}", config_path.display()))?;

    // The path, not a rendered page: the frontend owns what success looks
    // like, and it wants the path to tell the operator what was written.
    Ok(config_path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_complete_config() {
        let dir = std::env::temp_dir().join("iidrive-setup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let written = validate_and_write(
            &path,
            &SetupBody {
                api_id: 12345,
                api_hash: "a".repeat(32),
                phones: "+1 555 123 4567, +447700900123".into(),
            },
        )
        .unwrap();

        assert_eq!(written, path.display().to_string());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_id = 12345"));
        assert!(text.contains("+15551234567"));
        assert!(text.contains("+447700900123"));
        // Secret is generated and never a known constant.
        let secret_line = text.lines().find(|l| l.starts_with("secret")).unwrap();
        assert!(!secret_line.contains("change-me"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_bad_credentials() {
        let dir = std::env::temp_dir().join("iidrive-setup-bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        assert!(
            validate_and_write(
                &path,
                &SetupBody {
                    api_id: 0,
                    api_hash: "a".repeat(32),
                    phones: "+15551234567".into()
                }
            )
            .is_err()
        );
        assert!(
            validate_and_write(
                &path,
                &SetupBody {
                    api_id: 5,
                    api_hash: "short".into(),
                    phones: "+15551234567".into()
                }
            )
            .is_err()
        );
        assert!(
            validate_and_write(
                &path,
                &SetupBody {
                    api_id: 5,
                    api_hash: "a".repeat(32),
                    phones: "abc".into()
                }
            )
            .is_err()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
