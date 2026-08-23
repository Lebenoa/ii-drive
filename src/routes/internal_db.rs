use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// GET /api/internal-db/tables — table names in the embedded store.
pub async fn tables(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let mut res = state
        .db
        .query("INFO FOR DB")
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let rows: Vec<serde_json::Value> = res
        .take(0)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let info = rows.into_iter().next().unwrap_or(serde_json::Value::Null);
    let mut names: Vec<String> = info["tables"]
        .as_object()
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    Ok(Json(json!({ "tables": names })))
}

#[derive(Deserialize)]
pub struct QueryBody {
    pub sql: String,
}

/// POST /api/internal-db/query — run raw SurrealQL against the embedded
/// store and return every statement's result. Internal admin tooling: the
/// bearer guard keeps it owner-only, but the queries themselves are
/// unrestricted by design.
pub async fn query(
    State(state): State<AppState>,
    Json(body): Json<QueryBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let sql = body.sql.trim();
    if sql.is_empty() {
        return Err(ApiError::bad_request("query must not be empty"));
    }
    if sql.len() > 16 * 1024 {
        return Err(ApiError::bad_request("query too long"));
    }
    let mut res = state
        .db
        .query(sql)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let n = res.num_statements();
    let mut results = Vec::with_capacity(n);
    for i in 0..n {
        // Each statement yields its rows (or scalar) as generic JSON;
        // statement-level errors surface the same way.
        match res.take::<Vec<serde_json::Value>>(i) {
            Ok(rows) => results.push(json!({ "ok": true, "result": rows })),
            Err(e) => results.push(json!({ "ok": false, "error": e.to_string() })),
        }
    }
    Ok(Json(json!({ "results": results })))
}
