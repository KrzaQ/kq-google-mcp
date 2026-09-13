//! One error shape for the whole API: `{"error": {"code", "message"}}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::db::DbError;
use crate::google;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        )
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "not found")
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!("internal error: {err}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
    /// The Google client has no credentials, so nothing that talks to Google
    /// can work until `GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET`
    /// are set. Everything else in the portal keeps working, which is why this
    /// is an error per request and not a refusal to start.
    pub fn google_unconfigured() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "google_unconfigured",
            "the Google client is not configured on this server",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::not_found(),
            DbError::Conflict(m) => Self::conflict(m),
            DbError::Sqlx(e) => Self::internal(e),
        }
    }
}

/// A Google failure is reported for what it is: the portal did its part and
/// the other side refused, so the status says "upstream" and the message is
/// Google's own.
impl From<google::Error> for ApiError {
    fn from(e: google::Error) -> Self {
        use google::Error as G;
        match e {
            G::NotConfigured(_) => Self::google_unconfigured(),
            G::NeedsReauth { .. } => Self::new(StatusCode::CONFLICT, "needs_reauth", e.to_string()),
            G::Google(ref g) => Self::new(
                StatusCode::BAD_GATEWAY,
                "google",
                format!("google returned {}: {}", g.status, g.message),
            ),
            G::Transport(_) => Self::new(StatusCode::BAD_GATEWAY, "google_unreachable", {
                e.to_string()
            }),
            G::Unsupported(m) => Self::bad_request(m),
            // An id that would point the call at another endpoint, or a URL
            // out of a response body that does not point at Google: the
            // caller asked for something this server does not do.
            G::Path(m) | G::Untrusted(m) => Self::bad_request(m),
            G::TooLarge | G::PdftotextMissing => Self::bad_request(e.to_string()),
            G::PdftotextTimeout(_) => {
                Self::new(StatusCode::GATEWAY_TIMEOUT, "timeout", e.to_string())
            }
            G::Connection(_) | G::Malformed(_) => Self::internal(e),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(format!("{e:#}"))
    }
}
