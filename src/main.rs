mod app;
mod auth;
mod config;
mod db;
mod error;
mod routes;
mod state;
mod stream;
mod tg;

/// Resolves the config's relative filesystem paths against the config
/// file's directory so the server behaves identically regardless of the
/// working directory it was started from.
fn anchor_paths(mut cfg: config::Config, config_path: &str) -> config::Config {
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
    cfg.db_path = anchor(cfg.db_path);
    cfg.session_path = anchor(cfg.session_path);
    cfg.web_dist = anchor(cfg.web_dist);
    cfg
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .filter(|a| !a.starts_with('-'))
        .unwrap_or_else(|| "config.toml".to_string());

    let cfg = match config::Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load {config_path}: {e}");
            std::process::exit(1);
        }
    };
    // Relative data paths must follow the config file, not the process CWD:
    // starting the binary from another directory would otherwise create a
    // fresh empty store there and every existing file would "vanish".
    let cfg = anchor_paths(cfg, &config_path);

    let database = match db::open(&cfg.db_path).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to open database {}: {e}", cfg.db_path);
            std::process::exit(1);
        }
    };
    match db::counts(&database).await {
        Ok((files, folders)) => {
            tracing::info!(
                "database {} loaded: {files} files, {folders} folders",
                std::fs::canonicalize(&cfg.db_path)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| cfg.db_path.clone())
            );
        }
        Err(e) => tracing::warn!("could not count database rows: {e}"),
    }

    let state = app::shared_state(cfg, database);
    if let Err(e) = app::run(state).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
