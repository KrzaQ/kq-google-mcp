//! The route table. Everything under `/api` except health, the login flow and
//! the Google callback needs a browser session; the handlers say so
//! themselves with `require_session`, because the alternative — a middleware
//! over a subset of routes — puts the rule somewhere the handler cannot be
//! read next to it.

pub mod activity;
pub mod connections;
pub mod dto;
pub mod scopes;
pub mod tokens;

use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::{auth, links};
use crate::google::text;

/// What a monitor and the container's HEALTHCHECK ask for. The database is the
/// only thing whose absence makes the server useless, so it alone decides the
/// status; the other two are reported so a deployment can see why connecting
/// an account or reading a PDF does not work.
#[derive(Serialize, ToSchema)]
pub struct Health {
    /// "ok" or "error"
    pub status: String,
    pub database: bool,
    /// poppler's `pdftotext`, without which PDF text extraction is refused
    pub pdftotext: bool,
    /// `GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET` are set
    pub google: bool,
}

#[utoipa::path(get, path = "/api/health", tag = "health",
    responses((status = 200, body = Health), (status = 503, body = Health)))]
pub async fn health(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (StatusCode, Json<Health>) {
    let database = match state.db.ping().await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("health: the database does not answer: {e}");
            false
        }
    };
    let status = if database {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(Health {
            status: if database {
                "ok".into()
            } else {
                "error".into()
            },
            database,
            pdftotext: text::pdftotext_available(),
            google: state.google.is_some(),
        }),
    )
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(auth::me))
        .routes(routes!(auth::login))
        .routes(routes!(auth::callback))
        .routes(routes!(auth::logout))
        .routes(routes!(connections::list_connections))
        .routes(routes!(connections::start_connection))
        .routes(routes!(connections::reconnect_connection))
        .routes(routes!(connections::google_callback))
        .routes(routes!(
            connections::patch_connection,
            connections::delete_connection
        ))
        .routes(routes!(tokens::list_tokens, tokens::create_token))
        .routes(routes!(tokens::revoke_token))
        .routes(routes!(scopes::scopes))
        .routes(routes!(activity::list_audit))
        .routes(routes!(links::download))
}
