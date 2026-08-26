use color_eyre::Result;
use std::sync::{LazyLock, OnceLock};

use serde::Deserialize;

/// Process-wide configuration, loaded once by [`init`] and read through
/// [`get`] for the rest of the process' life.
///
/// Everything here is a value an operator sets once and forgets: Telegram
/// credentials, filesystem paths, the phone allowlists. There is deliberately
/// nothing worth re-reading at runtime — tunables that do get revisited live
/// in the database as [`crate::db::Instance`], where changing one is a
/// request rather than an edit-and-restart.
static CONFIG: OnceLock<Config> = OnceLock::new();

/// What [`get`] answers before [`init`] has run. Only test binaries ever see
/// it: keeping it in its own cell means a read that lands early cannot fill
/// `CONFIG` with defaults and leave the real load with nowhere to go.
static DEFAULTS: LazyLock<Config> = LazyLock::new(Config::default);

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
    /// Built SPA folder. Ships with the binary rather than the config, so it
    /// is located by [`asset_root`] and is not a config key.
    pub web_dist: String,
    /// Folder of translation files (`{lang}.json`) served to the web UI,
    /// which downloads a language only after the user picks it. Located
    /// beside the binary, like [`Self::web_dist`].
    pub locales_dir: String,
    pub allowed_phones: Vec<String>,
    /// Phone numbers allowed to reach operator-only endpoints (raw-SQL
    /// browser, instance settings). Phone rather than Telegram user id
    /// because Telegram never shows a user their own numeric id, so an id
    /// would be a setting nobody can fill in unaided.
    ///
    /// Deliberately a file setting and not a database one: `/internal-db`
    /// runs unrestricted `SurrealQL`, so an admin list living in the store
    /// would be a list admins can extend. Keeping it in a file the server
    /// only ever reads means granting operator rights takes filesystem
    /// access, not a session.
    pub admin_phones: Vec<String>,
    /// Directory for in-flight upload buffers (every upload is buffered here
    /// before its parts fan out; the resumable-upload sessions, which always
    /// spill by design).
    pub spill_dir: String,
    /// At-rest encryption of uploaded files (teldrive-compatible format).
    /// When enabled, every new upload is sealed with the key derived from
    /// [`Self::crypt_password`] and [`Self::crypt_salt`]. Files uploaded
    /// while this was off stay plaintext and are served as-is (detected by
    /// the absent container magic). Enabling it does not rewrite old
    /// uploads.
    pub crypt_enabled: bool,
    /// Operator password feeding key derivation (scrypt). Required when
    /// [`Self::crypt_enabled`] is on; ignored otherwise. Changing it makes
    /// previously encrypted uploads unreadable — treat it as permanent
    /// once files are stored.
    pub crypt_password: String,
    /// Salt for key derivation. Like the password, must never change after
    /// files are stored. Defaults to "ii-drive".
    pub crypt_salt: String,
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
    // Removed keys, still parsed so a config that sets them gets told why
    // they stopped having an effect instead of silently losing them.
    web_dist: Option<String>,
    locales_dir: Option<String>,
    allowed_phones: Option<Vec<String>>,
    admin_phones: Option<Vec<String>>,
    spill_dir: Option<String>,
    crypt_enabled: Option<bool>,
    crypt_password: Option<String>,
    crypt_salt: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            api_id: 0,
            api_hash: String::new(),
            secret: "change-me".into(),
            token_ttl_secs: 30 * 24 * 3600,
            db_path: "data/drive.surrealkv".into(),
            session_path: "data/session.db".into(),
            web_dist: asset_path("web/dist"),
            locales_dir: asset_path("locales"),
            allowed_phones: Vec::new(),
            admin_phones: Vec::new(),
            spill_dir: "data/spill".into(),
            crypt_enabled: false,
            crypt_password: String::new(),
            crypt_salt: "ii-drive".into(),
        }
    }
}

