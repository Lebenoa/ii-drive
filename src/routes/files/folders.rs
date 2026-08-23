use axum::extract::{Path, State};
use axum::Json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

use super::CreateFolderBody;

/// POST /api/folders — create a folder (parent "" = root).
pub async fn create_folder(
    State(state): State<AppState>,
    Json(body): Json<CreateFolderBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(ApiError::bad_request("folder name must be 1-128 characters"));
    }
    if !body.parent.is_empty()
        && crate::db::get_folder(&state.db, &body.parent)
            .await?
            .is_none()
    {
        return Err(ApiError::bad_request("parent folder not found"));
    }
    let row = crate::db::FolderRow {
        uid: ulid::Ulid::generate().to_string(),
        name: name.to_string(),
        parent: body.parent,
    };
    crate::db::create_folder(&state.db, &row.uid, &row.name, &row.parent).await?;
    Ok(Json(serde_json::json!({ "folder": row })))
}

/// GET /api/folders — every folder, ordered by name.
pub async fn list_folders(
    State(state): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let folders = crate::db::list_folders(&state.db).await?;
    Ok(Json(serde_json::json!({ "folders": folders })))
}

/// DELETE /api/folders/{id} — only when empty (no files, no subfolders).
pub async fn delete_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if crate::db::get_folder(&state.db, &id).await?.is_none() {
        return Err(ApiError::not_found("folder not found"));
    }
    if !crate::db::folder_is_empty(&state.db, &id).await? {
        return Err(ApiError::bad_request("folder is not empty"));
    }
    crate::db::delete_folder(&state.db, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
