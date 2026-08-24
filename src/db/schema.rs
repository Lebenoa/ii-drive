use super::{bots, settings, Conn, DbError, UNOWNED};

/// Latest schema version; a database without a recorded version is v1.
pub(super) const SCHEMA_LATEST: u64 = 5;

pub(super) async fn schema_version(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    Ok(schema_version_recorded(db).await?.unwrap_or(1))
}

pub(super) async fn set_schema_version(
    db: &surrealdb::Surreal<Conn>,
    v: u64,
) -> Result<(), DbError> {
    let mut res = db
        .query("UPSERT setting:schema SET version = $v")
        .bind(("v", v as i64))
        .await?;
    let _ = res.take::<surrealdb::types::Value>(0usize)?;
    Ok(())
}

/// Some(version) when `setting:schema` exists, None on a database that has
/// never been through migrate().
async fn schema_version_recorded(
    db: &surrealdb::Surreal<Conn>,
) -> Result<Option<u64>, DbError> {
    let mut res = db.query("SELECT version FROM setting:schema").await?;
    let rows: Vec<serde_json::Value> = res.take(0)?;
    Ok(rows
        .first()
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_u64()))
}

/// True when the store holds no user data at all (fresh install). An old
/// pre-versioning database with rows is NOT virgin and must migrate.
async fn store_is_virgin(db: &surrealdb::Surreal<Conn>) -> Result<bool, DbError> {
    let mut res = db
        .query("SELECT count() AS n FROM file GROUP ALL; SELECT count() AS n FROM folder GROUP ALL")
        .await?;
    let files: Vec<serde_json::Value> = res.take(0)?;
    let folders: Vec<serde_json::Value> = res.take(1)?;
    let n = |rows: &[serde_json::Value]| {
        rows.first()
            .and_then(|r| r.get("n"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    Ok(n(&files) + n(&folders) == 0)
}

/// Runs every migration above the stored version, in order, recording each
/// one as it lands. Append a new `v == N` arm (and bump SCHEMA_LATEST)
/// to add a migration; never renumber or edit shipped steps.
///
/// A brand-new store (no schema record and no rows at all) is stamped at
/// the latest version directly — migrations only exist for old data.
pub async fn migrate(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    if schema_version_recorded(db).await?.is_none() && store_is_virgin(db).await? {
        set_schema_version(db, SCHEMA_LATEST).await?;
        tracing::info!("fresh database initialized at schema v{SCHEMA_LATEST}");
        return Ok(SCHEMA_LATEST);
    }
    let mut v = schema_version(db).await?;
    while v < SCHEMA_LATEST {
        v += 1;
        match v {
            2 => {
                // folder becomes non-optional: drop rows from before the
                // feature. Their Telegram messages are NOT deleted.
                let dropped = purge_legacy_rows(db).await?;
                if dropped > 0 {
                    tracing::warn!(
                        "migration v2: dropped {dropped} pre-folder file rows \
                         (messages stay in Telegram)"
                    );
                }
            }
            3 => {
                // files become private by default: every row without an
                // explicit visibility is locked down.
                let mut res = db
                    .query("UPDATE file SET public = false WHERE public IS NONE RETURN AFTER")
                    .await?;
                let updated: Vec<serde_json::Value> = res.take(0)?;
                if !updated.is_empty() {
                    tracing::warn!(
                        "migration v3: {} files defaulted to private",
                        updated.len()
                    );
                }
            }
            4 => {
                // Every row gains an owner. Rows that predate multi-tenancy
                // are stamped UNOWNED rather than guessed at, so a uniform
                // `owner = $owner` filter never leaks them and
                // adopt_unowned() can hand them to the account that owns
                // the legacy session. The legacy `setting:bots` pool needs
                // no stamp — it is already addressed by its own id.
                let mut res = db
                    .query(
                        "UPDATE file SET owner = $u WHERE owner IS NONE RETURN AFTER; \
                         UPDATE folder SET owner = $u WHERE owner IS NONE RETURN AFTER",
                    )
                    .bind(("u", UNOWNED))
                    .await?;
                let files: Vec<serde_json::Value> = res.take(0)?;
                let folders: Vec<serde_json::Value> = res.take(1)?;
                if !files.is_empty() || !folders.is_empty() {
                    tracing::warn!(
                        "migration v4: {} files and {} folders await adoption",
                        files.len(),
                        folders.len()
                    );
                }
            }
            5 => {
                // Every listing filters on `owner`. Without an index each
                // one scans every tenant's rows, so one account's drive
                // gets slower as unrelated accounts add files.
                let mut res = db
                    .query(
                        "DEFINE INDEX IF NOT EXISTS file_owner ON file FIELDS owner; \
                         DEFINE INDEX IF NOT EXISTS folder_owner ON folder FIELDS owner",
                    )
                    .await?;
                let _ = res.take::<surrealdb::types::Value>(0usize)?;
                let _ = res.take::<surrealdb::types::Value>(1usize)?;
            }
            other => return Err(DbError::Shape(format!("no migration to version {other}"))),
        }
        set_schema_version(db, v).await?;
        tracing::info!("database migrated to schema v{v}");
    }
    Ok(v)
}

/// Deletes rows written before the folder feature existed (no `folder`
/// field). Their Telegram messages are NOT deleted, so the files remain
/// in the storage channel even though the drive forgets them.
pub async fn purge_legacy_rows(db: &surrealdb::Surreal<Conn>) -> Result<u64, DbError> {
    let mut res = db
        .query("DELETE FROM file WHERE folder IS NONE RETURN BEFORE")
        .await?;
    let deleted: Vec<serde_json::Value> = res.take(0)?;
    Ok(deleted.len() as u64)
}

/// Hands every unowned `file` and `folder` row, plus the pre-multi-tenant
/// global bot pool, to `owner`; returns how many rows were claimed.
///
/// Startup calls this whenever it adopts the old single-account session, so
/// it is idempotent: once claimed, nothing is unowned any more and a second
/// call claims zero. Nothing is ever deleted.
pub async fn adopt_unowned(db: &surrealdb::Surreal<Conn>, owner: i64) -> Result<u64, DbError> {
    if owner == UNOWNED {
        return Err(DbError::Shape(
            "the unowned sentinel cannot adopt rows".to_string(),
        ));
    }
    let mut res = db
        .query(
            "UPDATE file SET owner = $owner \
               WHERE owner IS NONE OR owner = $unowned RETURN AFTER; \
             UPDATE folder SET owner = $owner \
               WHERE owner IS NONE OR owner = $unowned RETURN AFTER",
        )
        .bind(("owner", owner))
        .bind(("unowned", UNOWNED))
        .await?;
    let files: Vec<serde_json::Value> = res.take(0)?;
    let folders: Vec<serde_json::Value> = res.take(1)?;
    let claimed = (files.len() + folders.len()) as u64
        + bots::adopt_legacy_pool(db, owner).await?
        + settings::adopt_legacy_split(db, owner).await?;
    if claimed > 0 {
        tracing::info!(%owner, claimed, "adopted pre-multi-tenant rows");
    }
    Ok(claimed)
}
