use color_eyre::eyre::WrapErr;

mod app;
mod art;
mod auth;
mod config;
mod crypt;
mod db;
mod error;
mod routes;
mod setup;
mod state;
mod stream;
mod tg;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    // Fancy error reports for setup/config failures: the whole point of
    // color-eyre over plain anyhow-style strings.
    color_eyre::install()?;
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
        setup::run(std::path::PathBuf::from(&config_path)).await?;
        println!();
        println!("Setup complete. Start ii-drive again to launch your drive.");
        std::process::exit(0);
    }

    // Relative data paths must follow the config file, not the process CWD:
    // starting the binary from another directory would otherwise create a
    // fresh empty store there and every existing file would "vanish".
    let cfg =
        config::init(&config_path).wrap_err_with(|| format!("failed to load `{config_path}`"))?;

    // First touch of the process-wide state: after `config::init`, because
    // the token signer and the Telegram hub read their settings out of it,
    // and inside the runtime, because the hub spawns its login pruner.
    let state = state::get();
    db::connect(&state.db, &cfg.db_path)
        .await
        .wrap_err_with(|| format!("failed to open database `{}`", cfg.db_path))?;
    // The Telegram hub holds its own clone of the handle, and namespace
    // selection does not carry across clones — see `db::attach_session`.
    state
        .hub
        .attach_session()
        .await
        .map_err(color_eyre::Report::msg)?;

    // Before anything can serve a request, so no upload is ever checked
    // against the placeholder cap the lazy state had to start with.
    state
        .hydrate_instance()
        .await
        .wrap_err("failed to load instance settings")?;
    match db::counts(&state.db).await {
        Ok((files, folders)) => {
            tracing::info!(
                "database {} loaded: {files} files, {folders} folders",
                std::fs::canonicalize(&cfg.db_path)
                    .map_or_else(|_| cfg.db_path.clone(), |p| p.display().to_string())
            );
        }
        Err(e) => tracing::warn!("could not count database rows: {e}"),
    }

    // Orphaned previews (crash between row delete and file unlink) are
    // swept once at startup and then on the operator's schedule — an anchor
    // wall-clock time plus an interval in hours ("00:00" every 24 h runs
    // nightly at midnight). The schedule re-reads every cycle, so changes
    // apply without a restart; a disabled or invalid schedule polls the
    // setting each minute and resumes sweeping when it becomes valid.
    tokio::spawn(async {
        let state = crate::state::get();
        match crate::routes::sweep(state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("startup thumbnail sweep removed {n} orphans"),
            Err(e) => tracing::warn!("startup thumbnail sweep failed: {e}"),
        }
        loop {
            let inst = state.instance();
            let Some(delay) =
                crate::routes::next_sweep_in(&inst.thumb_sweep_time, inst.thumb_sweep_hours)
            else {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            };
            drop(inst);
            tokio::time::sleep(delay).await;
            match crate::routes::sweep(state).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("thumbnail sweep removed {n} orphans"),
                Err(e) => tracing::warn!("thumbnail sweep failed: {e}"),
            }
        }
    });

    app::run()
        .await
        .wrap_err("failed to bind or serve the web server")?;

    Ok(())
}
