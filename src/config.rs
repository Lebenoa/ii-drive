use serde::Deserialize;

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
    pub storage_chat: String,
    pub max_file_size: u64,
    pub web_dist: String,
    pub allowed_phones: Vec<String>,
    /// Generate thumbnails for videos/images with ffmpeg when available.
    pub media_thumbs: bool,
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
    storage_chat: Option<String>,
    max_file_size: Option<SizeRepr>,
    web_dist: Option<String>,
    allowed_phones: Option<Vec<String>>,
    media_thumbs: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SizeRepr {
    Num(u64),
    Str(String),
}

pub const DEFAULT_SECRET_WARNING: &str =
    "config secret is the default placeholder — anyone can log in; set a real `secret` in config.toml";

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
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
            Some(SizeRepr::Str(s)) => parse_size(&s)?,
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
            session_path: raw
                .session_path
                .unwrap_or_else(|| "data/session.db".into()),
            storage_chat: raw.storage_chat.unwrap_or_else(|| "me".into()),
            max_file_size,
            web_dist: raw.web_dist.unwrap_or_else(|| "web/dist".into()),
            allowed_phones: raw
                .allowed_phones
                .unwrap_or_default()
                .iter()
                .map(|p| normalize_phone(p))
                .filter(|p| !p.is_empty())
                .collect(),
            media_thumbs: raw.media_thumbs.unwrap_or(true),
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
}

/// Strips everything but ASCII digits: "+1 (555) 010-2030" -> "15550102030".
fn normalize_phone(phone: &str) -> String {
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
}
