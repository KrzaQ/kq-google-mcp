//! JSON-RPC over the real router: bearer resolution, the per-token listing,
//! the refusals, and the tools themselves against a wiremock Google.
//!
//! The client here is the transport's own protocol rather than a mock of it —
//! every request goes through `require_bearer`, the streamable HTTP service
//! and rmcp's dispatch — because the things worth testing (who the principal
//! is, which tools exist for this token, what the content blocks look like on
//! the wire) all live in that path.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method as http_method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::{AuthMode, Config, GoogleConfig};
use crate::db::{
    ApiToken, AuditKind, AuditOutcome, ClientProfile, Connection, ConnectionStatus, Db,
    NewConnection, NewToken, User,
};
use crate::domain::{seal, token as domain_token};
use crate::http::{AppState, router};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/google/");
const HOST: &str = "gmcp.example";

fn fixture(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("{FIXTURES}{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("fixture {name} is not JSON: {e}"))
}

fn google_config(server: Option<&MockServer>) -> GoogleConfig {
    let base: url::Url = match server {
        Some(s) => s.uri().parse().unwrap(),
        None => "https://www.googleapis.com".parse().unwrap(),
    };
    GoogleConfig {
        client_id: Some("gmcp-test.apps.googleusercontent.com".into()),
        client_secret: Some("test-client-secret".into()),
        api_base: base.clone(),
        oauth_base: base.clone(),
        accounts_base: base,
    }
}

async fn app(db: &Db, server: Option<&MockServer>) -> Router {
    let config = Config {
        database: std::path::PathBuf::new(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: format!("https://{HOST}").parse().unwrap(),
        secret: SECRET.to_vec(),
        // Dev mode only decides that no identity provider is discovered; `/mcp`
        // has no cookie fallback either way.
        auth: AuthMode::Dev,
        google: google_config(server),
        auto_migrate: false,
    };
    let state = AppState::new(config, db.clone()).await.unwrap();
    router(state)
}

/// The refresh every Google call starts with. Mounted once per server.
async fn mount_token(server: &MockServer) {
    Mock::given(http_method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_refresh.json")))
        .mount(server)
        .await;
}

/// Nothing this server does may reach a send endpoint. Every test that writes
/// a draft mounts this, and wiremock fails the test if it is ever hit.
fn expect_no_send() -> Vec<Mock> {
    vec![
        Mock::given(path_regex(r".*/send$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .named("nothing is ever sent"),
        Mock::given(path_regex(r".*/trash$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .named("nothing is ever trashed"),
    ]
}

// ----- the JSON-RPC client ---------------------------------------------------

struct Client {
    app: Router,
    token: String,
    delegate_user: Option<String>,
    session: Option<String>,
    next_id: u64,
}

impl Client {
    fn new(app: Router, token: impl Into<String>) -> Self {
        Self {
            app,
            token: token.into(),
            delegate_user: None,
            session: None,
            next_id: 0,
        }
    }

    fn acting_for(mut self, email: &str) -> Self {
        self.delegate_user = Some(email.to_string());
        self
    }

    fn request(&self, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        if let Some(u) = &self.delegate_user {
            b = b.header("x-gmcp-user", u);
        }
        if let Some(s) = &self.session {
            b = b.header("mcp-session-id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn rpc(&mut self, method: &str, params: Value) -> (StatusCode, Value) {
        self.next_id += 1;
        let body =
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        let resp = self.app.clone().oneshot(self.request(body)).await.unwrap();
        let status = resp.status();
        if let Some(s) = resp.headers().get("mcp-session-id") {
            self.session = Some(s.to_str().unwrap().to_string());
        }
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, parse_body(&bytes))
    }

    async fn notify(&mut self, method: &str) {
        let body = json!({"jsonrpc": "2.0", "method": method});
        let resp = self.app.clone().oneshot(self.request(body)).await.unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
    }

    async fn initialize(&mut self) -> Value {
        let (status, v) = self
            .rpc(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "0"}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        self.notify("notifications/initialized").await;
        v["result"].clone()
    }

    async fn tools(&mut self) -> Vec<Value> {
        let (status, v) = self.rpc("tools/list", json!({})).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        v["result"]["tools"].as_array().cloned().unwrap_or_default()
    }

    async fn names(&mut self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools()
            .await
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    async fn call(&mut self, name: &str, args: Value) -> Value {
        let (status, v) = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        v
    }

    /// A call that must have worked, with its structured content.
    async fn ok(&mut self, name: &str, args: Value) -> Value {
        let v = self.call(name, args).await;
        assert!(!is_error(&v), "{name} failed: {}", error_text(&v));
        v["result"]["structuredContent"].clone()
    }

    /// A call that must have been refused, with the message.
    async fn refused(&mut self, name: &str, args: Value) -> String {
        let v = self.call(name, args).await;
        assert!(is_error(&v), "{name} was not refused: {v}");
        error_text(&v)
    }
}

/// Plain JSON, or the JSON-RPC message inside an SSE stream: rmcp answers a
/// session-mode POST with `data:` frames.
fn parse_body(bytes: &[u8]) -> Value {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return v;
    }
    let text = String::from_utf8_lossy(bytes);
    let mut last = Value::String(text.to_string());
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
            && (v.get("result").is_some() || v.get("error").is_some())
        {
            last = v;
        }
    }
    last
}

fn is_error(v: &Value) -> bool {
    v["error"].is_object() || v["result"]["isError"] == true
}

fn error_text(v: &Value) -> String {
    if let Some(m) = v["error"]["message"].as_str() {
        return m.to_string();
    }
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// ----- fixtures --------------------------------------------------------------

async fn user(db: &Db, subject: &str, email: &str) -> User {
    let u = db
        .upsert_user(subject, Some(email), Some(subject))
        .await
        .unwrap();
    db.touch_last_login(u.id).await.unwrap();
    db.get_user(u.id).await.unwrap()
}

async fn connect(
    db: &Db,
    owner: &User,
    label: &str,
    services: &[&str],
    delegate_ok: bool,
) -> Connection {
    db.create_connection(NewConnection {
        user_id: owner.id,
        label: label.into(),
        google_email: format!("{label}@example.test"),
        services: services.iter().map(|s| s.to_string()).collect(),
        granted_scopes: vec!["openid".into()],
        refresh_token_sealed: seal::seal(SECRET, "1//09exampleRefreshTokenForTests"),
        delegate_ok,
    })
    .await
    .unwrap()
}

async fn token(
    db: &Db,
    scopes: &[&str],
    owner: Option<&User>,
    client: ClientProfile,
) -> (ApiToken, String) {
    let created_by = owner.map(|u| u.id).unwrap_or(1);
    let new = domain_token::generate();
    let row = db
        .create_token(NewToken {
            name: "test".into(),
            token_hash: new.hash,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            client,
            user_id: owner.map(|u| u.id),
            all_connections: true,
            created_by,
        })
        .await
        .unwrap();
    (row, new.secret)
}

/// One person, one account with everything connected, one token.
async fn one_of_everything(
    db: &Db,
    scopes: &[&str],
    client: ClientProfile,
) -> (User, Connection, String) {
    let anna = user(db, "anna", "anna@example.test").await;
    let work = connect(
        db,
        &anna,
        "work",
        &["gmail", "drive", "docs", "sheets", "calendar"],
        true,
    )
    .await;
    let (_, secret) = token(db, scopes, Some(&anna), client).await;
    (anna, work, secret)
}

const EVERYTHING: &[&str] = &[
    "gmail:read",
    "gmail:draft",
    "gmail:modify",
    "drive:read",
    "docs:read",
    "docs:write",
    "sheets:read",
    "sheets:write",
    "calendar:read",
    "calendar:write",
];

// ----- auth ------------------------------------------------------------------

#[tokio::test]
async fn mcp_is_bearer_only_and_says_so_in_json() {
    let db = Db::open_memory().await.unwrap();
    let app = app(&db, None).await;
    let bare = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(bare).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    // A secret that is not a token gets the same answer.
    let (status, _) = Client::new(app, "gg_nonsense")
        .rpc("initialize", json!({}))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_delegate_token_needs_the_header_and_then_sees_only_gateway_connections() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    let bob = user(&db, "bob", "bob@example.test").await;
    connect(&db, &anna, "work", &["gmail"], true).await;
    connect(&db, &anna, "personal", &["gmail"], false).await;
    connect(&db, &bob, "bobs", &["gmail"], true).await;
    let (_, secret) = token(
        &db,
        &["gmail:read", "delegate"],
        None,
        ClientProfile::OpenWebUi,
    )
    .await;

    // Without X-Gmcp-User there is nothing to act as, and the middleware says so.
    let app = app(&db, None).await;
    let mut anonymous = Client::new(app.clone(), secret.clone());
    let (status, body) = anonymous.rpc("initialize", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("X-Gmcp-User"),
        "{body}"
    );

    let mut c = Client::new(app, secret).acting_for("anna@example.test");
    c.initialize().await;
    let out = c.ok("list_accounts", json!({})).await;
    let labels: Vec<&str> = out["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["work"],
        "only the gateway connections of the named user"
    );
}

// ----- the per-token listing --------------------------------------------------

#[tokio::test]
async fn a_gmail_read_token_lists_the_gmail_read_tools_and_nothing_else() {
    let db = Db::open_memory().await.unwrap();
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::ClaudeCode).await;
    let mut c = Client::new(app(&db, None).await, secret);
    let info = c.initialize().await;
    assert_eq!(info["serverInfo"]["name"], "gmcp");
    let instructions = info["instructions"].as_str().unwrap();
    for phrase in [
        "NEVER SENDS MAIL",
        "list_accounts first",
        "confirmed=true",
        "15 minutes",
        "MAX_MCP_OUTPUT_TOKENS",
        "https://gmcp.example/connections",
    ] {
        assert!(
            instructions.contains(phrase),
            "instructions lack {phrase:?}"
        );
    }

    assert_eq!(
        c.names().await,
        [
            "gmail_attachment_link",
            "gmail_attachment_text",
            "gmail_get_message",
            "gmail_get_thread",
            "gmail_list_labels",
            "gmail_search",
            "gmail_view_image",
            "list_accounts",
        ]
    );
}

#[tokio::test]
async fn a_hidden_tool_is_a_clean_error_that_names_the_scope() {
    let db = Db::open_memory().await.unwrap();
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;
    let message = c
        .refused(
            "sheets_append_rows",
            json!({"account": "work", "spreadsheet_id": "1", "tab": "Q3",
                   "rows": [["a"]], "confirmed": true}),
        )
        .await;
    assert!(message.contains("sheets:write"), "{message}");
    assert!(message.contains("gmail:read"), "{message}");
    // And a name that is no tool at all is told so rather than left to guess.
    assert!(
        c.refused("gmail_send", json!({})).await.contains("no tool"),
        "an unknown tool name"
    );
}

#[tokio::test]
async fn the_write_tools_carry_the_house_rules_in_their_schemas() {
    let db = Db::open_memory().await.unwrap();
    let (_, _, secret) = one_of_everything(&db, EVERYTHING, ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;
    let tools = c.tools().await;
    assert_eq!(tools.len(), crate::domain::scope::TOOLS.len());
    let by_name = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("no {name}"))
            .clone()
    };

    // The refusal that matters most is the one that cannot be argued with:
    // there is no attendees property to fill in.
    let create = by_name("calendar_create_event");
    let properties = &create["inputSchema"]["properties"];
    assert!(properties["attendees"].is_null(), "{properties}");
    assert!(properties["confirmed"].is_object());
    assert!(
        create["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("confirmed"))
    );
    assert!(
        create["description"]
            .as_str()
            .unwrap()
            .contains("no attendees argument")
    );
    for tool in ["calendar_update_event", "calendar_delete_event"] {
        assert!(by_name(tool)["inputSchema"]["properties"]["attendees"].is_null());
    }
    // A draft is its own confirmation, so it has no such argument.
    assert!(by_name("gmail_create_draft")["inputSchema"]["properties"]["confirmed"].is_null());
}

// ----- resolving `account` ----------------------------------------------------

#[tokio::test]
async fn an_account_is_refused_by_name_by_health_and_by_service() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    connect(&db, &anna, "work", &["gmail"], true).await;
    let stale = connect(&db, &anna, "personal", &["gmail", "calendar"], true).await;
    db.set_connection_status(
        stale.id,
        ConnectionStatus::NeedsReauth,
        Some("invalid_grant"),
    )
    .await
    .unwrap();
    let (_, secret) = token(&db, EVERYTHING, Some(&anna), ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;

    let unknown = c
        .refused("gmail_list_labels", json!({"account": "office"}))
        .await;
    assert!(unknown.contains("no account called `office`"), "{unknown}");
    assert!(
        unknown.contains("`work`") && unknown.contains("`personal`"),
        "{unknown}"
    );

    let reauth = c
        .refused("gmail_list_labels", json!({"account": "personal"}))
        .await;
    assert!(
        reauth.contains("https://gmcp.example/connections"),
        "{reauth}"
    );
    assert!(reauth.contains("connected again"), "{reauth}");

    let missing = c.refused("calendar_list", json!({"account": "work"})).await;
    assert_eq!(
        missing,
        "connection `work` has no `calendar` service; it was connected with `gmail`. \
         The person can add it by reconnecting the account at https://gmcp.example/connections"
    );
}

// ----- reading ----------------------------------------------------------------

#[tokio::test]
async fn reading_mail_answers_a_small_shape_and_logs_the_call() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .and(query_param("format", "full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&server)
        .await;
    let (anna, work, secret) =
        one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let out = c
        .ok(
            "gmail_get_message",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6"}),
        )
        .await;
    assert_eq!(out["subject"], "Q3 figures for Kraków");
    assert_eq!(
        out["rfc822_message_id"],
        "<CAF7n2sabc123@mail.example.test>"
    );
    assert_eq!(out["attachments"][0]["filename"], "q3-figures.pdf");
    assert_eq!(
        out["inline_images"][0]["content_id"],
        "chart-q3@example.test"
    );
    // Google's own payload is nowhere in the answer.
    assert!(out["payload"].is_null());

    let rows = db.list_audit(Default::default()).await.unwrap();
    let row = rows.iter().find(|r| r.kind == AuditKind::ToolCall).unwrap();
    assert_eq!(row.tool.as_deref(), Some("gmail_get_message"));
    assert_eq!(row.outcome, AuditOutcome::Ok);
    assert_eq!(row.connection_id, Some(work.id));
    assert_eq!(row.user_id, Some(anna.id));
    assert_eq!(row.args.as_ref().unwrap()["account"], "work");
    assert!(row.duration_ms.is_some());
    assert!(
        db.get_connection(work.id)
            .await
            .unwrap()
            .last_used_at
            .is_some()
    );
}

#[tokio::test]
async fn a_refused_call_is_logged_as_forbidden() {
    let db = Db::open_memory().await.unwrap();
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;
    c.refused("gmail_list_labels", json!({"account": "nope"}))
        .await;
    let rows = db.list_audit(Default::default()).await.unwrap();
    let row = rows.iter().find(|r| r.kind == AuditKind::ToolCall).unwrap();
    assert_eq!(row.outcome, AuditOutcome::Forbidden);
    assert!(row.detail.as_deref().unwrap().contains("no account called"));
}

// ----- images -----------------------------------------------------------------

/// A small PNG that survives a round trip through the image pipeline.
fn png() -> Vec<u8> {
    use image::{ImageFormat, Rgb, RgbImage};
    let img = RgbImage::from_fn(80, 60, |x, y| Rgb([(x * 3) as u8, (y * 4) as u8, 120]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

async fn mount_picture(server: &MockServer) {
    use base64::Engine;
    mount_token(server).await;
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(server)
        .await;
    Mock::given(http_method("GET"))
        .and(path(
            "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/attachments/ANGjdJ8chartPNG",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "size": png().len(),
            "data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(png()),
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_picture_comes_back_in_the_shape_each_client_can_see() {
    for (profile, blocks) in [
        (ClientProfile::ClaudeCode, 2),
        (ClientProfile::OpenCode, 2),
        (ClientProfile::Generic, 2),
        (ClientProfile::OpenWebUi, 3),
    ] {
        let db = Db::open_memory().await.unwrap();
        let server = MockServer::start().await;
        mount_picture(&server).await;
        let (_, work, secret) = one_of_everything(&db, &["gmail:read"], profile).await;
        let mut c = Client::new(app(&db, Some(&server)).await, secret);
        c.initialize().await;

        // By Content-ID, which is how an inline picture is named in the body.
        let v = c
            .call(
                "gmail_view_image",
                json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                       "attachment_id": "chart-q3@example.test"}),
            )
            .await;
        assert!(!is_error(&v), "{profile}: {}", error_text(&v));
        let result = &v["result"];
        assert!(
            result["structuredContent"].is_null(),
            "{profile} must never get structuredContent: {result}"
        );
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), blocks, "{profile}: {content:?}");
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("only in this turn")
        );
        assert!(content[0]["text"].as_str().unwrap().contains("chart.png"));
        assert_eq!(content[1]["type"], "image");
        assert!(
            content[1]["mimeType"]
                .as_str()
                .unwrap()
                .starts_with("image/"),
            "{content:?}"
        );
        assert!(!content[1]["data"].as_str().unwrap().is_empty());
        if profile == ClientProfile::OpenWebUi {
            // The one block Open WebUI's model actually receives.
            assert_eq!(content[2]["type"], "resource");
            let resource = &content[2]["resource"];
            assert_eq!(
                resource["uri"],
                format!("gmcp://{}/gmail/18f0a1b2c3d4e5f6/ANGjdJ8chartPNG", work.id)
            );
            assert_eq!(resource["mimeType"], content[1]["mimeType"]);
            assert_eq!(resource["blob"], content[1]["data"]);
        }
    }
}

#[tokio::test]
async fn a_picture_that_is_not_there_is_refused_with_what_is() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_picture(&server).await;
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;
    let message = c
        .refused(
            "gmail_view_image",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "nope"}),
        )
        .await;
    assert!(message.contains("chart.png"), "{message}");
}

// ----- drafts -----------------------------------------------------------------

#[tokio::test]
async fn writing_a_draft_never_touches_a_send_endpoint() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    for mock in expect_no_send() {
        server.register(mock).await;
    }
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&server)
        .await;
    Mock::given(http_method("POST"))
        .and(path("/gmail/v1/users/me/drafts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_draft.json")))
        .mount(&server)
        .await;
    let (_, _, secret) =
        one_of_everything(&db, &["gmail:read", "gmail:draft"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let out = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"],
                   "subject": "Q3", "body": "here they are"}),
        )
        .await;
    assert_eq!(out["draft_id"], "r-8812345678901234567");
    assert!(out["url"].as_str().unwrap().contains("#drafts?compose="));
    assert!(out["note"].as_str().unwrap().contains("nothing was sent"));

    let reply = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "body": "thanks", "reply_all": true}),
        )
        .await;
    assert_eq!(reply["thread_id"], "18f0a1b2c3d4e5f0");
    // The mocks with expect(0) are verified when the server is dropped.
    drop(server);
}

