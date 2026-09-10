//! The HTTP server: the JSON API under `/api`, the download route at `/dl`,
//! and the embedded frontend everywhere else.
//!
//! Two rules shape the router. `/api/*` never redirects on a missing session —
//! it answers 401 with the error envelope, because the frontend is a SPA and a
//! redirect there is a login page rendered inside a fetch. And there are no
//! CORS headers anywhere: the frontend is served from this same origin, so
//! nothing else has any business calling it.

pub mod audit;
pub mod auth;
pub mod error;
pub mod handlers;
pub mod legal;
pub mod links;
pub mod oidc;
mod r#static;
pub mod store;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::FromRef;
use axum_extra::extract::cookie::Key;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::config::{AuthMode, Config};
use crate::db::Db;
use crate::google;

/// The Google side of the state, present only when the client is configured.
/// Everything that touches Google goes through one of these two, so a
/// deployment without credentials serves the whole portal and refuses exactly
/// the things that need Google.
#[derive(Clone)]
pub struct Google {
    pub client: google::Client,
    pub oauth: Arc<google::oauth::OAuth>,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    /// None in dev mode, where there is no identity provider to discover.
    pub oidc: Option<Arc<oidc::OidcClient>>,
    /// None when `GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET` are
    /// unset; `/api/health` reports it and the connections page says so.
    pub google: Option<Google>,
    pub key: Key,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.key.clone()
    }
}

impl AppState {
    pub async fn new(config: Config, db: Db) -> Result<Self> {
        let oidc = match &config.auth {
            AuthMode::Oidc(o) => Some(Arc::new(oidc::discover(o, &config.public_url).await?)),
            AuthMode::Dev => None,
        };
        let google = Self::google_parts(&config, &db)?;
        let key = cookie_key(&config.secret);
        Ok(Self {
            db,
            config: Arc::new(config),
            oidc,
            google,
            key,
        })
    }

    /// The Google client and the connect flow, over the `connections` table.
    /// An unconfigured client is not an error: the portal starts, says so and
    /// works for everything that is not Google.
    fn google_parts(config: &Config, db: &Db) -> Result<Option<Google>> {
        if !config.google.configured() {
            return Ok(None);
        }
        let http = google::http_client().context("building the Google HTTP client")?;
        let client = google::Client::from_config(
            &config.google,
            config.secret.clone(),
            store::Connections::new(db.clone()),
        )
        .context("building the Google client")?;
        let oauth = google::oauth::OAuth::new(http, &config.google, &config.public_url)
            .context("building the Google connect flow")?;
        Ok(Some(Google {
            client,
            oauth: Arc::new(oauth),
        }))
    }

    /// Google, or nothing. Callers turn the `None` into the one error that
    /// says the server has no credentials.
    pub fn google(&self) -> Option<&Google> {
        self.google.as_ref()
    }
}

/// cookie::Key wants 64 bytes; stretch the configured secret deterministically.
pub fn cookie_key(secret: &[u8]) -> Key {
    use sha2::{Digest, Sha256};
    let mut material = Vec::with_capacity(64);
    material.extend_from_slice(&Sha256::digest([secret, b":signing"].concat()));
    material.extend_from_slice(&Sha256::digest([secret, b":encryption"].concat()));
    Key::from(&material)
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "gmcp",
        description = "Scoped Google portal: connections, tokens, activity and download links"
    ),
    tags(
        (name = "health"), (name = "auth"), (name = "connections"), (name = "tokens"),
        (name = "audit"), (name = "links")
    )
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    let (api, openapi) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(handlers::routes())
        .split_for_parts();
    let openapi_json = axum::Json(openapi);
    Router::new()
        .merge(api)
        // Bearer only, and its own transport: the MCP endpoint shares this
        // state and nothing else with the API.
        .merge(crate::mcp::router(state.clone()))
        // Public and unauthenticated: Google's consent screen links to both,
        // and a person deciding whether to connect an account must be able to
        // read them without one.
        .route("/about", axum::routing::get(legal::about))
        .route("/privacy", axum::routing::get(legal::privacy))
        .route("/terms", axum::routing::get(legal::terms))
        .route(
            "/api/openapi.json",
            axum::routing::get(move || async move { openapi_json.clone() }),
        )
        .fallback(r#static::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(config: Config) -> Result<()> {
    // The poppler lookup happens once, here, so /api/health and the first
    // extraction answer from the same decision.
    google::text::init();
    tracing::info!(
        "{}, pdftotext {}",
        config.summary(),
        if google::text::pdftotext_available() {
            "present"
        } else {
            "missing"
        }
    );
    if !config.google.configured() {
        tracing::warn!(
            "GMCP_GOOGLE_CLIENT_ID and GMCP_GOOGLE_CLIENT_SECRET are unset; \
             no account can be connected until they are"
        );
    }
    let db = Db::open(&config.database).await?;
    if config.auto_migrate {
        db.migrate().await?;
    }
    let bind = config.bind;
    let state = AppState::new(config, db).await?;
    if let AuthMode::Dev = state.config.auth {
        tracing::warn!("GMCP_AUTH=dev: every request is the dev user");
    }
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!("listening on http://{bind}");
    // The connect info is what the download route logs as the client address,
    // and what decides whether an X-Forwarded-For is believed.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await.ok();
}
