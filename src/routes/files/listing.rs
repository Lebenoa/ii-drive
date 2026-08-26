use axum::extract::{Path, Query};
use axum::{Extension, Json};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::{FileDto, ListQuery, MoveBody, VisibilityBody, is_message_gone};

/// A file the caller owns. Rows belonging to another account answer 404
/// rather than 403: whether that uid exists is not the caller's business.
async fn owned(state: &AppState, uid: i64, id: &str) -> ApiResult<crate::db::FileRow> {
    crate::db::get(&state.db, id)
        .await?
        .filter(|row| row.owner == uid)
        .ok_or_else(|| ApiError::not_found("file not found"))
}

/// PATCH /api/files/{id}/move — cut/paste target.
pub async fn move_file(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<MoveBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    owned(state, uid, &id).await?;
    // The destination must be the caller's own folder — a foreign folder id
    // is indistinguishable from a nonexistent one from here.
    if !body.folder.is_empty()
        && crate::db::get_folder(&state.db, &body.folder)
            .await?
            .is_none_or(|f| f.owner != uid)
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
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
    Json(body): Json<VisibilityBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    owned(state, uid, &id).await?;
    if !crate::db::set_public(&state.db, &id, body.public).await? {
        return Err(ApiError::not_found("file not found"));
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "public": body.public }),
    ))
}

pub async fn list_files(
    Extension(Caller(uid)): Extension<Caller>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let rows = crate::db::list(
        &state.db,
        uid,
        q.q.as_deref().unwrap_or(""),
        q.folder.as_deref().unwrap_or(""),
        q.limit.unwrap_or(100).min(500),
        q.offset.unwrap_or(0),
    )
    .await?;
    let files: Vec<FileDto> = rows
        .into_iter()
        .map(|r| {
            let has_thumb = super::thumbs::exists(&state.thumbs_dir, &r.uid);
            FileDto::new(r, has_thumb)
        })
        .collect();
    Ok(Json(serde_json::json!({ "files": files })))
}

pub async fn delete_file(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let row = owned(state, uid, &id).await?;
    // The messages live in the owner's chats, so only the owner's client can
    // remove them — here that is the caller.
    let tg = state.tg(uid).await?;
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
        match tg.delete_message(p.message_id, &p.chat).await {
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
    // Only after the delete is fully committed: a restore above must keep
    // the preview so the file stays intact for the retry.
    super::thumbs::remove(&state.thumbs_dir, &row.uid).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
