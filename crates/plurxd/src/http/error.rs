//! API error type: every handler returns `Result<_, ApiError>`, and this maps
//! failures to a JSON `{ "error": "..." }` body with the right status.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use plurx_core::error::StoreError;
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    NotFound(&'static str),
    BadRequest(String),
    Unauthorized,
    Forbidden,
    Conflict(String),
    UnsupportedMedia(String),
    ServiceUnavailable(String),
    /// A 422 whose body carries more than a sentence — the caller needs the
    /// data to fix the request, not just to read about it. Used for "path is
    /// not under any library root", which lists the roots so a path-mapping
    /// mistake between two applications diagnoses itself.
    Unprocessable(serde_json::Value),
    Internal(String),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, String) {
        match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, format!("{what} not found")),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required".into()),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "admin privileges required".into()),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            ApiError::UnsupportedMedia(msg) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, msg.clone()),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
            // Handled in `into_response`, which needs the whole body.
            ApiError::Unprocessable(v) => (StatusCode::UNPROCESSABLE_ENTITY, v.to_string()),
            ApiError::Internal(msg) => {
                // Detail is logged, not leaked to the client.
                tracing::error!(error = %msg, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".into(),
                )
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Unprocessable(body) = self {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
        }
        let (status, message) = self.parts();
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        ApiError::Internal(err.to_string())
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        ApiError::Internal(err.to_string())
    }
}