/// Loads `path`, anchors its relative data paths against the file's
/// directory, and installs it as the process-wide config.
pub fn init(path: &str) -> color_eyre::Result<&'static Config> {
    let mut cfg = anchor_paths(Config::load(path)?, path);
    resolve_secret(&mut cfg);
    CONFIG
        .set(cfg)
        .map_err(|_| color_eyre::eyre::eyre!("configuration is already initialized"))?;
    Ok(get())
}

/// Secret values that must never reach production token signing: the
/// built-in default, the example-config suggestion, and empty.
fn is_placeholder_secret(s: &str) -> bool {
    s.is_empty() || s == "change-me" || s == "change-me-to-a-long-random-string"
}

/// Replaces a missing or placeholder `secret` with a random one persisted
/// beside the database, so users get safe sessions without hand-editing
/// crypto material — and the value survives restarts. An explicitly set
/// short secret is honored but flagged: overriding a deliberate choice is
/// worse than warning about it.
fn resolve_secret(cfg: &mut Config) {
    if !is_placeholder_secret(&cfg.secret) {
        if cfg.secret.len() < 32 {
            tracing::warn!("config secret is shorter than 32 chars — use a long random string");
        }
        return;
    }

    let dir = std::path::Path::new(&cfg.db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let file = dir.join("secret.key");
    if let Ok(existing) = std::fs::read_to_string(&file) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 {
            cfg.secret = trimmed.to_string();
            tracing::info!("using generated session secret from {}", file.display());
            return;
        }
    }

    let secret: [u8; 32] = rand::random();
    let hex_secret = hex::encode(secret);
    if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&file, &hex_secret).is_ok() {
        tracing::info!("generated session secret, stored at {}", file.display());
    } else {
        // Not persisted: sessions reset on restart until the user fixes
        // permissions or sets `secret` manually. Still better than a
        // publicly-known placeholder.
        tracing::warn!(
            "could not write {}: session secret will change on restart",
            file.display()
        );
    }
    cfg.secret = hex_secret;
}

/// The process-wide configuration. Immutable once [`init`] has run, so this
/// hands out a borrow rather than a snapshot: call it as freely as you like.
///
/// Defaults stand in when `init` never ran, which only happens in test
/// binaries — production `main` initializes before touching anything else.
pub fn get() -> &'static Config {
    CONFIG.get().unwrap_or(&DEFAULTS)
}

/// Resolves one relative path against the config file's directory, so the
/// server finds the same files regardless of the working directory it was
/// started from. Absolute paths and a config in the CWD pass through.
fn anchored(config_path: &str, p: &str) -> String {
    let dir = std::path::Path::new(config_path)
        .parent()
        .filter(|d| !d.as_os_str().is_empty());
    match dir {
        Some(dir) if !std::path::Path::new(p).is_absolute() => {
            dir.join(p).to_string_lossy().into_owned()
        }
        _ => p.to_string(),
    }
}

/// Applies [`anchored`] to the data paths the config carries. The shipped
/// assets are deliberately absent: they travel with the binary, not with the
/// operator's data, and [`asset_root`] locates them.
fn anchor_paths(mut cfg: Config, config_path: &str) -> Config {
    // `db_path` matters most: unanchored, starting the binary from another
    // directory creates a fresh empty store there and every existing file
    // appears to vanish. `secret.key` is written beside it, so sessions
    // followed the same wrong directory.
    cfg.db_path = anchored(config_path, &cfg.db_path);
    cfg.session_path = anchored(config_path, &cfg.session_path);
    cfg.spill_dir = anchored(config_path, &cfg.spill_dir);
    cfg
}

/// Directory holding the assets that ship with the server: the built SPA and
/// the translation files. A release bundle is the executable with both
/// folders beside it, so they are found from the executable and need no
/// configuring — unlike the data paths, they are not the operator's files.
///
/// Debug builds answer with the source tree instead: `target/debug` has no
/// `web/dist` next to it, so `cargo run` would otherwise serve nothing.
fn asset_root() -> std::path::PathBuf {
    #[cfg(debug_assertions)]
    {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
    #[cfg(not(debug_assertions))]
    {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| ".".into())
    }
}