// ----- confirmation -----------------------------------------------------------

#[tokio::test]
async fn an_unconfirmed_sheet_write_previews_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    // If the tool wrote anything at all, this would be hit.
    server
        .register(
            Mock::given(path_regex(r"/v4/spreadsheets/.*"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(fixture("sheets_append.json")),
                )
                .expect(0)
                .named("an unconfirmed write reaches Google"),
        )
        .await;
    let (_, _, secret) = one_of_everything(
        &db,
        &["sheets:read", "sheets:write"],
        ClientProfile::Generic,
    )
    .await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let out = c
        .ok(
            "sheets_append_rows",
            json!({"account": "work", "spreadsheet_id": "1SpR", "tab": "September",
                   "rows": [["2026-09-08", "PHX", "1.5"], ["2026-09-09", "PHX", "2"]],
                   "confirmed": false}),
        )
        .await;
    assert_eq!(out["confirmed"], false);
    assert_eq!(out["written"], false);
    assert!(
        out["action"]
            .as_str()
            .unwrap()
            .contains("append 2 rows to the tab \"September\""),
        "{out}"
    );
    assert_eq!(out["details"][0], "2026-09-08 | PHX | 1.5");
    assert!(out["next"].as_str().unwrap().contains("confirmed=true"));
    drop(server);
}

