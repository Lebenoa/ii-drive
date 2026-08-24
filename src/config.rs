use anyhow::Result;
use std::sync::{LazyLock, OnceLock, RwLock};

use serde::Deserialize;

/// Process-wide configuration. Loaded once at startup via [`init`], read
/// through [`get`] (cheap snapshot clone), and refreshed from disk with
/// [`reload`]. `LazyLock` makes the static self-initializing without
/// needing a `once_cell`-style dance in `main`.
static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(Config::default()));

/// Where the config was loaded from, kept so [`reload`] re-reads the same
/// file even when the server was started with an explicit path argument.
static CONFIG_PATH: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub api_id: i32,
    pub api_hash: String,
    pub secret: String,
    pub token_ttl_secs: u64,
    pub db_path: String,
    pub session_path: String,
    pub max_file_size: u64,
    pub web_dist: String,
    /// Folder of translation files (`{lang}.json`) served to the web UI,
    /// which downloads a language only after the user picks it.
    pub locales_dir: String,
    pub allowed_phones: Vec<String>,
    /// Phone numbers allowed to reach operator-only endpoints (raw-SQL
    /// browser, config reload). Phone rather than Telegram user id because
    /// Telegram never shows a user their own numeric id, so an id would be
    /// a setting nobody can fill in unaided.
    pub admin_phones: Vec<String>,
    /// Generate thumbnails for videos/images with ffmpeg when available.
    pub media_thumbs: bool,
    /// How an accepted upload reaches Telegram. `Stream` relays the client
    /// body straight into per-part uploaders (no disk usage, but each part
    /// only starts draining once the sequential body feed reaches it).
    /// `Spill` buffers the whole body to `spill_dir` first, then drains all
    /// parts at full aggregate rate — faster tail on fast pipes, costs the
    /// file size in temporary disk space.
    pub upload_strategy: UploadStrategy,
    /// Directory for in-flight upload buffers (strategy `spill`, and the
    /// resumable-upload sessions, which always spill by design).
    pub spill_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStrategy {
    Stream,
    Spill,
}

impl std::fmt::Display for UploadStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UploadStrategy::Stream => "stream",
            UploadStrategy::Spill => "spill",
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    host: Option<String>,
    port: Option<u16>,
    api_id: Option<i32>,
    api_hash: Option<String>,
    secret: Option<String>,
    token_ttl_secs: Option<u64>,
    db_path: Option<String>,
    session_path: Option<String>,
    max_file_size: Option<SizeRepr>,
    web_dist: Option<String>,
    locales_dir: Option<String>,
    allowed_phones: Option<Vec<String>>,
    admin_phones: Option<Vec<String>>,
    media_thumbs: Option<bool>,
    upload_strategy: Option<String>,
    spill_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: "127.0.0.1".into(),
            port: 8080,
            api_id: 0,
            api_hash: String::new(),
            secret: "change-me".into(),
            token_ttl_secs: 30 * 24 * 3600,
            db_path: "data/drive.surrealkv".into(),
            session_path: "data/session.db".into(),
            max_file_size: 2 * 1024 * 1024 * 1024,
            web_dist: "web/dist".into(),
            locales_dir: "locales".into(),
            allowed_phones: Vec::new(),
            admin_phones: Vec::new(),
            media_thumbs: true,
            upload_strategy: UploadStrategy::Stream,
            spill_dir: "data/spill".into(),
        }
    }
}

/// Loads `path`, anchors its relative data paths against the file's
/// directory, and installs it as the process-wide config.
pub fn init(path: &str) -> anyhow::Result<Config> {
    let cfg = anchor_paths(Config::load(path)?, path);
    let _ = CONFIG_PATH.set(path.to_string());
    *CONFIG.write().expect("config lock poisoned") = cfg.clone();
    Ok(cfg)
}

/// Snapshot of the current configuration; cheap enough to call per request.
pub fn get() -> Config {
    CONFIG.read().expect("config lock poisoned").clone()
}

/// Re-reads the config file from disk and swaps it in. Startup-only fields
/// (`db_path`, `session_path`, credentials/secret already baked into open
/// sessions and issued tokens) are re-read but have no effect until restart.
pub fn reload() -> anyhow::Result<Config> {
    let path = CONFIG_PATH
        .get()
        .map(String::as_str)
        .unwrap_or("config.toml");
    init(path)
}

