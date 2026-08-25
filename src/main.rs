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

    // Config location comes from the environment, not an argument: a service
    // manager sets it once for both the wizard and the server, and there is
    // no other flag to justify an argument parser.
    let config_path = std::env::var("II_DRIVE_CONFIG")
        .ok()
        .filter(|p| !p.trim().is_empty())
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

    // First touch of the process-wide state: after `config::init`, because
    // the token signer and the Telegram hub read their settings out of it,
    // and inside the runtime, because the hub spawns its login pruner.
    let state = state::get();
    if let Err(e) = db::connect(&state.db, &cfg.db_path).await {
        eprintln!("failed to open database {}: {e}", cfg.db_path);
        std::process::exit(1);
    }
    // Before anything can serve a request, so no upload is ever checked
    // against the placeholder cap the lazy state had to start with.
    if let Err(e) = state.hydrate_instance().await {
        eprintln!("failed to load instance settings: {e}");
        std::process::exit(1);
    }
    match db::counts(&state.db).await {
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

    // Orphaned previews (crash between row delete and file unlink) are
    // swept at startup and then hourly: a long-running server never gets
    // the boot-time cleanup for free.
    tokio::spawn(async {
        let state = crate::state::get();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            ticker.tick().await;
            match crate::routes::sweep(state).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("thumbnail sweep removed {n} orphans"),
                Err(e) => tracing::warn!("thumbnail sweep failed: {e}"),
            }
        }
    });

    if let Err(e) = app::run().await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
