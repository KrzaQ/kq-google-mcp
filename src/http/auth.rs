//! Who is calling: a browser session (private cookie), a personal bearer
//! token, or a delegate token naming the acting person. Every principal
//! resolves to a user, so the layers above never care which.
//!
//! The API itself is for people: `require_session` guards everything under
//! `/api` that is not health, login or the Google callback. Bearer resolution
//! lives here all the same, in [`bearer`], because the MCP endpoint shares it
//! and a delegate token must be resolved the same way in both places.

use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiError, ApiResult};
use super::handlers::dto::UserDto;
use super::oidc::LoginState;
use crate::config::AuthMode;
use crate::db::{ApiToken, ClientProfile, Db, DbResult, Reach, User};
use crate::domain::limits::SESSION_DAYS;
use crate::domain::scope::{self, Scope};
use crate::domain::token;

pub const SESSION_COOKIE: &str = "gmcp_session";
const LOGIN_COOKIE: &str = "gmcp_login";
const LOGIN_COOKIE_PATH: &str = "/api/auth";
/// A delegate token acts only for people who have logged in through the
/// browser this recently, so a revocation in authentik reaches the gateway
/// path on its own, with the same lag a session cookie has.
pub const DELEGATE_LOGIN_DAYS: i64 = SESSION_DAYS;
pub const DEV_SUBJECT: &str = "dev";

/// The request header a delegate token uses to name the acting user.
pub const DELEGATE_HEADER: &str = "x-gmcp-user";

#[derive(Debug, Clone)]
pub enum Principal {
    Session(User),
    /// A personal token acting as its user.
    Token {
        token: ApiToken,
        user: User,
    },
    /// A delegate token acting for the user it named in `X-Gmcp-User`.
    Delegate {
        token: ApiToken,
        user: User,
    },
}

impl Principal {
    pub fn user(&self) -> &User {
        match self {
            Self::Session(u) | Self::Token { user: u, .. } | Self::Delegate { user: u, .. } => u,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session(_) => "session",
            Self::Token { .. } => "token",
            Self::Delegate { .. } => "delegate",
        }
    }

    /// The token behind a bearer call, for the log and for link ownership.
    pub fn token(&self) -> Option<&ApiToken> {
        match self {
            Self::Session(_) => None,
            Self::Token { token, .. } | Self::Delegate { token, .. } => Some(token),
        }
    }

    /// Which connections this principal may name, in the shape `Db` takes.
    pub fn reach(&self) -> Reach {
        match self {
            Self::Session(u) => Reach::Session { user_id: u.id },
            Self::Token { token, user } | Self::Delegate { token, user } => {
                Reach::of_token(token, user.id)
            }
        }
    }

    /// What this principal may do. A session is the person themselves, so it
    /// carries every service scope in the registry; a token carries exactly
    /// what its row says, and a string the registry no longer knows is
    /// dropped rather than trusted.
    pub fn scopes(&self) -> Vec<Scope> {
        match self {
            Self::Session(_) => scope::all_scopes()
                .into_iter()
                .filter(|s| *s != Scope::Delegate)
                .collect(),
            Self::Token { token, .. } | Self::Delegate { token, .. } => {
                token.scopes.iter().filter_map(|s| s.parse().ok()).collect()
            }
        }
    }

    /// How results are shaped for whoever is on the other end.
    pub fn client_profile(&self) -> ClientProfile {
        match self {
            Self::Session(_) => ClientProfile::Generic,
            Self::Token { token, .. } | Self::Delegate { token, .. } => token.client,
        }
    }

    /// The portal is for people: connections, tokens and the log are managed
    /// in the browser, never by a token. Tokens are for `/mcp`.
    pub fn require_session(&self) -> ApiResult<&User> {
        match self {
            Self::Session(u) => Ok(u),
            Self::Token { .. } | Self::Delegate { .. } => Err(ApiError::forbidden(
                "this is a browser endpoint; bearer tokens are for /mcp",
            )),
        }
    }
}

fn session_cookie(state: &AppState, user_id: i64) -> Cookie<'static> {
    let expires = Utc::now() + chrono::Duration::days(SESSION_DAYS);
    let mut c = Cookie::new(SESSION_COOKIE, format!("{user_id}:{}", expires.timestamp()));
    c.set_path("/");
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_secure(state.config.public_url.scheme() == "https");
    c.set_max_age(::time::Duration::days(SESSION_DAYS));
    c
}