/// Resolves the config's relative filesystem paths against the config
/// file's directory so the server behaves identically regardless of the
/// working directory it was started from.
fn anchor_paths(mut cfg: Config, config_path: &str) -> Config {
    let base = std::path::Path::new(config_path);
    let Some(dir) = base.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return cfg; // config in the CWD — nothing to anchor
    };
    let anchor = |p: String| -> String {
        if std::path::Path::new(&p).is_absolute() {
            p
        } else {
            dir.join(p).to_string_lossy().into_owned()
        }
    };
    cfg.session_path = anchor(cfg.session_path);
    cfg.web_dist = anchor(cfg.web_dist);
    cfg.locales_dir = anchor(cfg.locales_dir);
    cfg.spill_dir = anchor(cfg.spill_dir);
    cfg
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SizeRepr {
    Num(u64),
    Str(String),
}

pub const DEFAULT_SECRET_WARNING: &str = "config secret is the default placeholder — anyone can log in; set a real `secret` in config.toml";

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw: RawConfig = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("config file `{path}` not found, using defaults");
                toml::from_str("")?
            }
            Err(e) => return Err(e.into()),
        };

        let secret = raw.secret.unwrap_or_else(|| "change-me".to_string());
        if secret == "change-me" {
            tracing::warn!("{DEFAULT_SECRET_WARNING}");
        }

        let max_file_size = match raw.max_file_size {
            Some(SizeRepr::Num(n)) => n,
            Some(SizeRepr::Str(s)) => parse_size(&s).map_err(anyhow::Error::msg)?,
            None => 2 * 1024 * 1024 * 1024, // 2 GiB
        };

        Ok(Config {
            host: raw.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: raw.port.unwrap_or(8080),
            api_id: raw.api_id.unwrap_or(0),
            api_hash: raw.api_hash.unwrap_or_default(),
            secret,
            token_ttl_secs: raw.token_ttl_secs.unwrap_or(30 * 24 * 3600),
            db_path: raw.db_path.unwrap_or_else(|| "data/drive.surrealkv".into()),
            session_path: raw.session_path.unwrap_or_else(|| "data/session.db".into()),
            max_file_size,
            web_dist: raw.web_dist.unwrap_or_else(|| "web/dist".into()),
            locales_dir: raw.locales_dir.unwrap_or_else(|| "locales".into()),
            allowed_phones: raw
                .allowed_phones
                .unwrap_or_default()
                .iter()
                .map(|p| normalize_phone(p))
                .filter(|p| !p.is_empty())
                .collect(),
            admin_phones: raw
                .admin_phones
                .unwrap_or_default()
                .iter()
                .map(|p| normalize_phone(p))
                .filter(|p| !p.is_empty())
                .collect(),
            media_thumbs: raw.media_thumbs.unwrap_or(true),
            upload_strategy: match raw.upload_strategy.as_deref() {
                None | Some("stream") => UploadStrategy::Stream,
                Some("spill") => UploadStrategy::Spill,
                Some(other) => {
                    return Err(anyhow::anyhow!(
                        "invalid upload_strategy `{other}` (expected \"stream\" or \"spill\")"
                    ));
                }
            },
            spill_dir: raw.spill_dir.unwrap_or_else(|| "data/spill".into()),
        })
    }

    pub fn tg_configured(&self) -> bool {
        self.api_id > 0 && !self.api_hash.trim().is_empty()
    }
    /// Login gate: only phones configured here may start a Telegram login.
    /// Comparison is digit-normalized, so "+1 555 010 2030" and "+15550102030"
    pub fn phone_allowed(&self, phone: &str) -> bool {
        let want = normalize_phone(phone);
        !want.is_empty()
            && self
                .allowed_phones
                .iter()
                .any(|p| normalize_phone(p) == want)
    }
    /// Operator gate for endpoints that read or write across every tenant.
    /// The list is an explicit opt-in: an empty one (the default, and what
    /// every pre-existing config.toml has) means nobody qualifies, so a
    /// missing key can never silently hand one tenant the whole database.
    ///
    /// Keyed on the phone the account signed in with, since that is the only
    /// identifier the operator can actually look up.
    pub fn is_admin_phone(&self, phone: &str) -> bool {
        let want = normalize_phone(phone);
        !want.is_empty() && self.admin_phones.contains(&want)
    }
}