#[tokio::test]
async fn a_confirmed_sheet_write_goes_through_and_reports_the_range() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"/v4/spreadsheets/1SpR/values/September:append"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_append.json")))
        .expect(1)
        .mount(&server)
        .await;
    let (_, _, secret) = one_of_everything(
        &db,
        &["sheets:read", "sheets:write"],
        ClientProfile::Generic,
    )
    .await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;
    let out = c
        .ok(
            "sheets_append_rows",
            json!({"account": "work", "spreadsheet_id": "1SpR", "tab": "September",
                   "rows": [["2026-09-08", "PHX", "1.5"]], "confirmed": true}),
        )
        .await;
    assert_eq!(out["updated_range"], "September!A5:D5");
    assert_eq!(out["updated_rows"], 1);
    drop(server);
}

// ----- calendar ---------------------------------------------------------------

#[tokio::test]
async fn an_event_with_attendees_can_be_read_but_not_changed_or_deleted() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("GET"))
        .and(path(
            "/calendar/v3/calendars/primary/events/7z8y9x0w1v2u3t4s5r6q7p8o",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("calendar_event_with_attendees.json")),
        )
        .mount(&server)
        .await;
    // Neither a patch nor a delete may ever be attempted.
    for m in ["PATCH", "DELETE"] {
        server
            .register(
                Mock::given(http_method(m))
                    .and(path_regex(r"/calendar/v3/calendars/.*"))
                    .respond_with(ResponseTemplate::new(200))
                    .expect(0)
                    .named("an event with attendees is never touched"),
            )
            .await;
    }
    let (_, _, secret) = one_of_everything(
        &db,
        &["calendar:read", "calendar:write"],
        ClientProfile::Generic,
    )
    .await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let event = json!({"account": "work", "event_id": "7z8y9x0w1v2u3t4s5r6q7p8o"});
    let read = c.ok("calendar_get_event", event.clone()).await;
    assert_eq!(read["attendees"], 2);
    assert_eq!(read["title"], "Phoenix handover");

    let mut update = event.as_object().unwrap().clone();
    update.insert("title".into(), json!("Moved"));
    update.insert("confirmed".into(), json!(true));
    let refused = c
        .refused("calendar_update_event", Value::Object(update))
        .await;
    assert!(refused.contains("attendees"), "{refused}");
    assert!(refused.contains("their calendars"), "{refused}");

    let mut delete = event.as_object().unwrap().clone();
    delete.insert("confirmed".into(), json!(true));
    let refused = c
        .refused("calendar_delete_event", Value::Object(delete))
        .await;
    assert!(refused.contains("attendees"), "{refused}");
    drop(server);
}

