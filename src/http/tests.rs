//! Router tests. Requests go through the whole stack — extractors, auth,
//! handlers, the JSON envelope — against an in-memory database and, where
//! Google is involved, a wiremock server standing in for it. Nothing here
//! touches the network, a file or an environment variable.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{AuthMode, GoogleConfig, OidcConfig};
use crate::db::{
    ApiToken, ClientProfile, Connection, ConnectionStatus, LinkKind, NewConnection, NewLink,
    NewToken, User,
};
use crate::domain::{link as domain_link, seal, token as domain_token};
use crate::http::auth::{DEV_SUBJECT, Principal};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/google/");

fn fixture(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("{FIXTURES}{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture {name} is not JSON: {e}"))
}

/// A Google client that is not configured: what a fresh clone has.
fn unconfigured() -> GoogleConfig {
    GoogleConfig {
        client_id: None,
        client_secret: None,
        api_base: "https://www.googleapis.com".parse().unwrap(),
        oauth_base: "https://oauth2.googleapis.com".parse().unwrap(),
        accounts_base: "https://accounts.google.com".parse().unwrap(),
    }
}

/// Every Google base pointed at one mock server, as `google/tests` does.
fn mocked(server: &MockServer) -> GoogleConfig {
    let base: url::Url = server.uri().parse().unwrap();
    GoogleConfig {
        client_id: Some("gmcp-test.apps.googleusercontent.com".into()),
        client_secret: Some("test-client-secret".into()),
        api_base: base.clone(),
        oauth_base: base.clone(),
        accounts_base: base,
    }
}

fn config(auth: AuthMode, public_url: &str, google: GoogleConfig) -> Config {
    Config {
        database: PathBuf::new(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: public_url.parse().unwrap(),
        secret: SECRET.to_vec(),
        auth,
        google,
        auto_migrate: false,
    }
}

fn oidc_mode() -> AuthMode {
    AuthMode::Oidc(OidcConfig {
        issuer: "http://127.0.0.1:1/unused".into(),
        client_id: "x".into(),
        client_secret: "y".into(),
        group: Some("gmcp".into()),
    })
}

/// A state built without OIDC discovery, which is how the tests that do not
/// exercise the login flow get a strict, cookie-or-token-only server.
fn state_of(db: &Db, config: Config) -> AppState {
    let key = cookie_key(&config.secret);
    let google = AppState::google_parts(&config, db).expect("google parts");
    AppState {
        db: db.clone(),
        config: Arc::new(config),
        oidc: None,
        google,
        key,
    }
}

/// Dev auth: every request is the dev user, and no Google credentials.
fn dev_app(db: &Db) -> Router {
    router(state_of(
        db,
        config(AuthMode::Dev, "http://localhost:8000", unconfigured()),
    ))
}

/// Dev auth with Google pointed at a mock server.
fn google_app(db: &Db, server: &MockServer) -> Router {
    router(state_of(
        db,
        config(AuthMode::Dev, "http://localhost:8000", mocked(server)),
    ))
}

/// Neither dev mode nor a discovered provider: a cookie or a bearer token, or
/// nothing at all.
fn strict_app(db: &Db) -> Router {
    router(state_of(
        db,
        config(oidc_mode(), "https://gmcp.example", unconfigured()),
    ))
}

fn strict_state(db: &Db) -> AppState {
    state_of(
        db,
        config(oidc_mode(), "https://gmcp.example", unconfigured()),
    )
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value, HeaderMap) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, body, headers)
}

/// The raw bytes of a response, for the download route.
async fn call_bytes(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>, HeaderMap) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes, headers)
}

