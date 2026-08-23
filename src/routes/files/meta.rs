use axum::Json;

pub async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Public upload limits so clients can pre-check files before transferring.
pub async fn limits() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "max_file_size": crate::config::get().max_file_size }))
}