/// A cookie that carries flow state from one redirect to the next: ten
/// minutes, scoped to the path that reads it back.
pub fn flow_cookie(
    state: &AppState,
    name: &'static str,
    path: &'static str,
    value: String,
) -> Cookie<'static> {
    let mut c = Cookie::new(name, value);
    c.set_path(path);
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_secure(state.config.public_url.scheme() == "https");
    c.set_max_age(::time::Duration::minutes(10));
    c
}

pub fn removal(name: &'static str, path: &'static str) -> Cookie<'static> {
    let mut c = Cookie::from(name);
    c.set_path(path);
    c
}

async fn session_user(state: &AppState, jar: &PrivateCookieJar) -> Option<User> {
    let cookie = jar.get(SESSION_COOKIE)?;
    let (id, exp) = cookie.value().split_once(':')?;
    let id: i64 = id.parse().ok()?;
    let exp: i64 = exp.parse().ok()?;
    if exp < Utc::now().timestamp() {
        return None;
    }
    state.db.get_user(id).await.ok()
}

/// The one user `GMCP_AUTH=dev` knows. It is upserted on every login, so its
/// `last_login_at` behaves like anyone else's.
pub async fn dev_user(db: &Db) -> DbResult<User> {
    db.upsert_user(DEV_SUBJECT, Some("dev@localhost"), Some("Dev"))
        .await
}

/// The person behind the session cookie, or the dev user in dev mode. The two
/// browser flows — the OIDC callback and the Google callback — use this rather
/// than the extractor, because their answer to "not logged in" is a page or a
/// redirect and not a JSON envelope.
pub async fn browser_user(state: &AppState, jar: &PrivateCookieJar) -> Option<User> {
    if let Some(u) = session_user(state, jar).await {
        return Some(u);
    }
    match state.config.auth {
        AuthMode::Dev => dev_user(&state.db).await.ok(),
        AuthMode::Oidc(_) => None,
    }
}

/// The principal behind an `Authorization: Bearer` header, if there is one.
/// Shared by the API extractor and the MCP middleware so a delegate token is
/// resolved the same way everywhere: it must name a known user in
/// `X-Gmcp-User`, and acts as that user.
pub async fn bearer(state: &AppState, headers: &HeaderMap) -> ApiResult<Option<Principal>> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ApiError::unauthorized())?;
    let Some(secret) = value.strip_prefix("Bearer ").map(str::trim) else {
        return Err(ApiError::unauthorized());
    };
    let Some(t) = state.db.find_active_token(&token::hash(secret)).await? else {
        return Err(ApiError::unauthorized());
    };
    state.db.touch_token_used(&t).await?;
    if let Some(user_id) = t.user_id {
        let user = state.db.get_user(user_id).await?;
        return Ok(Some(Principal::Token { token: t, user }));
    }
    let email = headers
        .get(DELEGATE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::forbidden("a delegate token needs X-Gmcp-User"))?;
    let user = state
        .db
        .find_user_by_email(email)
        .await?
        .ok_or_else(|| ApiError::forbidden(format!("{email} has never logged in here")))?;
    let fresh = user
        .last_login_at
        .is_some_and(|at| Utc::now() - at <= chrono::Duration::days(DELEGATE_LOGIN_DAYS));
    if !fresh {
        return Err(ApiError::forbidden(format!(
            "{email} has not logged in through the browser for {DELEGATE_LOGIN_DAYS} days; \
             log in there once to keep using the gateway"
        )));
    }
    Ok(Some(Principal::Delegate { token: t, user }))
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(p) = bearer(state, &parts.headers).await? {
            return Ok(p);
        }
        let jar = PrivateCookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::unauthorized())?;
        if let Some(u) = session_user(state, &jar).await {
            return Ok(Principal::Session(u));
        }
        if let AuthMode::Dev = state.config.auth {
            let u = dev_user(&state.db).await?;
            return Ok(Principal::Session(u));
        }
        Err(ApiError::unauthorized())
    }
}

// ----- handlers -------------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub struct Me {
    /// "session", "token" or "delegate"
    pub kind: String,
    pub user: UserDto,
    /// How many connections this principal can reach
    pub connections: usize,
    /// How many tokens this person manages
    pub tokens: usize,
    pub scopes: Vec<String>,
    /// False when `GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET` are
    /// unset: the connections page says so instead of offering Connect.
    pub google_configured: bool,
}

#[utoipa::path(get, path = "/api/me", tag = "auth",
    responses((status = 200, body = Me), (status = 401, body = super::error::ErrorBody)))]