fn req(method: &str, uri: &str, auth: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(a) = auth {
        b = b.header(header::AUTHORIZATION, format!("Bearer {a}"));
    }
    match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

fn with_cookies(mut r: Request<Body>, cookies: &str) -> Request<Body> {
    r.headers_mut()
        .insert(header::COOKIE, cookies.parse().unwrap());
    r
}

fn cookie_header(headers: &HeaderMap) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

async fn user(db: &Db, subject: &str, email: &str) -> User {
    db.upsert_user(subject, Some(email), Some(subject))
        .await
        .unwrap()
}

async fn connection(db: &Db, user: &User, label: &str, google_email: &str) -> Connection {
    db.create_connection(NewConnection {
        user_id: user.id,
        label: label.into(),
        google_email: google_email.into(),
        services: vec!["gmail".into(), "drive".into()],
        granted_scopes: vec!["openid".into()],
        refresh_token_sealed: seal::seal(SECRET, "1//09exampleRefreshTokenForTests"),
        delegate_ok: true,
    })
    .await
    .unwrap()
}

/// A token row and its secret.
async fn token_for(
    db: &Db,
    name: &str,
    scopes: &[&str],
    user: Option<&User>,
    created_by: i64,
) -> (ApiToken, String) {
    let new = domain_token::generate();
    let row = db
        .create_token(NewToken {
            name: name.into(),
            token_hash: new.hash,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            client: ClientProfile::Generic,
            user_id: user.map(|u| u.id),
            all_connections: true,
            created_by,
        })
        .await
        .unwrap();
    (row, new.secret)
}

// ----- the shape of the API -------------------------------------------------

#[tokio::test]
async fn health_and_openapi_need_no_auth_and_report_google_unconfigured() {
    let db = Db::open_memory().await.unwrap();
    let app = strict_app(&db);

    let (s, b, _) = call(&app, req("GET", "/api/health", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "ok");
    assert_eq!(b["database"], true);
    assert_eq!(b["google"], false, "no client id, no Google");

    let (s, b, _) = call(&app, req("GET", "/api/openapi.json", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["paths"]["/api/connections"].is_object());
    assert!(b["paths"]["/api/connections/{id}"].is_object());
    assert!(b["paths"]["/api/tokens"].is_object());
    assert!(b["paths"]["/api/scopes"].is_object());
    assert!(b["paths"]["/api/audit"].is_object());
    assert!(b["paths"]["/dl/{id}"].is_object());
}

#[tokio::test]
async fn api_never_redirects_on_missing_auth() {
    let db = Db::open_memory().await.unwrap();
    let app = strict_app(&db);
    for uri in ["/api/me", "/api/connections", "/api/tokens", "/api/audit"] {
        let (s, b, h) = call(&app, req("GET", uri, None, None)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(b["error"]["code"], "unauthorized", "{uri}");
        assert!(h.get(header::LOCATION).is_none(), "{uri} redirected");
    }
    let (s, _, _) = call(&app, req("GET", "/api/me", Some("gg_bogus"), None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_api_is_for_browsers_and_a_token_is_told_so() {
    let db = Db::open_memory().await.unwrap();
    let app = strict_app(&db);
    let anna = user(&db, "anna", "anna@example.test").await;
    let (_, secret) = token_for(&db, "claude", &["gmail:read"], Some(&anna), anna.id).await;
    let (s, b, _) = call(&app, req("GET", "/api/me", Some(&secret), None)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        b["error"]["message"].as_str().unwrap().contains("/mcp"),
        "{b}"
    );
}

#[tokio::test]
async fn an_unknown_path_falls_through_to_the_frontend_but_api_never_does() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);
    // No bundle is built in a test run, so the fallback says so — with a 404
    // that is emphatically not JSON, and never the index for an /api path.
    let (s, b, _) = call(&app, req("GET", "/api/nope", None, None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, Value::String("not found".into()));
}

// ----- dev mode, tokens, scopes and the log ---------------------------------

#[tokio::test]
async fn dev_mode_is_one_session_that_manages_its_own_tokens() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);

    let (s, b, _) = call(&app, req("GET", "/api/me", None, None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["kind"], "session");
    assert_eq!(b["user"]["email"], "dev@localhost");
    assert_eq!(b["connections"], 0);
    assert_eq!(b["google_configured"], false);
    // A session is the person, so it carries every service scope and not
    // `delegate`, which is about who a token acts for.
    let scopes = b["scopes"].as_array().unwrap();
    assert!(scopes.contains(&json!("gmail:draft")));
    assert!(!scopes.contains(&json!("delegate")));

    let (s, b, _) = call(
        &app,
        req(
            "POST",
            "/api/tokens",
            None,
            Some(json!({
                "name": "claude",
                "scopes": ["gmail:read", "docs:read"],
                "client": "claude-code",
                "all_connections": true
            })),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let secret = b["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("gg_"));
    assert_eq!(b["client"], "claude-code");
    assert_eq!(b["scopes"], json!(["gmail:read", "docs:read"]));
    assert_eq!(b["delegate"], false);
    let id = b["id"].as_i64().unwrap();

    let (s, b, _) = call(&app, req("GET", "/api/tokens", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 1);
    assert!(b[0].get("secret").is_none(), "the secret is shown once");
    assert!(b[0].get("token_hash").is_none());

    // The token resolves as its user, but only for /mcp; the API says so.
    let state = state_of(
        &db,
        config(AuthMode::Dev, "http://localhost:8000", unconfigured()),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {secret}").parse().unwrap(),
    );
    let p = auth::bearer(&state, &headers).await.unwrap().unwrap();
    assert!(matches!(p, Principal::Token { .. }));
    assert_eq!(p.user().subject, DEV_SUBJECT);

    let (s, _, _) = call(
        &app,
        req("DELETE", &format!("/api/tokens/{id}"), None, None),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(
        auth::bearer(&state, &headers).await.is_err(),
        "a revoked token is no token"
    );

    // Both actions are in the log.
    let (s, b, _) = call(&app, req("GET", "/api/audit", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    let kinds: Vec<&str> = b["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["token_revoked", "token_created"]);
    assert_eq!(b["entries"][1]["args"]["client"], "claude-code");
    assert_eq!(b["next"], Value::Null);
}

#[tokio::test]
async fn a_token_is_checked_against_the_registry_and_against_its_owner() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);
    let create = |body: Value| req("POST", "/api/tokens", None, Some(body));

    let (s, b, _) = call(
        &app,
        create(json!({"name": "x", "scopes": ["gmail:send"], "all_connections": true})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let message = b["error"]["message"].as_str().unwrap();
    assert!(message.contains("unknown scope"), "{message}");
    assert!(message.contains("gmail:read"), "the valid list: {message}");
    assert!(message.contains("delegate"), "the valid list: {message}");

    let (s, b, _) = call(
        &app,
        create(json!({"name": "x", "scopes": ["docs:write"], "all_connections": true})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("docs:write is useless without docs:read"),
        "{b}"
    );

    // Someone else's connection cannot be put on an allowlist.
    let bob = user(&db, "bob", "bob@example.test").await;
    let his = connection(&db, &bob, "work", "bob.work@example.test").await;
    let (s, b, _) = call(
        &app,
        create(json!({"name": "x", "scopes": ["gmail:read"], "connection_ids": [his.id]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not one of yours"),
        "{b}"
    );

    // A personal token has to reach something.
    let (s, _, _) = call(&app, create(json!({"name": "x", "scopes": ["gmail:read"]}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // A delegate token has no user and no allowlist of its own.
    let (s, b, _) = call(
        &app,
        create(
            json!({"name": "gw", "scopes": ["gmail:read", "delegate"], "all_connections": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b, _) = call(
        &app,
        create(json!({"name": "gw", "scopes": ["gmail:read", "delegate"], "client": "openwebui"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["delegate"], true);
    assert_eq!(b["user_id"], Value::Null);

    let (s, _, _) = call(
        &app,
        create(json!({"name": "x", "scopes": ["gmail:read"], "client": "emacs", "all_connections": true})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an unknown client profile");
}

#[tokio::test]
async fn a_token_may_only_name_its_own_connections_and_then_reaches_them() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);
    // The dev user exists once something asks for it.
    let (_, me, _) = call(&app, req("GET", "/api/me", None, None)).await;
    let dev = db
        .get_user(me["user"]["id"].as_i64().unwrap())
        .await
        .unwrap();
    let work = connection(&db, &dev, "work", "anna.work@example.test").await;
    connection(&db, &dev, "personal", "anna@example.test").await;

    let (s, b, _) = call(
        &app,
        req(
            "POST",
            "/api/tokens",
            None,
            Some(json!({"name": "one", "scopes": ["gmail:read"], "connection_ids": [work.id]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["connection_ids"], json!([work.id]));
    let secret = b["secret"].as_str().unwrap().to_string();

    // Its reach is the allowlist and nothing else.
    let state = state_of(
        &db,
        config(AuthMode::Dev, "http://localhost:8000", unconfigured()),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {secret}").parse().unwrap(),
    );
    let p = auth::bearer(&state, &headers).await.unwrap().unwrap();
    let visible = db.visible_connections(p.reach()).await.unwrap();
    assert_eq!(
        visible.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
        ["work"]
    );
}

#[tokio::test]
async fn the_scope_registry_says_what_each_level_unlocks() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);
    let (s, b, _) = call(&app, req("GET", "/api/scopes", None, None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let gmail = &b["services"][0];
    assert_eq!(gmail["service"], "gmail");
    assert_eq!(gmail["levels"][0]["scope"], "gmail:read");
    assert!(
        gmail["levels"][0]["tools"]
            .as_array()
            .unwrap()
            .contains(&json!("gmail_search"))
    );
    let docs = b["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == "docs")
        .unwrap();
    assert_eq!(docs["levels"][1]["scope"], "docs:write");
    assert_eq!(docs["levels"][1]["requires"], "docs:read");
    assert_eq!(b["delegate"]["scope"], "delegate");
    assert!(
        b["clients"]
            .as_array()
            .unwrap()
            .contains(&json!("openwebui"))
    );
}

// ----- how a bearer token resolves ------------------------------------------

#[tokio::test]
async fn a_delegate_token_acts_only_for_someone_who_logged_in_lately() {
    let db = Db::open_memory().await.unwrap();
    let state = strict_state(&db);
    let anna = user(&db, "anna", "anna@example.test").await;
    let (_, gateway) =
        token_for(&db, "openwebui", &["gmail:read", "delegate"], None, anna.id).await;

    let headers = |email: Option<&str>| {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {gateway}").parse().unwrap(),
        );
        if let Some(email) = email {
            h.insert(auth::DELEGATE_HEADER, email.parse().unwrap());
        }
        h
    };

    let e = auth::bearer(&state, &headers(None)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert!(e.message.contains("X-Gmcp-User"), "{}", e.message);

    let e = auth::bearer(&state, &headers(Some("nobody@example.test")))
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert!(e.message.contains("never logged in"), "{}", e.message);

    let p = auth::bearer(&state, &headers(Some("anna@example.test")))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(p, Principal::Delegate { .. }));
    assert_eq!(p.user().id, anna.id);
    assert_eq!(p.reach(), crate::db::Reach::Delegate { user_id: anna.id });

    // A person who has not been in the browser for a month loses the gateway.
    sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
        .bind(Utc::now() - Duration::days(31))
        .bind(anna.id)
        .execute(db.pool())
        .await
        .unwrap();
    let e = auth::bearer(&state, &headers(Some("anna@example.test")))
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert!(e.message.contains("has not logged in"), "{}", e.message);
}

// ----- connecting a Google account ------------------------------------------

/// The token endpoint and userinfo, as the connect flow needs them.
async fn mount_google(server: &MockServer, email: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_token.json")))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth2/v3/userinfo"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sub": "104729384756102938475",
            "email": email,
            "email_verified": true
        })))
        .mount(server)
        .await;
}

/// Start a connect (or a reconnect) and hand back the consent state and the
/// cookie the callback will need.
async fn start(app: &Router, uri: &str, body: Value) -> (String, String) {
    let (s, b, h) = call(app, req("POST", uri, None, Some(body))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let url: url::Url = b["url"].as_str().unwrap().parse().unwrap();
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    // The consent URL points back at the route this router actually serves,
    // and at the public URL rather than at anything a header said.
    assert_eq!(
        query["redirect_uri"],
        "http://localhost:8000/api/google/callback"
    );
    assert_eq!(query["access_type"], "offline");
    (query["state"].clone(), cookie_header(&h))
}

async fn finish(app: &Router, state: &str, cookies: &str) -> String {
    let (s, _, h) = call(
        app,
        with_cookies(
            req(
                "GET",
                &format!("/api/google/callback?code=4/0Aexample&state={state}"),
                None,
                None,
            ),
            cookies,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    h[header::LOCATION].to_str().unwrap().to_string()
}

#[tokio::test]
async fn connecting_an_account_stores_a_sealed_grant() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_google(&server, "anna@example.test").await;
    let app = google_app(&db, &server);

    let (s, b, _) = call(&app, req("GET", "/api/me", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["google_configured"], true);
    let dev = db
        .get_user(b["user"]["id"].as_i64().unwrap())
        .await
        .unwrap();

    let (consent, cookies) = start(
        &app,
        "/api/connections/start",
        json!({"label": "work", "services": ["gmail", "docs"]}),
    )
    .await;
    assert!(
        cookies.contains("gmcp_google="),
        "the flow cookie: {cookies}"
    );

    let location = finish(&app, &consent, &cookies).await;
    let connections = db.list_connections(dev.id).await.unwrap();
    assert_eq!(connections.len(), 1);
    let c = &connections[0];
    assert_eq!(location, format!("/connections?connected={}", c.id));
    assert_eq!(c.label, "work");
    assert_eq!(c.google_email, "anna@example.test");
    // Docs pulled Drive in, and that is what is recorded.
    assert_eq!(c.services, ["gmail", "drive", "docs"]);
    assert_eq!(c.status, ConnectionStatus::Ok);
    assert!(
        c.granted_scopes
            .contains(&"https://www.googleapis.com/auth/gmail.modify".to_string()),
        "{:?}",
        c.granted_scopes
    );
    assert_eq!(
        seal::open(SECRET, &c.refresh_token_sealed).unwrap(),
        "1//09exampleRefreshTokenForTests",
        "the refresh token never touches the database in the clear"
    );

    // The list shows it, and says the grant is narrower than what was asked.
    let (s, b, _) = call(&app, req("GET", "/api/connections", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b[0]["label"], "work");
    assert_eq!(
        b[0]["partial"], true,
        "the fixture grants no documents scope"
    );
    assert!(b[0].get("refresh_token_sealed").is_none());

    // Renaming and the gateway flag are the two edits there are.
    let (s, b, _) = call(
        &app,
        req(
            "PATCH",
            &format!("/api/connections/{}", c.id),
            None,
            Some(json!({"label": "job", "delegate_ok": true})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "job");
    assert_eq!(b["delegate_ok"], true);

    // And the connect is in the log.
    let (_, b, _) = call(&app, req("GET", "/api/audit?kind=connect", None, None)).await;
    assert_eq!(b["entries"][0]["connection_id"], c.id);
    assert_eq!(b["entries"][0]["args"]["google_email"], "anna@example.test");
}

#[tokio::test]
async fn a_second_connection_of_the_same_google_account_is_refused() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_google(&server, "anna@example.test").await;
    let app = google_app(&db, &server);
    let (_, me, _) = call(&app, req("GET", "/api/me", None, None)).await;
    let dev = db
        .get_user(me["user"]["id"].as_i64().unwrap())
        .await
        .unwrap();
    connection(&db, &dev, "personal", "anna@example.test").await;

    let (consent, cookies) = start(
        &app,
        "/api/connections/start",
        json!({"label": "work", "services": ["gmail"]}),
    )
    .await;
    assert_eq!(
        finish(&app, &consent, &cookies).await,
        "/connections?error=already_connected"
    );
    assert_eq!(db.list_connections(dev.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_reconnect_that_comes_back_as_a_different_account_is_refused() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_google(&server, "someone.else@example.test").await;
    let app = google_app(&db, &server);
    let (_, me, _) = call(&app, req("GET", "/api/me", None, None)).await;
    let dev = db
        .get_user(me["user"]["id"].as_i64().unwrap())
        .await
        .unwrap();
    let existing = connection(&db, &dev, "work", "anna@example.test").await;

    let (consent, cookies) = start(
        &app,
        &format!("/api/connections/{}/reconnect", existing.id),
        json!({}),
    )
    .await;
    assert_eq!(
        finish(&app, &consent, &cookies).await,
        "/connections?error=different_account"
    );
    let after = db.get_connection(existing.id).await.unwrap();
    assert_eq!(after.google_email, "anna@example.test");
    assert_eq!(after.refresh_token_sealed, existing.refresh_token_sealed);
}

#[tokio::test]
async fn a_callback_without_its_flow_cookie_goes_back_with_an_error() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_google(&server, "anna@example.test").await;
    let app = google_app(&db, &server);
    let (s, _, h) = call(
        &app,
        req("GET", "/api/google/callback?code=x&state=y", None, None),
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h[header::LOCATION], "/connections?error=flow_expired");

    // A state that is not the one this server issued is refused too.
    let (consent, cookies) = start(
        &app,
        "/api/connections/start",
        json!({"label": "work", "services": ["gmail"]}),
    )
    .await;
    assert!(!consent.is_empty());
    let (s, _, h) = call(
        &app,
        with_cookies(
            req("GET", "/api/google/callback?code=x&state=wrong", None, None),
            &cookies,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(h[header::LOCATION], "/connections?error=state_mismatch");
}

#[tokio::test]
async fn without_google_credentials_connecting_says_so_and_nothing_else_breaks() {
    let db = Db::open_memory().await.unwrap();
    let app = dev_app(&db);
    let (s, b, _) = call(
        &app,
        req(
            "POST",
            "/api/connections/start",
            None,
            Some(json!({"label": "work", "services": ["gmail"]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["code"], "google_unconfigured");
    let (s, _, _) = call(&app, req("GET", "/api/connections", None, None)).await;
    assert_eq!(s, StatusCode::OK);
}

// ----- download links --------------------------------------------------------

/// A user, a connection, a token and a state, which is what a link needs.
async fn link_fixture(db: &Db, server: &MockServer) -> (AppState, Connection, ApiToken) {
    let anna = user(db, "anna", "anna@example.test").await;
    let c = connection(db, &anna, "work", "anna@example.test").await;
    let (t, _) = token_for(db, "claude", &["drive:read"], Some(&anna), anna.id).await;
    let state = state_of(
        db,
        config(AuthMode::Dev, "http://localhost:8000", mocked(server)),
    );
    (state, c, t)
}

#[tokio::test]
async fn minting_a_link_writes_the_capability_and_logs_it() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    let (state, c, t) = link_fixture(&db, &server).await;
    // An expired row from an earlier call is swept on the way past.
    db.create_link(NewLink {
        id: "expired0000000000000x".into(),
        connection_id: c.id,
        token_id: t.id,
        kind: LinkKind::DriveDownload,
        target: json!({"file_id": "old"}),
        filename: "old.pdf".into(),
        mime_type: "application/pdf".into(),
        size: None,
        expires_at: Utc::now() - Duration::minutes(1),
        uses_left: 3,
    })
    .await
    .unwrap();

    let minted = links::mint(
        &state,
        c.user_id,
        links::NewDownload {
            connection_id: c.id,
            token_id: t.id,
            target: links::Target::DriveDownload {
                file_id: "1AbC".into(),
            },
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: Some(12),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        minted.url,
        format!("http://localhost:8000/dl/{}", minted.id)
    );
    assert_eq!(minted.id.len(), domain_link::ID_LEN);
    let left = minted.expires_at - Utc::now();
    assert!(left > Duration::minutes(14) && left <= Duration::minutes(15));

    let row = db.take_link(&minted.id, Utc::now()).await.unwrap().unwrap();
    assert_eq!(row.uses_left, 2, "three uses, one spent by this check");
    assert_eq!(row.target["file_id"], "1AbC");

    let audit = db.list_audit(Default::default()).await.unwrap();
    assert_eq!(audit[0].kind, crate::db::AuditKind::LinkCreated);
    assert_eq!(audit[0].connection_id, Some(c.id));
    assert_eq!(audit[0].token_id, Some(t.id));
    assert!(audit[0].detail.as_ref().unwrap().contains("report.pdf"));

    // The expired one is gone.
    assert!(matches!(
        db.take_link("expired0000000000000x", Utc::now())
            .await
            .unwrap(),
        Err(crate::db::LinkRefusal::Unknown)
    ));
}

#[tokio::test]
async fn a_link_streams_from_google_once_it_is_hit() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_refresh.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1AbC"))
        .and(query_param("alt", "media"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"%PDF-1.7 a small report".to_vec())
                .insert_header("content-type", "application/pdf"),
        )
        .mount(&server)
        .await;
    let (state, c, t) = link_fixture(&db, &server).await;
    let app = router(state.clone());
    let minted = links::mint(
        &state,
        c.user_id,
        links::NewDownload {
            connection_id: c.id,
            token_id: t.id,
            target: links::Target::DriveDownload {
                file_id: "1AbC".into(),
            },
            filename: "zażółć raport.pdf".into(),
            mime_type: "application/pdf".into(),
            size: None,
        },
    )
    .await
    .unwrap();

    let (s, bytes, h) =
        call_bytes(&app, req("GET", &format!("/dl/{}", minted.id), None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bytes, b"%PDF-1.7 a small report");
    assert_eq!(h[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(h[header::CONTENT_LENGTH], "23");
    assert_eq!(
        h[header::CONTENT_DISPOSITION],
        "attachment; filename*=UTF-8''za%C5%BC%C3%B3%C5%82%C4%87%20raport.pdf"
    );

    let audit = db.list_audit(Default::default()).await.unwrap();
    assert_eq!(audit[0].kind, crate::db::AuditKind::LinkUsed);
    assert_eq!(audit[0].connection_id, Some(c.id));
    assert!(
        db.get_connection(c.id)
            .await
            .unwrap()
            .last_used_at
            .is_some()
    );
}

/// The route has no principal — the id is the permission — but the row it
/// spends belongs to somebody, and the Activity page filters on the person
/// looking at it. A hit logged against nobody is a hit nobody can see.
#[tokio::test]
async fn a_link_hit_shows_up_on_the_owners_activity_page() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_refresh.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1AbC"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"a report".to_vec()))
        .mount(&server)
        .await;
    // Dev mode is the person behind the browser, so the log has to name them.
    let me = auth::dev_user(&db).await.unwrap();
    let c = connection(&db, &me, "work", "dev@example.test").await;
    let (t, _) = token_for(&db, "claude", &["drive:read"], Some(&me), me.id).await;
    let state = state_of(
        &db,
        config(AuthMode::Dev, "http://localhost:8000", mocked(&server)),
    );
    let app = router(state.clone());
    let minted = links::mint(
        &state,
        me.id,
        links::NewDownload {
            connection_id: c.id,
            token_id: t.id,
            target: links::Target::DriveDownload {
                file_id: "1AbC".into(),
            },
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: None,
        },
    )
    .await
    .unwrap();
    let (s, _, _) = call_bytes(&app, req("GET", &format!("/dl/{}", minted.id), None, None)).await;
    assert_eq!(s, StatusCode::OK);
    // And a hit on a link that is past it belongs to the same person.
    db.create_link(NewLink {
        id: "expired0000000000000y".into(),
        connection_id: c.id,
        token_id: t.id,
        kind: LinkKind::DriveDownload,
        target: json!({"file_id": "1AbC"}),
        filename: "old.pdf".into(),
        mime_type: "application/pdf".into(),
        size: None,
        expires_at: Utc::now() - Duration::minutes(1),
        uses_left: 3,
    })
    .await
    .unwrap();
    let (s, _, _) = call(&app, req("GET", "/dl/expired0000000000000y", None, None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, b, _) = call(&app, req("GET", "/api/audit", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    let entries = b["entries"].as_array().unwrap();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["link_refused", "link_used", "link_created"]);
    for entry in entries {
        assert_eq!(entry["connection_id"], c.id);
        assert_eq!(entry["token_id"], t.id);
    }

    // A hit on an id that was never minted belongs to nobody, so it is not on
    // anyone's page.
    let (s, _, _) = call(&app, req("GET", "/dl/nonsense", None, None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, b, _) = call(&app, req("GET", "/api/audit", None, None)).await;
    assert_eq!(b["entries"].as_array().unwrap().len(), 3);
    assert_eq!(db.list_audit(Default::default()).await.unwrap().len(), 4);
}

/// The cap is the reason the link route can stream from Google at all; a
/// model that asks for a link to a 100 MB file is told so while it is still in
/// the tool call, not by a download that dies halfway.
#[tokio::test]
async fn a_file_over_the_download_cap_is_refused_at_mint() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    let (state, c, t) = link_fixture(&db, &server).await;
    let error = links::mint(
        &state,
        c.user_id,
        links::NewDownload {
            connection_id: c.id,
            token_id: t.id,
            target: links::Target::DriveDownload {
                file_id: "1AbC".into(),
            },
            filename: "huge.iso".into(),
            mime_type: "application/octet-stream".into(),
            size: Some(60 * 1024 * 1024),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("50 MB"), "{}", error.message);
    // Nothing was written down, so there is no link to hit.
    assert!(db.list_audit(Default::default()).await.unwrap().is_empty());
}

/// The stored type is whatever a mail header or an uploader said. A row from
/// before it was normalised at mint can hold anything, including bytes that
/// would end the header and start another one.
#[tokio::test]
async fn a_stored_mime_type_that_is_not_one_streams_as_octet_stream() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_refresh.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/1AbC"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"bytes".to_vec()))
        .mount(&server)
        .await;
    let (state, c, t) = link_fixture(&db, &server).await;
    let app = router(state);
    db.create_link(NewLink {
        id: "badmime00000000000000".into(),
        connection_id: c.id,
        token_id: t.id,
        kind: LinkKind::DriveDownload,
        target: json!({"file_id": "1AbC"}),
        filename: "report.pdf".into(),
        mime_type: "application/pdf\r\nX-Evil: 1".into(),
        size: Some(99),
        expires_at: Utc::now() + Duration::minutes(5),
        uses_left: 3,
    })
    .await
    .unwrap();

    let (s, bytes, h) = call_bytes(&app, req("GET", "/dl/badmime00000000000000", None, None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(bytes, b"bytes");
    assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(h["x-content-type-options"], "nosniff");
    // The length is the one this response carries, not the one recorded at
    // mint: the file may have changed, and a wrong one truncates the download.
    assert_eq!(h[header::CONTENT_LENGTH], "5");
}

#[tokio::test]
async fn the_three_refusals_are_one_answer() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    let (state, c, t) = link_fixture(&db, &server).await;
    let app = router(state);
    let make = async |id: &str, expires_at, uses_left| {
        db.create_link(NewLink {
            id: id.into(),
            connection_id: c.id,
            token_id: t.id,
            kind: LinkKind::DriveDownload,
            target: json!({"file_id": "1AbC"}),
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: None,
            expires_at,
            uses_left,
        })
        .await
        .unwrap();
    };
    make(
        "expired0000000000000x",
        Utc::now() - Duration::minutes(1),
        3,
    )
    .await;
    make(
        "spent000000000000000x",
        Utc::now() + Duration::minutes(5),
        0,
    )
    .await;

    let mut bodies = Vec::new();
    for id in ["nonsense", "expired0000000000000x", "spent000000000000000x"] {
        let (s, b, _) = call(&app, req("GET", &format!("/dl/{id}"), None, None)).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{id}");
        bodies.push(b);
    }
    assert_eq!(bodies[0], bodies[1], "an expired link looks unknown");
    assert_eq!(bodies[1], bodies[2], "a spent link looks unknown");
    assert_eq!(bodies[0]["error"]["code"], "not_found");

    // The log knows the difference, even though the caller does not.
    let refusals = db.list_audit(Default::default()).await.unwrap();
    let details: Vec<&str> = refusals
        .iter()
        .map(|e| e.detail.as_deref().unwrap())
        .collect();
    assert!(
        refusals
            .iter()
            .all(|e| e.kind == crate::db::AuditKind::LinkRefused)
    );
    assert!(
        details.iter().any(|d| d.contains("no such link")),
        "{details:?}"
    );
    assert!(details.iter().any(|d| d.contains("expired")), "{details:?}");
    assert!(
        details.iter().any(|d| d.contains("no uses left")),
        "{details:?}"
    );
}

// ----- the login flow --------------------------------------------------------

mod oidc_flow {
    use openidconnect::core::{
        CoreGenderClaim, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType, CoreRsaPrivateSigningKey,
        CoreSubjectIdentifierType,
    };
    use openidconnect::{
        Audience, AuthUrl, EmptyAdditionalProviderMetadata, EndUserEmail, IdToken, IdTokenClaims,
        IssuerUrl, JsonWebKeyId, JsonWebKeySetUrl, Nonce, PrivateSigningKey, ResponseTypes,
        StandardClaims, SubjectIdentifier, TokenUrl,
    };

    use super::*;
    use crate::http::oidc::GroupsClaims;

    struct Issuer {
        server: MockServer,
        key: CoreRsaPrivateSigningKey,
    }

    async fn issuer() -> Issuer {
        let server = MockServer::start().await;
        let pem = include_str!("../../tests/fixtures/oidc-test-key.pem");
        let key = CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new("test".into())))
            .unwrap();
        let base = server.uri();
        let metadata = CoreProviderMetadata::new(
            IssuerUrl::new(base.clone()).unwrap(),
            AuthUrl::new(format!("{base}/authorize")).unwrap(),
            JsonWebKeySetUrl::new(format!("{base}/jwks")).unwrap(),
            vec![ResponseTypes::new(vec![CoreResponseType::Code])],
            vec![CoreSubjectIdentifierType::Public],
            vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            EmptyAdditionalProviderMetadata {},
        )
        .set_token_endpoint(Some(TokenUrl::new(format!("{base}/token")).unwrap()));
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&metadata))
            .mount(&server)
            .await;
        let jwks = CoreJsonWebKeySet::new(vec![key.as_verification_key()]);
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&jwks))
            .mount(&server)
            .await;
        Issuer { server, key }
    }

    impl Issuer {
        async fn mount_token(&self, nonce: &str, groups: &[&str]) {
            let now = Utc::now();
            let claims: IdTokenClaims<GroupsClaims, CoreGenderClaim> = IdTokenClaims::new(
                IssuerUrl::new(self.server.uri()).unwrap(),
                vec![Audience::new("client".into())],
                now + Duration::minutes(5),
                now,
                StandardClaims::<CoreGenderClaim>::new(SubjectIdentifier::new("subject-1".into()))
                    .set_email(Some(EndUserEmail::new("k@example.test".into()))),
                GroupsClaims {
                    groups: groups.iter().map(|g| g.to_string()).collect(),
                },
            )
            .set_nonce(Some(Nonce::new(nonce.to_string())));
            let token: IdToken<
                GroupsClaims,
                CoreGenderClaim,
                CoreJweContentEncryptionAlgorithm,
                CoreJwsSigningAlgorithm,
            > = IdToken::new(
                claims,
                &self.key,
                CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
                None,
                None,
            )
            .unwrap();
            let body = json!({"access_token": "at", "token_type": "Bearer", "id_token": token.to_string()});
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&self.server)
                .await;
        }
    }

    async fn oidc_app(db: &Db, iss: &Issuer, group: Option<&str>) -> Router {
        let cfg = config(
            AuthMode::Oidc(OidcConfig {
                issuer: iss.server.uri(),
                client_id: "client".into(),
                client_secret: "secret".into(),
                group: group.map(String::from),
            }),
            "http://localhost:8000",
            unconfigured(),
        );
        router(AppState::new(cfg, db.clone()).await.unwrap())
    }

    async fn start_login(app: &Router) -> (String, String, String) {
        let (s, _, h) = call(
            app,
            req("GET", "/api/auth/login?next=/connections", None, None),
        )
        .await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let location: url::Url = h[header::LOCATION].to_str().unwrap().parse().unwrap();
        let q: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
        assert_eq!(q["redirect_uri"], "http://localhost:8000/api/auth/callback");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(q["scope"].contains("openid"));
        (q["state"].clone(), q["nonce"].clone(), cookie_header(&h))
    }

    #[tokio::test]
    async fn a_member_logs_in_and_gets_a_session() {
        let db = Db::open_memory().await.unwrap();
        let iss = issuer().await;
        let app = oidc_app(&db, &iss, Some("gmcp")).await;
        let (state, nonce, cookies) = start_login(&app).await;
        iss.mount_token(&nonce, &["staff", "gmcp"]).await;

        let (s, _, h) = call(
            &app,
            with_cookies(
                req(
                    "GET",
                    &format!("/api/auth/callback?code=abc&state={state}"),
                    None,
                    None,
                ),
                &cookies,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(h[header::LOCATION], "/connections");
        let session = cookie_header(&h);
        assert!(session.contains("gmcp_session="));

        let (s, b, _) = call(
            &app,
            with_cookies(req("GET", "/api/me", None, None), &session),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["kind"], "session");
        assert_eq!(b["user"]["email"], "k@example.test");
        assert_eq!(b["connections"], 0);

        let (s, _, h) = call(
            &app,
            with_cookies(req("POST", "/api/auth/logout", None, None), &session),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(cookie_header(&h).contains("gmcp_session="));
    }

    #[tokio::test]
    async fn a_non_member_is_refused_and_no_user_is_created() {
        let db = Db::open_memory().await.unwrap();
        let iss = issuer().await;
        let app = oidc_app(&db, &iss, Some("gmcp")).await;
        let (state, nonce, cookies) = start_login(&app).await;
        iss.mount_token(&nonce, &["staff"]).await;
        let (s, _, h) = call(
            &app,
            with_cookies(
                req(
                    "GET",
                    &format!("/api/auth/callback?code=abc&state={state}"),
                    None,
                    None,
                ),
                &cookies,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(!cookie_header(&h).contains("gmcp_session="));
        assert!(db.list_users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_configured_group_anyone_of_the_issuer_may_log_in() {
        let db = Db::open_memory().await.unwrap();
        let iss = issuer().await;
        let app = oidc_app(&db, &iss, None).await;
        let (state, nonce, cookies) = start_login(&app).await;
        iss.mount_token(&nonce, &["staff"]).await;
        let (s, _, h) = call(
            &app,
            with_cookies(
                req(
                    "GET",
                    &format!("/api/auth/callback?code=abc&state={state}"),
                    None,
                    None,
                ),
                &cookies,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert!(cookie_header(&h).contains("gmcp_session="));
        assert_eq!(db.list_users().await.unwrap().len(), 1);
    }

    /// The provider's own `error` and `error_description` land in the query
    /// string of a link anyone can send around, and the page is served as
    /// text/html from the portal origin, where a script would reach the
    /// session cookie and mint a bearer token.
    #[tokio::test]
    async fn a_provider_error_is_never_reflected_into_the_page() {
        let db = Db::open_memory().await.unwrap();
        let iss = issuer().await;
        let app = oidc_app(&db, &iss, Some("gmcp")).await;
        let (s, body, h) = call_bytes(
            &app,
            req(
                "GET",
                "/api/auth/callback?error=%3Cscript%3Ealert(1)%3C/script%3E\
                 &error_description=%3Cimg%20src=x%20onerror=alert(2)%3E",
                None,
                None,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(h[header::CONTENT_TYPE].to_str().unwrap().contains("html"));
        let html = String::from_utf8(body).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("alert(1)"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
        assert!(html.contains("The identity provider rejected the login."));
    }

    #[tokio::test]
    async fn a_state_mismatch_and_a_missing_cookie_fail() {
        let db = Db::open_memory().await.unwrap();
        let iss = issuer().await;
        let app = oidc_app(&db, &iss, Some("gmcp")).await;
        let (_state, nonce, cookies) = start_login(&app).await;
        iss.mount_token(&nonce, &["gmcp"]).await;
        let (s, _, _) = call(
            &app,
            with_cookies(
                req("GET", "/api/auth/callback?code=abc&state=wrong", None, None),
                &cookies,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = call(
            &app,
            req("GET", "/api/auth/callback?code=abc&state=x", None, None),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
