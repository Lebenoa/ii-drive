use axum::extract::{Path, Query, State};
use axum::Json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{is_message_gone, FileDto, ListQuery, MoveBody, VisibilityBody};

/// PATCH /api/files/{id}/move — cut/paste target.
pub async fn move_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !body.folder.is_empty()
        && crate::db::get_folder(&state.db, &body.folder)
            .await?
            .is_none()
    {
        return Err(ApiError::bad_request("target folder not found"));
    }
    if !crate::db::set_folder(&state.db, &id, &body.folder).await? {
        return Err(ApiError::not_found("file not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// PATCH /api/files/{id}/visibility — flip private/public.
pub async fn set_visibility(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VisibilityBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !crate::db::set_public(&state.db, &id, body.public).await? {
        return Err(ApiError::not_found("file not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true, "public": body.public })))
}

pub async fn list_files(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = crate::db::list(
        &state.db,
        q.q.as_deref().unwrap_or(""),
        q.folder.as_deref().unwrap_or(""),
        q.limit.unwrap_or(100).min(500),
        q.offset.unwrap_or(0),
    )
    .await?;
    let files: Vec<FileDto> = rows.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::json!({ "files": files })))
}

pub async fn delete_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = crate::db::get(&state.db, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("file not found"))?;
    // Row first, then message: if the process dies in between, the API stays
    // consistent (file gone) and only Telegram holds an orphan. The reverse
    // order would leave a row pointing at a deleted message.
    let deleted = crate::db::delete(&state.db, &id).await?;
    if deleted == 0 {
        return Err(ApiError::not_found("file not found"));
    }
    // Delete every part's message; on the first failure restore the row so
    // the file stays listed and the delete can be retried. Parts that are
    // already gone on Telegram (e.g. a previous partial delete) are skipped
    // instead of failing the whole delete.
    let mut failed: Option<String> = None;
    for p in &row.parts {
        match state.tg.delete_message(p.message_id, &p.chat).await {
            Ok(()) => {}
            Err(e) if is_message_gone(&e) => {
                tracing::warn!(message_id = p.message_id, "part already deleted: {e}");
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failed {
        if let Err(re) = crate::db::insert(&state.db, &row).await {
            tracing::error!(uid = %row.uid, "cannot restore row after failed delete: {re}");
        }
        return Err(ApiError::internal(e));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
