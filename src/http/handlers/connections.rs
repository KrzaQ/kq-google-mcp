//! Connections: the list, the consent flow that makes one, and the two edits
//! a person can make to one afterwards.
//!
//! Connecting is a browser round trip through Google, so it is two handlers
//! and a cookie: `start` (or `reconnect`) hands back the consent URL and
//! remembers the flow state, and `/api/google/callback` picks it up when the
//! browser comes back. The cookie is private, scoped to `/api/google` and
//! lives ten minutes, exactly as the OIDC login state does.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde::Deserialize;
use serde_json::json;

use super::dto::{ConnectInput, ConnectionDto, ConnectionPatchInput, ConsentUrl, ReconnectInput};
use crate::db::{
    AuditKind, AuditOutcome, Connection, ConnectionPatch, NewAuditEntry, NewConnection, User,
};
use crate::domain::scope::{Service, services_implied};
use crate::domain::seal;
use crate::google::oauth::FlowState;
use crate::http::auth::Principal;
use crate::http::error::{ApiError, ApiResult, ErrorBody};
use crate::http::{AppState, audit, auth};

/// The private cookie that carries the consent flow, and the path it is
/// scoped to: only the callback ever needs it.
const FLOW_COOKIE: &str = "gmcp_google";
const FLOW_COOKIE_PATH: &str = "/api/google";
/// Where the browser lands after a connect, good or bad.
const CONNECTIONS_PAGE: &str = "/connections";

#[utoipa::path(get, path = "/api/connections", tag = "connections",
    responses((status = 200, body = Vec<ConnectionDto>), (status = 401, body = ErrorBody)))]
pub async fn list_connections(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<ConnectionDto>>> {
    p.require_session()?;
    Ok(Json(
        state
            .db
            .visible_connections(p.reach())
            .await?
            .into_iter()
            .map(ConnectionDto::from)
            .collect(),
    ))
}

#[utoipa::path(post, path = "/api/connections/start", tag = "connections", request_body = ConnectInput,
    responses((status = 200, body = ConsentUrl), (status = 400, body = ErrorBody),
              (status = 409, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub async fn start_connection(
    State(state): State<AppState>,
    p: Principal,
    jar: PrivateCookieJar,
    Json(input): Json<ConnectInput>,
) -> ApiResult<(PrivateCookieJar, Json<ConsentUrl>)> {
    let me = p.require_session()?;
    let label = clean_label(&input.label)?;
    let services = parse_services(&input.services)?;
    let google = state.google().ok_or_else(ApiError::google_unconfigured)?;
    if state.db.find_connection(me.id, &label).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "a connection labelled {label:?} already exists"
        )));
    }
    let authorization = google.oauth.authorize(&label, &services);
    Ok((
        remember(&state, jar, &authorization.state),
        Json(ConsentUrl {
            url: authorization.url.to_string(),
        }),
    ))
}

