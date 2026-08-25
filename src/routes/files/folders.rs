use axum::extract::Path;
use axum::{Extension, Json};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};

use super::CreateFolderBody;

/// POST /api/folders — create a folder (parent "" = root).
pub async fn create_folder(
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<CreateFolderBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(ApiError::bad_request(
            "folder name must be 1-128 characters",
        ));
    }
    // A parent owned by somebody else reads as nonexistent, so a tree can
    // never be grafted onto another tenant's folder.
    if !body.parent.is_empty()
        && !crate::db::get_folder(&state.db, &body.parent)
            .await?
            .is_some_and(|f| f.owner == uid)
    {
        return Err(ApiError::bad_request("parent folder not found"));
    }
    let row = crate::db::FolderRow {
        owner: uid,
        uid: ulid::Ulid::generate().to_string(),
        name: name.to_string(),
        parent: body.parent,
    };
    crate::db::create_folder(&state.db, uid, &row.uid, &row.name, &row.parent).await?;
    Ok(Json(serde_json::json!({ "folder": row })))
}

/// GET /api/folders — the caller's folders, ordered by name.
pub async fn list_folders(
    Extension(Caller(uid)): Extension<Caller>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    let folders = crate::db::list_folders(&state.db, uid).await?;
    Ok(Json(serde_json::json!({ "folders": folders })))
}

/// DELETE /api/folders/{id} — only when empty (no files, no subfolders).
pub async fn delete_folder(
    Extension(Caller(uid)): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    // 404 for a foreign folder too — its existence stays private.
    if !crate::db::get_folder(&state.db, &id)
        .await?
        .is_some_and(|f| f.owner == uid)
    {
        return Err(ApiError::not_found("folder not found"));
    }
    if !crate::db::folder_is_empty(&state.db, &id).await? {
        return Err(ApiError::bad_request("folder is not empty"));
    }
    crate::db::delete_folder(&state.db, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
