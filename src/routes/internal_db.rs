use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Refuses anyone the config does not name an operator. 404 rather than
/// 403: a tenant who is not an operator learns nothing about the endpoint
/// existing, so it cannot be probed from an ordinary account.
async fn admin_only(state: &AppState, uid: i64) -> Result<(), ApiError> {
    if state.is_admin(uid).await {
        Ok(())
    } else {
        Err(ApiError::not_found("not found"))
    }
}

/// GET /api/internal-db/tables — table names in the embedded store.
///
/// Operator-only, gated on `admin_phones`: the schema is process-wide, so
/// it names every tenant's tables regardless of who asks.
pub async fn tables(
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    admin_only(state, uid).await?;
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
/// store and return every statement's result. The queries are unrestricted
/// by design, which means they read and write across EVERY tenant: `file`
/// rows with their `chat`/`message_id`, and `setting:bots_*` which holds
/// plaintext bot tokens. The bearer guard alone is therefore not a
/// sufficient boundary — it only proves *some* account is signed in — so
/// this is operator-only, gated on `admin_phones`.
pub async fn query(
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<QueryBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    admin_only(state, uid).await?;
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
        crate::db::open_mem().await
    }

    /// Bad SQL must fail via Response::check (surrealdb 3 yields
    /// Bool(false) from take() for errored statements), while scalar,
    /// info and empty-select shapes deserialize as arrays.
    #[tokio::test]
    async fn statement_shapes() -> Result<(), DbError> {
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

    /// The handlers themselves are not exercised: `admin_only` resolves the
    /// caller's phone through the hub and reads the process-wide
    /// `config::get()` static, which every test in the binary shares, so
    /// mutating it here would race sibling tests. The gate is one
    /// `is_admin_phone` call over that phone, so testing the predicate
    /// covers the decision.
    #[test]
    fn admin_gate_decides_by_config() {
        let base = crate::config::Config::load("definitely-missing-config.toml").unwrap();
        let cfg = crate::config::Config {
            admin_phones: vec!["15550102030".into()],
            ..base.clone()
        };
        assert!(
            cfg.is_admin_phone("+15550102030"),
            "listed operator gets through"
        );
        // A signed-in tenant that is not listed must be refused, even though
        // the bearer guard already accepted its token.
        assert!(!cfg.is_admin_phone("+447700900123"));
        // Empty list (the default) denies everyone.
        assert!(base.admin_phones.is_empty());
        assert!(!base.is_admin_phone("+15550102030"));
    }
}