/// Strips everything but ASCII digits: "+1 (555) 010-2030" -> "15550102030".
pub(crate) fn normalize_phone(phone: &str) -> String {
    phone.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Parse a human byte size like "2GiB", "500MB", "1024KiB", "42".
/// Binary (1024-based): KiB MiB GiB TiB and bare K M G T.
/// Decimal (1000-based): KB MB GB TB.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let num_end = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(s.len());
    let (num_part, suffix_part) = s.split_at(num_end);
    let cleaned = num_part.replace('_', "");
    let value: f64 = cleaned
        .parse()
        .map_err(|_| format!("invalid size number in `{input}`"))?;
    if value < 0.0 {
        return Err(format!("size must be positive: `{input}`"));
    }

    let mult: u64 = match suffix_part.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        // Bare letters: 1024-based (common drive-usage convention).
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        "t" => 1024u64 * 1024 * 1024 * 1024,
        // Explicit "b" suffixes: SI, 1000-based.
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        "tb" => 1000 * 1000 * 1000 * 1000,
        // Explicit IEC units: 1024-based.
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "tib" => 1024u64 * 1024 * 1024 * 1024,
        other => return Err(format!("unknown size unit `{other}` in `{input}`")),
    };

    let total = value * mult as f64;
    if !total.is_finite() || total > u64::MAX as f64 {
        return Err(format!("size too large: `{input}`"));
    }
    if total.fract() != 0.0 {
        return Err(format!("size must be a whole number of bytes: `{input}`"));
    }
    Ok(total as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("42").unwrap(), 42);
        assert_eq!(parse_size("2GiB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2gib").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("2GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_size("1024KiB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("3 MB").unwrap(), 3_000_000);
        assert_eq!(parse_size("1_000").unwrap(), 1000);
        assert!(parse_size("12zb").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("-5").is_err());
        // Fractional bytes must not silently truncate; a fractional value
        // that multiplies out to whole bytes is fine.
        assert!(parse_size("2.5").is_err());
        assert!(parse_size("0.5").is_err());
        assert_eq!(parse_size("1.5KiB").unwrap(), 1536);
        assert_eq!(parse_size("2.0GiB").unwrap(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn config_defaults() {
        let cfg = Config::load("definitely-missing-config.toml").unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.max_file_size, 2 * 1024 * 1024 * 1024);
        assert!(!cfg.tg_configured());
    }

    #[test]
    fn phone_allowlist() {
        let cfg = Config {
            allowed_phones: vec!["+15550102030".into(), "447700900123".into()],
            ..Config::load("definitely-missing-config.toml").unwrap()
        };

        // Formatting differences normalize to the same digits.
        assert!(cfg.phone_allowed("+1 555 010 2030"));
        assert!(cfg.phone_allowed("15550102030"));
        assert!(cfg.phone_allowed("+44 7700 900123"));
        // Unknown or malformed input is rejected.
        assert!(!cfg.phone_allowed("+15559999999"));
        assert!(!cfg.phone_allowed(""));
        assert!(!cfg.phone_allowed("not-a-phone"));
        // Empty list allows nobody.
        let open = Config::load("definitely-missing-config.toml").unwrap();
        assert!(!open.phone_allowed("+15550102030"));
    }

    #[test]
    fn admin_allowlist() {
        let cfg = Config {
            admin_phones: vec!["15550102030".into()],
            ..Config::load("definitely-missing-config.toml").unwrap()
        };
        assert!(cfg.is_admin_phone("+15550102030"));
        // Formatting is normalized, as it is for the login allowlist: an
        // operator writes the number the way Telegram shows it.
        assert!(cfg.is_admin_phone("+1 (555) 010-2030"));
        // A signed-in tenant whose number is not listed is not an operator.
        assert!(!cfg.is_admin_phone("+447700900123"));
        // An account whose phone Telegram would not give us cannot slip
        // through on an empty string.
        assert!(!cfg.is_admin_phone(""));
        // Fail closed: the default (and every config predating the key)
        // grants nobody cross-tenant access.
        let none = Config::load("definitely-missing-config.toml").unwrap();
        assert!(none.admin_phones.is_empty());
        assert!(!none.is_admin_phone("+15550102030"));
    }
}
#[cfg(test)]
mod strategy_tests {
    use super::*;

    #[test]
    fn strategy_defaults_to_stream() {
        let cfg = Config::load("nonexistent-config.toml").unwrap();
        assert_eq!(cfg.upload_strategy, UploadStrategy::Stream);
        assert!(!cfg.spill_dir.is_empty());
    }

    #[test]
    fn strategy_accepts_both_values_and_rejects_others() {
        let dir = std::env::temp_dir().join("iidrive-strategy-test");
        std::fs::create_dir_all(&dir).unwrap();
        for (text, want) in [
            ("upload_strategy = \"stream\"", UploadStrategy::Stream),
            ("upload_strategy = \"spill\"", UploadStrategy::Spill),
        ] {
            let p = dir.join("c.toml");
            std::fs::write(&p, text).unwrap();
            assert_eq!(
                Config::load(p.to_str().unwrap()).unwrap().upload_strategy,
                want
            );
        }
        let p = dir.join("bad.toml");
        std::fs::write(&p, "upload_strategy = \"turbo\"").unwrap();
        assert!(Config::load(p.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
