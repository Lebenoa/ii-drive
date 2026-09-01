use super::{Conn, DbError};

/// The only schema version. History starts here: pre-1.0 databases must
/// reset their data directory rather than migrate.
pub(super) const SCHEMA_LATEST: u64 = 1;

pub async fn migrate(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    // Some(version) when `setting:schema` exists, None on a database that
    // has never been through `migrate()`.
    let mut res = db.query("SELECT version FROM setting:schema").await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    let recorded = rows
        .first()
        .and_then(|r| r.get("version"))
        .and_then(serde_json::Value::as_u64);
    match recorded {
        None => {
            // Fresh install: create everything the current layout needs and
            // stamp v1. Owner indexes keep per-tenant listings indexed from
            // day one instead of arriving via a migration.
            let mut res = db
                .query(
                    "DEFINE INDEX IF NOT EXISTS file_owner ON file FIELDS owner; \
                     DEFINE INDEX IF NOT EXISTS folder_owner ON folder FIELDS owner",
                )
                .await?;
            let _ = res.take::<surrealdb::types::Value>(0usize)?;
            let _ = res.take::<surrealdb::types::Value>(1usize)?;
            #[allow(clippy::as_conversions, clippy::cast_possible_wrap)]
            // Schema version is a small u64 counter; always fits i64.
            let mut stamp = db
                .query("UPSERT setting:schema SET version = $v")
                .bind(("v", SCHEMA_LATEST as i64))
                .await?;
            let _ = stamp.take::<surrealdb::types::Value>(0usize)?;
            tracing::info!("fresh database initialized at schema v{SCHEMA_LATEST}");
        }
        Some(v) if v == SCHEMA_LATEST => {}
        Some(other) => {
            return Err(DbError::Shape(format!(
                "database is at schema v{other}, but this build only speaks v{SCHEMA_LATEST}. \
                 Legacy installs must reset their data directory."
            )));
        }
    }
    Ok(SCHEMA_LATEST)
}
