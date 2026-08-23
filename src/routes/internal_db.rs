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
    // surrealdb 3 does not fail take() on statement-level errors — it
    // yields Bool(false) — so collect per-statement errors up front
    // (take_errors borrows, unlike the consuming check()).
    let errors = res.take_errors();
    if let Some((_, e)) = errors.iter().next() {
        return Err(ApiError::bad_request(e.to_string()));
    }
    let n = res.num_statements();
    let mut results = Vec::with_capacity(n);
    for i in 0..n {
        match res.take::<Vec<serde_json::Value>>(i) {
            Ok(rows) => results.push(json!({ "ok": true, "result": rows })),
            Err(e) => results.push(json!({ "ok": false, "error": e.to_string() })),
        }
    }
    Ok(Json(json!({ "results": results })))
}

#[cfg(test)]
mod tests {
    use crate::db::DbError;

    async fn test_db() -> Result<surrealdb::Surreal<surrealdb::engine::local::Db>, DbError> {
        let dir = tempfile::tempdir()?;
        let db = surrealdb::Surreal::new::<surrealdb::engine::local::SurrealKv>(
            dir.path().join("t.db"),
        )
        .await?;
        db.use_ns("drive").await?;
        db.use_db("drive").await?;
        Ok(db)
    }

    /// Bad SQL must fail via Response::check (surrealdb 3 yields
    /// Bool(false) from take() for errored statements), while scalar,
    /// info and empty-select shapes deserialize as arrays.
    #[tokio::test]
    async fn statement_shapes() -> Result<(), DbError> {
        // This test opens its own engine; share the DB test budget.
        let _engine = crate::db::harness::acquire();
        let db = test_db().await?;
        db.query("DEFINE TABLE IF NOT EXISTS file").await?;

        // RETURN/INFO always yield a row; a fresh table's SELECT yields an
        // empty array — take() must still succeed on it.
        let mut r = db.query("RETURN 1").await?;
        assert!(r.take_errors().is_empty());
        assert!(!r.take::<Vec<serde_json::Value>>(0)?.is_empty());

        let mut r = db.query("INFO FOR DB").await?;
        assert!(r.take_errors().is_empty());
        let info: Vec<serde_json::Value> = r.take(0)?;
        assert!(info[0]["tables"].is_object());

        let mut r = db.query("SELECT * FROM file LIMIT 5").await?;
        assert!(r.take_errors().is_empty());
        let rows: Vec<serde_json::Value> = r.take(0)?;
        assert_eq!(rows.len(), 0);

        // Parse errors fail db.query() itself…
        assert!(db.query("SELEC * FROM file").await.is_err());

        // …while statement-level runtime errors (e.g. selecting an
        // undefined table) surface via take_errors — take() alone would
        // hand back a bogus Bool(false).
        let mut r = db.query("SELECT * FROM missing_table").await?;
        assert!(!r.take_errors().is_empty(), "runtime error must surface");
        Ok(())
    }
}