#[tokio::test]
async fn creating_an_event_previews_first_and_then_notifies_nobody() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("POST"))
        .and(path("/calendar/v3/calendars/primary/events"))
        .and(query_param("sendUpdates", "none"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("calendar_event_created.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (_, _, secret) = one_of_everything(
        &db,
        &["calendar:read", "calendar:write"],
        ClientProfile::Generic,
    )
    .await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let args = json!({"account": "work", "title": "Deep work",
                      "start": "2026-09-10T09:00:00Z", "end": "2026-09-10T11:00:00Z"});
    let mut unconfirmed = args.as_object().unwrap().clone();
    unconfirmed.insert("confirmed".into(), json!(false));
    let preview = c
        .ok("calendar_create_event", Value::Object(unconfirmed))
        .await;
    assert_eq!(preview["written"], false);
    assert!(preview["action"].as_str().unwrap().contains("Deep work"));
    assert!(
        preview["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("nobody is invited"))
    );

    let mut confirmed = args.as_object().unwrap().clone();
    confirmed.insert("confirmed".into(), json!(true));
    let done = c
        .ok("calendar_create_event", Value::Object(confirmed))
        .await;
    assert!(
        done["note"]
            .as_str()
            .unwrap()
            .contains("nobody was notified")
    );
    assert!(done["event"]["event_id"].is_string());
    drop(server);
}

// ----- labels -----------------------------------------------------------------

#[tokio::test]
async fn labels_are_resolved_by_name_and_the_bin_is_refused() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_labels.json")))
        .mount(&server)
        .await;
    Mock::given(http_method("POST"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/modify"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_modified.json")),
        )
        .mount(&server)
        .await;
    let (_, _, secret) =
        one_of_everything(&db, &["gmail:read", "gmail:modify"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let out = c
        .ok(
            "gmail_modify_labels",
            json!({"account": "work", "message_ids": ["18f0a1b2c3d4e5f6"],
                   "add": ["Invoices"], "archive": true, "mark_read": true}),
        )
        .await;
    assert_eq!(out["added"], json!(["Label_18"]));
    assert_eq!(out["removed"], json!(["INBOX", "UNREAD"]));
    assert_eq!(out["modified"], 1);

    for label in ["TRASH", "spam"] {
        let refused = c
            .refused(
                "gmail_modify_labels",
                json!({"account": "work", "message_ids": ["18f0a1b2c3d4e5f6"],
                       "add": [label]}),
            )
            .await;
        assert!(refused.contains("never moves mail to"), "{refused}");
    }
    let unknown = c
        .refused(
            "gmail_modify_labels",
            json!({"account": "work", "message_ids": ["18f0a1b2c3d4e5f6"],
                   "add": ["Nowhere"]}),
        )
        .await;
    assert!(unknown.contains("Invoices"), "{unknown}");
}

// ----- links ------------------------------------------------------------------

#[tokio::test]
async fn an_attachment_link_is_minted_against_the_token_and_expires() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&server)
        .await;
    let (_, work, secret) =
        one_of_everything(&db, &["gmail:read"], ClientProfile::ClaudeCode).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;
    let out = c
        .ok(
            "gmail_attachment_link",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "ANGjdJ8pdfQ3"}),
        )
        .await;
    assert!(
        out["url"]
            .as_str()
            .unwrap()
            .starts_with("https://gmcp.example/dl/"),
        "{out}"
    );
    assert_eq!(out["filename"], "q3-figures.pdf");
    assert_eq!(out["mime_type"], "application/pdf");
    assert!(out["note"].as_str().unwrap().contains("15 minutes"));
    let expires: chrono::DateTime<Utc> = out["expires_at"].as_str().unwrap().parse().unwrap();
    assert!(expires > Utc::now());

    let rows = db.list_audit(Default::default()).await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.kind == AuditKind::LinkCreated && r.connection_id == Some(work.id))
    );
}

