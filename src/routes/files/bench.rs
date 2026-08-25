use axum::{Extension, Json};

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Operator-only upload A/B: same buffer, same storage channel, bot pool vs
/// owner session. Exists to answer one question — is the throughput gap vs
/// the official client caused by bot sessions or by connection depth?
async fn admin_only(state: &AppState, uid: i64) -> Result<(), ApiError> {
    if state.is_admin(uid).await {
        Ok(())
    } else {
        Err(ApiError::not_found("not found"))
    }
}

#[derive(serde::Deserialize)]
pub struct BenchBody {
    size_mb: u64,
}

pub async fn bench(
    Extension(Caller(uid)): Extension<Caller>,
    Json(body): Json<BenchBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let state = crate::state::get();
    admin_only(state, uid).await?;
    if body.size_mb == 0 || body.size_mb > 512 {
        return Err(ApiError::bad_request("size_mb must be 1..=512"));
    }
    let tg = state.tg(uid).await?;
    // First selected storage channel mirrors real upload conditions.
    let chat = crate::db::get_channels(&state.db, &uid.to_string())
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .next()
        .map(|c| c.chat)
        .ok_or_else(|| ApiError::bad_request("no storage channels selected"))?;
    tg.bench_upload(body.size_mb, &chat)
        .await
        .map_err(ApiError::bad_request)
        .map(Json)
}
