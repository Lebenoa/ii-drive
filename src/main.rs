mod app;
mod art;
mod auth;
mod config;
mod db;
mod error;
mod routes;
mod setup;
mod state;
mod stream;
mod tg;

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

    // First run on this data directory: no config yet — serve the setup
    // wizard, which writes the file and exits. Starting again then boots
    // the real server.
    if !std::path::Path::new(&config_path).exists() {
        tracing::info!("`{config_path}` not found — starting first-run setup wizard");
        if let Err(e) = setup::run(std::path::PathBuf::from(&config_path)).await {
            eprintln!("setup wizard error: {e}");
            std::process::exit(1);
        }
        println!();
        println!("Setup complete. Start ii-drive again to launch your drive.");
        std::process::exit(0);
    }

    // Relative data paths must follow the config file, not the process CWD:
    // starting the binary from another directory would otherwise create a
    // fresh empty store there and every existing file would "vanish".
    let cfg = match config::init(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load {config_path}: {e}");
            std::process::exit(1);
        }
    };

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

    let state = app::shared_state(database);
    if let Err(e) = app::run(state).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
