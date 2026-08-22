mod app;
mod auth;
mod config;
mod db;
mod error;
mod routes;
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

    let cfg = match config::Config::load(&config_path) {
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

    let state = app::shared_state(cfg, database);
    if let Err(e) = app::run(state).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