#[utoipa::path(post, path = "/api/connections/{id}/reconnect", tag = "connections",
    params(("id" = i64, Path)), request_body = ReconnectInput,
    responses((status = 200, body = ConsentUrl), (status = 400, body = ErrorBody),
              (status = 404, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub async fn reconnect_connection(
    State(state): State<AppState>,
    p: Principal,
    jar: PrivateCookieJar,
    Path(id): Path<i64>,
    Json(input): Json<ReconnectInput>,
) -> ApiResult<(PrivateCookieJar, Json<ConsentUrl>)> {
    let me = p.require_session()?;
    let connection = mine(&state, me, id).await?;
    let services = match &input.services {
        Some(asked) => parse_services(asked)?,
        None => services_implied(&stored_services(&connection)),
    };
    let google = state.google().ok_or_else(ApiError::google_unconfigured)?;
    let authorization = google.oauth.reconnect(
        connection.id,
        &connection.label,
        &connection.google_email,
        &services,
    );
    Ok((
        remember(&state, jar, &authorization.state),
        Json(ConsentUrl {
            url: authorization.url.to_string(),
        }),
    ))
}

#[utoipa::path(patch, path = "/api/connections/{id}", tag = "connections",
    params(("id" = i64, Path)), request_body = ConnectionPatchInput,
    responses((status = 200, body = ConnectionDto), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
pub async fn patch_connection(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<i64>,
    Json(input): Json<ConnectionPatchInput>,
) -> ApiResult<Json<ConnectionDto>> {
    let me = p.require_session()?;
    mine(&state, me, id).await?;
    let label = match &input.label {
        Some(label) => Some(clean_label(label)?),
        None => None,
    };
    let updated = state
        .db
        .update_connection(
            id,
            ConnectionPatch {
                label,
                delegate_ok: input.delegate_ok,
            },
        )
        .await?;
    Ok(Json(ConnectionDto::from(updated)))
}

/// Remove a connection. The grant is revoked at Google first so the account's
/// permissions page stops listing this app, but a failure there does not keep
/// the row: the person asked for it to be gone.
#[utoipa::path(delete, path = "/api/connections/{id}", tag = "connections",
    params(("id" = i64, Path)),
    responses((status = 204), (status = 404, body = ErrorBody)))]
pub async fn delete_connection(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<i64>,
) -> ApiResult<axum::http::StatusCode> {
    let me = p.require_session()?;
    let connection = mine(&state, me, id).await?;
    if let Some(google) = state.google() {
        match seal::open(&state.config.secret, &connection.refresh_token_sealed) {
            Ok(refresh_token) => {
                if let Err(e) = google.oauth.revoke(&refresh_token).await {
                    tracing::warn!("revoking connection {id} at Google: {e}");
                }
            }
            Err(e) => tracing::warn!("connection {id} cannot be opened to revoke it: {e}"),
        }
        google.client.tokens().forget(id);
    }
    state.db.delete_connection(id).await?;
    audit::record(
        &state.db,
        NewAuditEntry {
            detail: Some(format!(
                "{} ({})",
                connection.label, connection.google_email
            )),
            ..audit::by(me, AuditKind::ConnectionRemoved, AuditOutcome::Ok)
        },
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ----- the Google callback --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GoogleCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// Where Google sends the browser back. Everything that can go wrong here ends
/// as `/connections?error=<code>`, because the person is looking at a page and
/// not at a JSON envelope; the process log carries the detail.
#[utoipa::path(get, path = "/api/google/callback", tag = "connections",
    responses((status = 303, description = "back to /connections, with ?connected=<id> or ?error=<code>")))]
pub async fn google_callback(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<GoogleCallbackQuery>,
) -> Response {
    let flow = jar
        .get(FLOW_COOKIE)
        .and_then(|c| serde_json::from_str::<FlowState>(c.value()).ok());
    let jar = jar.remove(auth::removal(FLOW_COOKIE, FLOW_COOKIE_PATH));
    match connect(&state, &jar, flow, q).await {
        Ok(id) => (
            jar,
            Redirect::to(&format!("{CONNECTIONS_PAGE}?connected={id}")),
        )
            .into_response(),
        Err(code) => (
            jar,
            Redirect::to(&format!("{CONNECTIONS_PAGE}?error={code}")),
        )
            .into_response(),
    }
}

/// The callback proper. Every failure is one of the codes below, which is what
/// the connections page turns into a sentence.
async fn connect(
    state: &AppState,
    jar: &PrivateCookieJar,
    flow: Option<FlowState>,
    q: GoogleCallbackQuery,
) -> Result<i64, &'static str> {
    let google = state.google().ok_or("google_unconfigured")?;
    let me = auth::browser_user(state, jar)
        .await
        .ok_or("not_logged_in")?;
    if let Some(error) = q.error {
        tracing::info!("google refused the consent: {error}");
        return Err(match error.as_str() {
            "access_denied" => "access_denied",
            _ => "refused",
        });
    }
    let flow = flow.ok_or("flow_expired")?;
    flow.check(q.state.as_deref().unwrap_or_default())
        .map_err(|e| match e {
            crate::google::oauth::FlowError::Missing => "no_state",
            _ => "state_mismatch",
        })?;
    let code = q.code.ok_or("no_code")?;
    let grant = google.oauth.exchange(&code, &flow).await.map_err(|e| {
        tracing::warn!("google code exchange: {e}");
        "exchange_failed"
    })?;
    let refresh_token = grant.refresh_token.ok_or_else(|| {
        tracing::warn!("google issued no refresh token for {}", flow.label);
        "no_refresh_token"
    })?;
    let info = google
        .oauth
        .userinfo(&grant.access_token)
        .await
        .map_err(|e| {
            tracing::warn!("google userinfo: {e}");
            "userinfo_failed"
        })?;
    let google_email = info.email.ok_or("no_email")?;
    let services: Vec<String> = services_implied(&flow.services)
        .iter()
        .map(ToString::to_string)
        .collect();
    let sealed = seal::seal(&state.config.secret, &refresh_token);

    let (connection, kind) = match flow.reconnect {
        Some(id) => {
            let existing = state
                .db
                .get_connection(id)
                .await
                .map_err(|_| "gone")
                .and_then(|c| {
                    if c.user_id == me.id {
                        Ok(c)
                    } else {
                        Err("gone")
                    }
                })?;
            // A reconnect must land on the account it already has: a different
            // one would silently repoint every token that names this label.
            if !existing.google_email.eq_ignore_ascii_case(&google_email) {
                tracing::warn!(
                    "reconnect of {} came back as {google_email}",
                    existing.google_email
                );
                return Err("different_account");
            }
            let updated = state
                .db
                .set_connection_grant(id, &services, &grant.granted_scopes, &sealed)
                .await
                .map_err(|e| {
                    tracing::error!("storing the reconnected grant: {e}");
                    "not_stored"
                })?;
            google.client.tokens().forget(id);
            (updated, AuditKind::Reconnect)
        }
        None => {
            // One Google account per person per portal: two connections to the
            // same mailbox would be two labels for one thing, and the tools
            // resolve by label.
            let clash = state
                .db
                .list_connections(me.id)
                .await
                .map_err(|_| "not_stored")?
                .into_iter()
                .any(|c| c.google_email.eq_ignore_ascii_case(&google_email));
            if clash {
                return Err("already_connected");
            }
            let created = state
                .db
                .create_connection(NewConnection {
                    user_id: me.id,
                    label: flow.label.clone(),
                    google_email: google_email.clone(),
                    services: services.clone(),
                    granted_scopes: grant.granted_scopes.clone(),
                    refresh_token_sealed: sealed,
                    delegate_ok: false,
                })
                .await
                .map_err(|e| {
                    tracing::warn!("storing the new connection: {e}");
                    "label_taken"
                })?;
            (created, AuditKind::Connect)
        }
    };
    audit::record(
        &state.db,
        NewAuditEntry {
            connection_id: Some(connection.id),
            args: audit::args(json!({
                "label": connection.label,
                "google_email": connection.google_email,
                "services": services,
                "granted_scopes": grant.granted_scopes,
            })),
            detail: Some(format!(
                "{} ({})",
                connection.label, connection.google_email
            )),
            ..audit::by(&me, kind, AuditOutcome::Ok)
        },
    )
    .await;
    Ok(connection.id)
}

