use axum::Router;
use axum::routing::{delete, get, post};
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let public = Router::new()
        .route("/health", get(crate::routes::health))
        .route("/api/limits", get(crate::routes::limits))
        .route(
            "/locales/manifest.json",
            get(crate::routes::locale_manifest),
        )
        .route("/locales/{file}", get(crate::routes::locale_file))
        .route("/api/auth/phone", post(crate::routes::auth_phone))
        .route("/api/auth/code", post(crate::routes::auth_code))
        .route("/api/auth/password", post(crate::routes::auth_password))
        .route("/api/files/{id}/raw", get(crate::routes::raw_file))
        .route("/api/files/{id}/thumb", get(crate::routes::file_thumb))
        .route("/api/files/{id}/link", get(crate::routes::file_link));

    let protected = Router::new()
        .route("/api/me", get(crate::routes::me))
        .route("/api/auth/logout", post(crate::routes::auth_logout))
        .route("/api/avatar", get(crate::routes::avatar))
        .route("/api/media-token", get(crate::routes::media_token))
        .route("/api/botfather", post(crate::routes::botfather_send))
        .route("/api/botfather/bots", get(crate::routes::botfather_bots))
        .route("/api/botfather/token", post(crate::routes::botfather_token))
        .route(
            "/api/botfather/draft",
            get(crate::routes::botfather_draft).delete(crate::routes::botfather_cancel),
        )
        .route(
            "/api/channels",
            get(crate::routes::list_channels).post(crate::routes::select_channels),
        )
        .route("/api/channels/create", post(crate::routes::create_channel))
        .route(
            "/api/bot",
            get(crate::routes::list_bots).post(crate::routes::add_bot),
        )
        .route("/api/bot/{id}", delete(crate::routes::remove_bot))
        .route(
            "/api/settings",
            get(crate::routes::get_settings).put(crate::routes::save_settings),
        )
        .route("/api/config/reload", post(crate::routes::reload_config))
        .route(
            "/api/internal-db/tables",
            get(crate::routes::internal_db_tables),
        )
        .route(
            "/api/internal-db/query",
            post(crate::routes::internal_db_query),
        )
        .route(
            "/api/rules",
            get(crate::routes::get_rules).put(crate::routes::save_rules),
        )
        .route(
            "/api/files",
            get(crate::routes::list_files)
                .post(crate::routes::upload_file)
                // The upload cap is runtime-reloadable, so no static body
                // limit here: the guard reads the live config per request,
                // and axum's own default cap is disabled for this route
                // (the handler still enforces the limit while streaming).
                .layer(axum::extract::DefaultBodyLimit::disable())
                .layer(axum::middleware::from_fn(crate::routes::upload_limit)),
        )
        .route("/api/files/upload/init", post(crate::routes::upload_init))
        .route(
            "/api/files/upload/{id}",
            get(crate::routes::upload_status)
                .delete(crate::routes::upload_abort)
                .put(crate::routes::upload_chunk)
                // Chunks are buffered whole in RAM, so the default-limit
                // slot stays bounded even though the session's declared
                // total can be gigabytes.
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/files/upload/{id}/complete",
            post(crate::routes::upload_complete),
        )
        .route(
            "/api/folders",
            get(crate::routes::list_folders).post(crate::routes::create_folder),
        )
        .route("/api/folders/{id}", delete(crate::routes::delete_folder))
        .route("/api/files/{id}", delete(crate::routes::delete_file))
        .route(
            "/api/files/{id}/visibility",
            axum::routing::patch(crate::routes::set_visibility),
        )
        .route(
            "/api/files/{id}/move",
            axum::routing::patch(crate::routes::move_file),
        )
        .route("/api/files/upload-bench", post(crate::routes::upload_bench))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::guard,
        ));

    let web_dist = crate::config::get().web_dist;
    let app: Router<()> = public.merge(protected).with_state(state);
    let dist = std::path::Path::new(&web_dist);
    if dist.is_dir() {
        let index = dist.join("index.html");
        let spa = ServeDir::new(dist).fallback(ServeFile::new(index));
        app.fallback_service(spa)
    } else {
        tracing::warn!(
            "web_dist `{}` not found — API-only mode (run `nub run build` in web/)",
            web_dist
        );
        app.fallback(|| async {
            axum::response::Html(
                "<h1>ii-drive</h1><p>Web UI not built. Run <code>nub install &amp;&amp; nub run build</code> in <code>web/</code>.</p>",
            )
        })
    }
}

pub async fn run(state: AppState) -> std::io::Result<()> {
    let mut accounts = state.hub.restore().await;
    tracing::info!("{} signed-in account(s)", accounts.len());
    for uid in &accounts {
        let Some(tg) = state.hub.get(*uid).await else {
            continue;
        };
        match crate::db::get_bots(&state.db, *uid).await {
            Ok(bots) => {
                for b in bots {
                    if let Err(e) = tg.configure_bot(&b.token).await {
                        tracing::warn!("bot @{} failed to sign back in: {e}", b.username);
                    }
                }
            }
            Err(e) => tracing::warn!("could not load bot settings for {uid}: {e}"),
        }
        // Warm the connection so the first /api/me does not race the
        // MTProto handshake and report a spurious "not authorized" (which
        // the web client treats as a logout).
        tokio::spawn(async move {
            let _ = tg.status().await;
        });
    }

    let cfg = crate::config::get();
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await
}

pub fn shared_state(db: surrealdb::Surreal<surrealdb::engine::local::Db>) -> AppState {
    let cfg = crate::config::get();
    AppState::new(
        db,
        crate::auth::Tokens::new(&cfg.secret, cfg.token_ttl_secs),
        crate::tg::TgHub::new(cfg),
    )
}