pub async fn me(State(state): State<AppState>, p: Principal) -> ApiResult<axum::Json<Me>> {
    let user = p.require_session()?;
    let connections = state.db.visible_connections(p.reach()).await?.len();
    let tokens = state.db.list_user_tokens(user.id).await?.len();
    Ok(axum::Json(Me {
        kind: p.kind().into(),
        user: UserDto::from(user.clone()),
        connections,
        tokens,
        scopes: p.scopes().iter().map(ToString::to_string).collect(),
        google_configured: state.google.is_some(),
    }))
}

#[derive(Deserialize)]
pub struct LoginQuery {
    pub next: Option<String>,
}

fn safe_next(next: Option<String>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n,
        _ => "/".to_string(),
    }
}

/// Start the login. Dev mode signs the dev user in directly.
#[utoipa::path(get, path = "/api/auth/login", tag = "auth",
    responses((status = 303, description = "redirect to the identity provider")))]
pub async fn login(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<LoginQuery>,
) -> Response {
    let next = safe_next(q.next);
    match &state.oidc {
        None => {
            let user = match dev_user(&state.db).await {
                Ok(u) => u,
                Err(e) => return ApiError::from(e).into_response(),
            };
            let jar = jar.add(session_cookie(&state, user.id));
            (jar, Redirect::to(&next)).into_response()
        }
        Some(oidc) => {
            let (url, login) = oidc.authorize();
            let value = serde_json::to_string(&(login, next)).expect("serialize login state");
            let jar = jar.add(flow_cookie(&state, LOGIN_COOKIE, LOGIN_COOKIE_PATH, value));
            (jar, Redirect::to(url.as_str())).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// The login flow is the one place a browser, not the frontend, is talking to
/// the API, so a failure is a page and not a JSON envelope.
pub fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font-family:sans-serif;max-width:40em;margin:4em auto\"><h1>{title}</h1><p>{body}</p>\
         <p><a href=\"/\">Back</a></p></body>"
    );
    (status, Html(html)).into_response()
}

#[utoipa::path(get, path = "/api/auth/callback", tag = "auth",
    responses((status = 303), (status = 403, description = "not a member of the required group, when one is configured")))]
pub async fn callback(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = &state.oidc else {
        return page(StatusCode::NOT_FOUND, "Not found", "OIDC is not enabled.");
    };
    if let Some(err) = q.error {
        let desc = q.error_description.unwrap_or_default();
        return page(
            StatusCode::BAD_REQUEST,
            "Login failed",
            &format!("{err}: {desc}"),
        );
    }
    let Some(cookie) = jar.get(LOGIN_COOKIE) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Login expired",
            "Start again from the login page.",
        );
    };
    let Ok((login, next)) = serde_json::from_str::<(LoginState, String)>(cookie.value()) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Login expired",
            "Start again from the login page.",
        );
    };
    let jar = jar.remove(removal(LOGIN_COOKIE, LOGIN_COOKIE_PATH));
    if q.state.as_deref() != Some(login.csrf.as_str()) {
        return page(StatusCode::BAD_REQUEST, "Login failed", "State mismatch.");
    }
    let Some(code) = q.code else {
        return page(
            StatusCode::BAD_REQUEST,
            "Login failed",
            "No authorization code.",
        );
    };
    let identity = match oidc.exchange(&code, &login).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("OIDC callback: {e:#}");
            return page(
                StatusCode::BAD_GATEWAY,
                "Login failed",
                "The identity provider rejected the login.",
            );
        }
    };
    if let Some(group) = &oidc.group
        && !identity.groups.iter().any(|g| g == group)
    {
        tracing::warn!(
            "login refused for {}: not in group {group}",
            identity.subject
        );
        return page(
            StatusCode::FORBIDDEN,
            "Not allowed",
            &format!("Your account is not in the <code>{group}</code> group."),
        );
    }
    let user = match state
        .db
        .upsert_user(
            &identity.subject,
            identity.email.as_deref(),
            identity.name.as_deref(),
        )
        .await
    {
        Ok(u) => u,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let jar = jar.add(session_cookie(&state, user.id));
    (jar, Redirect::to(&next)).into_response()
}

#[utoipa::path(post, path = "/api/auth/logout", tag = "auth", responses((status = 204)))]
pub async fn logout(jar: PrivateCookieJar) -> Response {
    (
        jar.remove(removal(SESSION_COOKIE, "/")),
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_must_be_a_local_path() {
        assert_eq!(safe_next(Some("/connections".into())), "/connections");
        assert_eq!(safe_next(Some("//evil.example".into())), "/");
        assert_eq!(safe_next(Some("https://evil.example".into())), "/");
        assert_eq!(safe_next(None), "/");
    }
}