/// The whole point of the `Arc` around the extractor: one server instance,
/// cloned per session, serves every token.
#[tokio::test]
async fn one_server_instance_serves_two_tokens_with_different_scopes() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    connect(&db, &anna, "work", &["gmail", "calendar"], true).await;
    let (_, reader) = token(&db, &["gmail:read"], Some(&anna), ClientProfile::Generic).await;
    let (_, writer) = token(
        &db,
        &["calendar:read", "calendar:write"],
        Some(&anna),
        ClientProfile::Generic,
    )
    .await;
    let app = Arc::new(app(&db, None).await);

    let mut a = Client::new((*app).clone(), reader);
    a.initialize().await;
    let mut b = Client::new((*app).clone(), writer);
    b.initialize().await;
    assert!(a.names().await.contains(&"gmail_search".to_string()));
    assert!(!a.names().await.contains(&"calendar_list".to_string()));
    assert!(
        b.names()
            .await
            .contains(&"calendar_create_event".to_string())
    );
    assert!(!b.names().await.contains(&"gmail_search".to_string()));
}

// ----- a grant that stops working halfway through a call ---------------------

/// A server for the other services: the token endpoint and nothing else.
async fn google_server() -> MockServer {
    let server = MockServer::start().await;
    mount_token(&server).await;
    server
}