// ----- shared -----------------------------------------------------------

fn remember(state: &AppState, jar: PrivateCookieJar, flow: &FlowState) -> PrivateCookieJar {
    let value = serde_json::to_string(flow).expect("the flow state serialises");
    jar.add(auth::flow_cookie(
        state,
        FLOW_COOKIE,
        FLOW_COOKIE_PATH,
        value,
    ))
}

/// A connection of this person, or a 404. Someone else's id is not found
/// rather than forbidden: whether it exists is not their business.
async fn mine(state: &AppState, me: &User, id: i64) -> ApiResult<Connection> {
    let connection = state.db.get_connection(id).await?;
    if connection.user_id == me.id {
        Ok(connection)
    } else {
        Err(ApiError::not_found())
    }
}

fn clean_label(label: &str) -> ApiResult<String> {
    let label = label.trim();
    if label.is_empty() {
        return Err(ApiError::bad_request("a connection needs a label"));
    }
    Ok(label.to_string())
}

/// Service names from the registry, with Drive filled in where it is implied.
/// An unknown one is refused with the list, as an unknown token scope is.
fn parse_services(services: &[String]) -> ApiResult<Vec<Service>> {
    let mut parsed = Vec::with_capacity(services.len());
    for name in services {
        let service: Service = name.trim().parse().map_err(|_| {
            ApiError::bad_request(format!(
                "unknown service {name:?}; valid services are {}",
                Service::ALL
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        if !parsed.contains(&service) {
            parsed.push(service);
        }
    }
    if parsed.is_empty() {
        return Err(ApiError::bad_request(
            "a connection needs at least one service",
        ));
    }
    Ok(services_implied(&parsed))
}

/// What a stored connection's `services` column parses back to. A name the
/// registry no longer knows is dropped rather than trusted.
fn stored_services(connection: &Connection) -> Vec<Service> {
    connection
        .services
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_is_ticked_along_and_an_unknown_service_is_refused() {
        let parsed = parse_services(&["docs".into(), "gmail".into()]).unwrap();
        assert_eq!(
            parsed,
            vec![Service::Gmail, Service::Drive, Service::Docs],
            "docs pulls drive in, and the order is the registry's"
        );
        let error = parse_services(&["mail".into()]).unwrap_err();
        assert!(
            error.message.contains("unknown service"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("gmail, drive, docs"),
            "{}",
            error.message
        );
        assert!(parse_services(&[]).is_err());
    }
}
