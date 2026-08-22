use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::UNAUTHORIZED, msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.into())
    }
    pub fn too_large(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::PAYLOAD_TOO_LARGE, msg.into())
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::SERVICE_UNAVAILABLE, msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        let msg: String = msg.into();
        tracing::error!("internal error: {msg}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl From<crate::db::DbError> for ApiError {
    fn from(e: crate::db::DbError) -> Self {
        ApiError::internal(format!("database error: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(format!("io error: {e}"))
    }
}