#[tokio::test]
async fn a_grant_google_refuses_mid_call_names_the_account_and_the_portal() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    Mock::given(http_method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("oauth_invalid_grant.json")))
        .mount(&server)
        .await;
    let (_, work, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    // The same sentence list_accounts and the next call would give, rather
    // than a connection id the model cannot show anyone.
    let refused = c
        .refused(
            "gmail_search",
            json!({"account": "work", "query": "from:marta"}),
        )
        .await;
    assert!(refused.contains("`work`"), "{refused}");
    assert!(refused.contains("work@example.test"), "{refused}");
    assert!(
        refused.contains("https://gmcp.example/connections"),
        "{refused}"
    );
    assert!(
        !refused.contains(&format!("connection {}", work.id)),
        "{refused}"
    );
    let after = db.get_connection(work.id).await.unwrap();
    assert_eq!(after.status, ConnectionStatus::NeedsReauth);
}

#[tokio::test]
async fn a_refresh_token_sealed_under_another_secret_asks_for_a_reconnect() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let anna = user(&db, "anna", "anna@example.test").await;
    // Sealed before GMCP_SECRET was rotated, so nothing here can open it.
    let work = db
        .create_connection(NewConnection {
            user_id: anna.id,
            label: "work".into(),
            google_email: "work@example.test".into(),
            services: vec!["gmail".into()],
            granted_scopes: vec!["openid".into()],
            refresh_token_sealed: seal::seal(
                b"the secret this deployment used to have, long enough",
                "1//09exampleRefreshTokenForTests",
            ),
            delegate_ok: true,
        })
        .await
        .unwrap();
    let (_, secret) = token(&db, &["gmail:read"], Some(&anna), ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let refused = c
        .refused("gmail_list_labels", json!({"account": "work"}))
        .await;
    assert!(refused.contains("`work`"), "{refused}");
    assert!(
        refused.contains("https://gmcp.example/connections"),
        "{refused}"
    );
    // And the connection stops claiming to be healthy, with the one command
    // that says whether the secret is the problem.
    let after = db.get_connection(work.id).await.unwrap();
    assert_eq!(after.status, ConnectionStatus::NeedsReauth);
    let detail = after.status_detail.unwrap();
    assert!(detail.contains("GMCP_SECRET"), "{detail}");
    assert!(detail.contains("gmcp check-secret"), "{detail}");
}