/// One shipped asset folder, resolved against [`asset_root`].
fn asset_path(rel: &str) -> String {
    asset_root().join(rel).to_string_lossy().into_owned()
}

/// A byte size written either as a plain number of bytes or as a human
/// string like `"2GiB"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SizeRepr {
    Num(u64),
    Str(String),
}

impl SizeRepr {
    pub(crate) fn bytes(&self) -> Result<u64, String> {
        match self {
            Self::Num(n) => Ok(*n),
            Self::Str(s) => parse_size(s),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> color_eyre::Result<Self> {
        let raw: RawConfig = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("config file `{path}` not found, using defaults");
                toml::from_str("")?
            }
            Err(e) => return Err(e.into()),
        };

        // A placeholder secret is replaced by a generated, persisted one in
        // `resolve_secret` (init/reload); user-set secrets pass through and
        // only get a warning there if they look weak.

        // Removed keys, still parsed so a config that sets them gets told
        // why they stopped having an effect instead of silently losing them.
        for (key, set) in [
            ("web_dist", raw.web_dist.is_some()),
            ("locales_dir", raw.locales_dir.is_some()),
        ] {
            if set {
                tracing::warn!(
                    "ignoring config key `{key}`: the web UI and translations ship with the \
                     binary and are located beside it — delete this key"
                );
            }
        }

        Ok(Self {
            host: raw.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: raw.port.unwrap_or(8080),
            api_id: raw.api_id.unwrap_or(0),
            api_hash: raw.api_hash.unwrap_or_default(),
            // resolve_secret (init/reload) swaps this placeholder for a
            // generated, persisted value.
            secret: raw.secret.unwrap_or_else(|| "change-me".into()),
            token_ttl_secs: raw.token_ttl_secs.unwrap_or(30 * 24 * 3600),
            db_path: raw.db_path.unwrap_or_else(|| "data/drive.surrealkv".into()),
            session_path: raw.session_path.unwrap_or_else(|| "data/session.db".into()),
            web_dist: asset_path("web/dist"),
            locales_dir: asset_path("locales"),
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
            spill_dir: raw.spill_dir.unwrap_or_else(|| "data/spill".into()),
            crypt_enabled: raw.crypt_enabled.unwrap_or(false),
            crypt_password: raw.crypt_password.unwrap_or_default(),
            crypt_salt: raw.crypt_salt.unwrap_or_else(|| "ii-drive".into()),
        })
    }
    /// The key used for at-rest encryption of uploads, derived from the
    /// configured password and salt. Returns `None` while encryption is
    /// disabled. A missing password with encryption enabled is a
    /// misconfiguration: uploads would produce unreadable containers, so
    /// this surfaces it as an error instead.
    pub fn crypt_key(&self) -> color_eyre::Result<Option<crate::crypt::Key>> {
        if !self.crypt_enabled {
            return Ok(None);
        }
        if self.crypt_password.is_empty() {
            return Err(color_eyre::eyre::eyre!(
                "crypt_enabled requires a non-empty crypt_password"
            ));
        }
        Ok(Some(crate::crypt::derive_key(
            &self.crypt_password,
            &self.crypt_salt,
        )))
    }
    /// The key for decrypting stored files, derived whenever the password
    /// and salt are configured — regardless of the upload toggle. Decoding
    /// a stored encrypted part must not depend on whether encryption is
    /// currently enabled for new uploads.
    pub fn crypt_key_unconditional(&self) -> Option<crate::crypt::Key> {
        if self.crypt_password.is_empty() {
            return None;
        }
        Some(crate::crypt::derive_key(&self.crypt_password, &self.crypt_salt))
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
pub fn normalize_phone(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

/// Parse a human byte size like "2GiB", "500MB", "1024KiB", "42".
/// Binary (1024-based): KiB MiB GiB TiB and bare K M G T.
/// Decimal (1000-based): KB MB GB TB.
#[allow(
    clippy::as_conversions,           // f64 size math: fractional inputs like "2.5GiB"
    clippy::cast_sign_loss,           // total is range-checked > u64::MAX before the cast
    clippy::cast_precision_loss,      // mult max ~2^40 fits f64's 52-bit mantissa exactly
    clippy::cast_possible_truncation, // guarded by the fract() and > u64::MAX checks
)]
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
        // Bare letters and IEC units are 1024-based; the "b" forms are SI.
        "k" | "kib" => 1024,
        "m" | "mib" => 1024 * 1024,
        "g" | "gib" => 1024 * 1024 * 1024,
        "t" | "tib" => 1024u64 * 1024 * 1024 * 1024,
        // Explicit "b" suffixes: SI, 1000-based.
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        "tb" => 1000 * 1000 * 1000 * 1000,
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
        assert!(!cfg.tg_configured());
    }

    /// Every relative data path follows the config file, not the working
    /// directory. `db_path` was the one omission, and it is the expensive
    /// one: launching from elsewhere silently created a second, empty store
    /// and the drive looked wiped.
    #[test]
    fn relative_data_paths_follow_the_config_file() {
        let cfg = anchor_paths(Config::default(), "/srv/ii-drive/config.toml");
        let base = std::path::Path::new("/srv/ii-drive");

        for (name, got) in [
            ("db_path", &cfg.db_path),
            ("session_path", &cfg.session_path),
            ("spill_dir", &cfg.spill_dir),
        ] {
            assert!(
                std::path::Path::new(got).starts_with(base),
                "{name} was left relative to the working directory: {got}"
            );
        }

        // The assets are the binary's, not the operator's. Moving a config
        // must not move the UI out from under a running install.
        for (name, got) in [
            ("web_dist", &cfg.web_dist),
            ("locales_dir", &cfg.locales_dir),
        ] {
            assert!(
                !std::path::Path::new(got).starts_with(base),
                "{name} followed the config file: {got}"
            );
        }
    }

    /// The assets are found with no config key and no working directory: a
    /// release bundle carries them beside the executable, and this build
    /// carries them in the source tree.
    #[test]
    fn shipped_assets_are_located_from_the_binary() {
        let cfg = Config::load("definitely-missing-config.toml").unwrap();
        for (name, got) in [
            ("web_dist", &cfg.web_dist),
            ("locales_dir", &cfg.locales_dir),
        ] {
            let p = std::path::Path::new(got);
            assert!(p.is_absolute(), "{name} is not absolute: {got}");
            assert!(
                p.starts_with(asset_root()),
                "{name} escaped the asset root: {got}"
            );
        }
        // `cargo test` is a debug build, so the root is this repository and
        // the dictionary that ships with it must really be there.
        assert!(
            std::path::Path::new(&cfg.locales_dir)
                .join("en.json")
                .is_file(),
            "debug builds must resolve assets to the source tree"
        );
    }

    /// An absolute path is the operator being explicit; a config in the
    /// working directory has nothing to anchor against.
    #[test]
    fn anchoring_leaves_absolute_paths_and_bare_config_names_alone() {
        assert_eq!(
            anchored("/srv/ii-drive/config.toml", "/var/lib/ii-drive/db"),
            "/var/lib/ii-drive/db"
        );
        assert_eq!(
            anchored("config.toml", "data/drive.surrealkv"),
            "data/drive.surrealkv"
        );
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

    /// A file that still points `web_dist`/`locales_dir` somewhere custom is
    /// warned about and ignored — it must not send the server looking for a
    /// UI the bundle never puts there.
    #[test]
    fn removed_asset_keys_do_not_move_the_assets() {
        let dir = std::env::temp_dir().join("iidrive-asset-key-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("stale-assets.toml");
        std::fs::write(
            &p,
            "web_dist = \"/nowhere/ui\"\nlocales_dir = \"/nowhere/i18n\"\n",
        )
        .unwrap();
        let cfg = Config::load(p.to_str().unwrap()).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(cfg.web_dist, asset_path("web/dist"));
        assert_eq!(cfg.locales_dir, asset_path("locales"));
    }
}
