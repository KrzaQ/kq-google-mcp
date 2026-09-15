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
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use http_body_util::BodyExt;
use mail_parser::MimeHeaders;
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
use crate::google::text::truncation_notice;
use crate::http::{AppState, router};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef0123456789abcdef";
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/google/");
const HOST: &str = "gmcp.example";
/// What `GMCP_TIMEZONE` says, and so the zone every test user starts in.
const HOUSE: Tz = chrono_tz::Europe::Warsaw;

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
    app_in(db, server, uploads_dir()).await
}

/// A staging directory of this test's own. Nothing is written to disk until a
/// test actually stages a file, and the tests that do remove it again.
fn uploads_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "gmcp-test-uploads-{}",
        crate::domain::link::new_id()
    ))
}

/// The same, for a test that has to look at the staged files itself.
async fn app_in(db: &Db, server: Option<&MockServer>, upload_dir: std::path::PathBuf) -> Router {
    let config = Config {
        database: std::path::PathBuf::new(),
        upload_dir,
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: format!("https://{HOST}").parse().unwrap(),
        secret: SECRET.to_vec(),
        // Dev mode only decides that no identity provider is discovered; `/mcp`
        // has no cookie fallback either way.
        auth: AuthMode::Dev,
        google: google_config(server),
        auto_migrate: false,
        timezone: HOUSE,
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
    user_in(db, subject, email, HOUSE).await
}

/// The same, for somebody who keeps another clock. Two people on one server
/// see the same stored instant in two different zones, and the tests say so.
async fn user_in(db: &Db, subject: &str, email: &str, zone: Tz) -> User {
    let u = db
        .upsert_user(subject, Some(email), Some(subject), zone)
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
    token_for(db, scopes, owner, client, None).await
}

/// The same, with a connection allowlist: `None` is a token that reaches all
/// of its user's connections, `Some(ids)` one that reaches only those.
async fn token_for(
    db: &Db,
    scopes: &[&str],
    owner: Option<&User>,
    client: ClientProfile,
    allowlist: Option<&[i64]>,
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
            all_connections: allowlist.is_none(),
            created_by,
        })
        .await
        .unwrap();
    if let Some(ids) = allowlist {
        db.set_token_connections(row.id, ids).await.unwrap();
    }
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
        "render=\"formula\"",
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
            "gmail_list_send_as",
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

    // The careful Docs writes cannot be made without both locks, and each one
    // says that making it invalidates the read it was planned from.
    let required = |tool: &str| {
        by_name(tool)["inputSchema"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    for tool in [
        "docs_insert_text",
        "docs_edit_paragraph",
        "docs_style_paragraph",
        "docs_insert_code",
        "docs_insert_table",
    ] {
        for argument in ["revision_id", "confirmed"] {
            assert!(
                required(tool).contains(&json!(argument)),
                "{tool} does not require {argument}"
            );
        }
        assert!(
            by_name(tool)["description"]
                .as_str()
                .unwrap()
                .contains("revision id"),
            "{tool} does not say that a write makes the revision id stale"
        );
    }
    // `expect` is required where a write overwrites and optional where it
    // inserts, because inserting beside the wrong paragraph is recoverable.
    for tool in ["docs_edit_paragraph", "docs_style_paragraph"] {
        assert!(required(tool).contains(&json!("expect")), "{tool}");
    }
    for tool in ["docs_insert_text", "docs_insert_code", "docs_insert_table"] {
        assert!(!required(tool).contains(&json!("expect")), "{tool}");
        assert!(by_name(tool)["inputSchema"]["properties"]["expect"].is_object());
    }
    // The one that is careful and the one that is broad each point at the
    // other, so a model picking between them reads both.
    assert!(
        by_name("docs_edit_paragraph")["description"]
            .as_str()
            .unwrap()
            .contains("docs_replace_text")
    );
    assert!(
        by_name("docs_replace_text")["description"]
            .as_str()
            .unwrap()
            .contains("docs_edit_paragraph")
    );
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

/// A PNG wider than every profile's cap, so what each client gets back is the
/// downscale its cap asks for and not simply the bytes that went in. A smooth
/// gradient rather than noise: it has to survive re-encoding at 100 KB.
fn png() -> Vec<u8> {
    use image::{ImageFormat, Rgb, RgbImage};
    let img = RgbImage::from_fn(1600, 1200, |x, y| {
        Rgb([(x / 8) as u8, (y / 8) as u8, ((x + y) / 16) as u8])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

/// The size of a picture that came back, decoded from the block's base64.
fn image_size(data: &str) -> (u32, u32) {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("an image block is base64");
    let decoded = image::load_from_memory(&bytes).expect("an image block decodes");
    (decoded.width(), decoded.height())
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
    let mut claude_code: Option<(u32, u32)> = None;
    let mut generic: Option<(u32, u32)> = None;
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
        // Claude Code is capped at 1024 px and everything else at 1568, so the
        // 1600 px original comes back at two different sizes.
        let size = image_size(content[1]["data"].as_str().unwrap());
        match profile {
            ClientProfile::ClaudeCode => claude_code = Some(size),
            ClientProfile::Generic => generic = Some(size),
            _ => {}
        }
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
    assert_eq!(claude_code, Some((1024, 768)));
    assert_eq!(generic, Some((1568, 1176)));
    assert_ne!(
        claude_code, generic,
        "the profiles must not share one picture"
    );
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
    mount_send_as(&server).await;
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

// ----- attachments ------------------------------------------------------------

/// A client on a router whose staging directory this test can look into, and
/// which removes it again when the test ends.
struct Attaching {
    client: Client,
    app: Router,
    dir: std::path::PathBuf,
}

impl Drop for Attaching {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Attaching {
    async fn new(db: &Db, server: &MockServer, scopes: &[&str]) -> Self {
        let (_, _, secret) = one_of_everything(db, scopes, ClientProfile::Generic).await;
        Self::for_token(db, server, secret).await
    }

    async fn for_token(db: &Db, server: &MockServer, secret: String) -> Self {
        let dir = uploads_dir();
        let app = app_in(db, Some(server), dir.clone()).await;
        let mut client = Client::new(app.clone(), secret);
        client.initialize().await;
        Self { client, app, dir }
    }

    /// Mint a URL with the tool and POST the bytes to it, the way an agent
    /// does with curl. The upload id is what a draft tool takes.
    async fn upload(&mut self, filename: &str, bytes: &[u8]) -> Value {
        self.upload_with("gmail_upload_link", json!({"filename": filename}), bytes)
            .await
    }

    /// The same three steps, for whichever tool mints the ticket and whatever
    /// the file is called.
    async fn upload_with(&mut self, tool: &str, args: Value, bytes: &[u8]) -> Value {
        let minted = self.client.ok(tool, args).await;
        let url = minted["url"].as_str().expect("a URL").to_string();
        let path = url.strip_prefix("https://gmcp.example").expect(&url);
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::from(bytes.to_vec()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).expect("the upload answers JSON")
    }

    fn staged_files(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    /// GET one of this server's own URLs, the way the person with the link
    /// does — or, for a picture on its way into a document, the way Google
    /// does.
    async fn fetch(&self, url: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let path = url.strip_prefix("https://gmcp.example").expect(url);
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, body.to_vec(), headers)
    }
}

/// The three steps end to end: a tool mints a URL, the agent posts the bytes
/// itself, and the draft carries them.
#[tokio::test]
async fn a_draft_carries_the_files_that_were_uploaded_for_it() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/upload/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    // A draft with files never goes to the JSON endpoint: the message is too
    // large to carry as base64 inside it.
    server
        .register(
            Mock::given(path("/gmail/v1/users/me/drafts"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .named("a draft with files does not go as JSON"),
        )
        .await;
    let mut a = Attaching::new(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let first = a.upload("Faktura 04-2026.pdf", b"%PDF-1.7 invoice").await;
    assert_eq!(first["filename"], "Faktura 04-2026.pdf");
    assert_eq!(first["mime_type"], "application/pdf");
    assert_eq!(first["size"], 16);
    let second = a.upload("zażółć.csv", b"a,b\n1,2\n").await;
    assert_eq!(a.staged_files(), 2);

    let out = a
        .client
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Faktura",
                   "body": "W załączeniu faktura i zestawienie.",
                   "attachments": [first["upload_id"], second["upload_id"]]}),
        )
        .await;
    // The result says what the mail actually carries, by name.
    assert_eq!(
        out["attachments"],
        json!(["Faktura 04-2026.pdf", "zażółć.csv"])
    );
    // And the body promised files it has, so there is nothing to warn about.
    assert!(out["attachment_warning"].is_null(), "{out}");

    // The message is one multipart/mixed carrying both files beside the body.
    let request = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .rfind(|r| r.url.path() == "/upload/gmail/v1/users/me/drafts")
        .expect("the draft went to the upload endpoint");
    assert!(
        request
            .url
            .query()
            .unwrap_or_default()
            .contains("uploadType=multipart"),
        "{:?}",
        request.url.query()
    );
    let body = String::from_utf8_lossy(&request.body).to_string();
    assert!(body.contains("Content-Type: message/rfc822"), "{body}");
    assert!(body.contains("multipart/mixed"), "{body}");
    assert!(body.contains("W za"), "the body survives: {body}");
    assert!(body.contains("Faktura 04-2026.pdf"), "{body}");
    assert!(body.contains("Content-Type: text/csv"), "{body}");

    // The files were attached once and are gone: nothing is left on disk for
    // a second draft to pick up, and the ids no longer resolve.
    assert_eq!(a.staged_files(), 0);
    let again = a
        .client
        .refused(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "again",
                   "body": "again", "attachments": [first["upload_id"]]}),
        )
        .await;
    assert!(again.contains("gmail_upload_link"), "{again}");
    drop(server);
}

/// One person's staged file is not another's to attach, however they came by
/// the id.
#[tokio::test]
async fn an_upload_of_somebody_elses_is_refused_and_stays_theirs() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/upload/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    let mut anna = Attaching::new(&db, &server, &["gmail:read", "gmail:draft"]).await;
    let uploaded = anna.upload("prywatne.pdf", b"%PDF-1.7 mine").await;

    // A second person, with their own account and their own token, on the
    // same server and so the same staging store.
    let marta = user(&db, "marta", "marta@example.test").await;
    connect(&db, &marta, "marta-work", &["gmail"], false).await;
    let (_, secret) = token(
        &db,
        &["gmail:read", "gmail:draft"],
        Some(&marta),
        ClientProfile::Generic,
    )
    .await;
    let mut hers = Client::new(anna.app.clone(), secret);
    hers.initialize().await;

    let refused = hers
        .refused(
            "gmail_create_draft",
            json!({"account": "marta-work", "to": ["someone@example.test"],
                   "subject": "not mine", "body": "not mine",
                   "attachments": [uploaded["upload_id"]]}),
        )
        .await;
    assert!(refused.contains("there is no staged upload"), "{refused}");
    // And the file is still Anna's to attach.
    assert_eq!(anna.staged_files(), 1);
    let out = anna
        .client
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "mine",
                   "body": "mine", "attachments": [uploaded["upload_id"]]}),
        )
        .await;
    assert_eq!(out["attachments"], json!(["prywatne.pdf"]));
    drop(server);
}

/// The failure this whole thing is for: a body that promises files and a
/// draft that carries none.
#[tokio::test]
async fn a_draft_that_promises_a_file_and_carries_none_says_so() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    // Polish, which is what the mail that prompted this was written in.
    let polish = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Faktury",
                   "body": "Cześć, przesyłam w załączeniu trzy faktury. Pozdrawiam"}),
        )
        .await;
    let warning = polish["attachment_warning"].as_str().unwrap_or_default();
    assert!(warning.contains("carries no file"), "{polish}");
    assert!(warning.contains("gmail_upload_link"), "{polish}");
    assert_eq!(polish["attachments"], json!([]));

    // English, and from the HTML half of a body as well as the plain one.
    let english = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Report",
                   "body": "The report is ready.",
                   "html": "<p>Please see the <b>attached</b> report.</p>"}),
        )
        .await;
    assert!(!english["attachment_warning"].is_null(), "{english}");

    // An ordinary body says nothing about files, and gets no warning.
    let plain = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Jutro",
                   "body": "Cześć, spotkajmy się jutro o dziesiątej. Pozdrawiam"}),
        )
        .await;
    assert!(plain["attachment_warning"].is_null(), "{plain}");

    // Nothing here had a file, so every one of these went as JSON.
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .all(|r| !r.url.path().starts_with("/upload/")),
        "a draft with no files does not use the upload endpoint"
    );
    drop(server);
}

/// The draft whose raw read every attach test answers with.
const DRAFT: &str = "r-8812345678901234567";
const DRAFT_AT: &str = "/gmail/v1/users/me/drafts/r-8812345678901234567";
const DRAFT_UPLOAD_AT: &str = "/upload/gmail/v1/users/me/drafts/r-8812345678901234567";

/// The message inside an upload request, parsed back. A draft with files goes
/// to Gmail as a `multipart/related` whose second part is the mail itself, so
/// this is what Gmail is actually being asked to store.
fn uploaded_message(body: &[u8]) -> mail_parser::Message<'_> {
    const MARK: &[u8] = b"Content-Type: message/rfc822\r\n\r\n";
    const CLOSE: &[u8] = b"\r\n--gmcp";
    let start = body
        .windows(MARK.len())
        .position(|window| window == MARK)
        .expect("a message part")
        + MARK.len();
    let rest = &body[start..];
    let end = rest
        .windows(CLOSE.len())
        .rposition(|window| window == CLOSE)
        .expect("the closing boundary");
    mail_parser::MessageParser::default()
        .parse(&rest[..end])
        .expect("the draft Gmail was sent is a message")
}

/// Every address of one header, as `name <address>` or the bare address.
fn header_addresses(address: Option<&mail_parser::Address>) -> Vec<String> {
    address
        .map(|a| {
            a.iter()
                .map(|addr| match (addr.name(), addr.address()) {
                    (Some(name), Some(address)) => format!("{name} <{address}>"),
                    (_, address) => address.unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A draft with one file gains a second one, and everything else about it —
/// who it is to, what it says in both bodies, and which conversation it
/// belongs to — is still there afterwards. Rewriting it with
/// gmail_update_draft would mean restating all of that correctly.
#[tokio::test]
async fn a_file_is_added_to_a_draft_that_keeps_everything_else() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(&server, "GET", DRAFT_AT, fixture("gmail_draft_raw.json")).await;
    mount(&server, "PUT", DRAFT_UPLOAD_AT, fixture("gmail_draft.json")).await;
    // A message with files in it never goes as JSON.
    server
        .register(
            Mock::given(http_method("PUT"))
                .and(path(DRAFT_AT))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .named("a draft with files does not go as JSON"),
        )
        .await;
    let mut a = Attaching::new(&db, &server, &["gmail:read", "gmail:draft"]).await;
    let added = a.upload("zestawienie.csv", b"a,b\n1,2\n").await;

    let out = a
        .client
        .ok(
            "gmail_attach_to_draft",
            json!({"account": "work", "draft_id": DRAFT,
                   "attachments": [added["upload_id"]]}),
        )
        .await;
    // The whole list, not the one that was added: what the person is told the
    // draft carries has to be what it carries.
    assert_eq!(out["attachments"], json!(["raport.pdf", "zestawienie.csv"]));
    assert_eq!(out["from"], "\"Anna Kowalska\" <anna@example.test>");
    assert!(out["url"].as_str().unwrap().contains(DRAFT), "{out}");

    let request = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .rfind(|r| r.url.path() == DRAFT_UPLOAD_AT)
        .expect("the draft went to the upload endpoint");
    assert!(
        request
            .url
            .query()
            .unwrap_or_default()
            .contains("uploadType=multipart"),
        "{:?}",
        request.url.query()
    );
    // The conversation, which the metadata part names and the headers repeat.
    let body = String::from_utf8_lossy(&request.body).to_string();
    assert!(
        body.contains(r#"{"message":{"threadId":"18f0a1b2c3d4e5f0"}}"#),
        "{body}"
    );
    let message = uploaded_message(&request.body);
    assert_eq!(
        message.in_reply_to().as_text(),
        Some("CAF7n2sabc123@mail.example.test")
    );
    let references: Vec<&str> = message
        .references()
        .as_text_list()
        .unwrap_or_default()
        .iter()
        .map(|id| id.as_ref())
        .collect();
    assert_eq!(
        references,
        [
            "20260901T090000.0@example.test",
            "CAF7n2sabc123@mail.example.test"
        ]
    );
    // Everyone it was addressed to, including the blind copy.
    assert_eq!(
        header_addresses(message.to()),
        ["Marta Nowak <marta@example.test>"]
    );
    assert_eq!(header_addresses(message.cc()), ["team@example.test"]);
    assert_eq!(header_addresses(message.bcc()), ["archiwum@example.test"]);
    assert_eq!(
        header_addresses(message.from()),
        ["Anna Kowalska <anna@example.test>"]
    );
    assert_eq!(message.subject(), Some("Re: Q3 figures for Kraków"));
    // Both bodies, still saying what they said.
    assert!(
        message
            .body_text(0)
            .unwrap_or_default()
            .contains("w załączeniu raport."),
        "{:?}",
        message.body_text(0)
    );
    assert!(
        message
            .body_html(0)
            .unwrap_or_default()
            .contains("<b>raport</b>"),
        "{:?}",
        message.body_html(0)
    );
    // And both files, the old one byte for byte beside the new one.
    let files: Vec<(String, Vec<u8>)> = message
        .attachments()
        .map(|part| {
            (
                part.attachment_name().unwrap_or_default().to_string(),
                part.contents().to_vec(),
            )
        })
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert_eq!(files[0].0, "raport.pdf");
    assert_eq!(files[0].1, b"%PDF-1.7 raport kwartalny");
    assert_eq!(files[1].0, "zestawienie.csv");
    assert_eq!(files[1].1, b"a,b\n1,2\n");

    // The upload was spent, so nothing is left on the shelf to attach twice.
    assert_eq!(a.staged_files(), 0);
    drop(server);
}

/// A draft whose body names its own pictures cannot be rebuilt from its
/// parts, so nothing is written at all and the uploads stay where they are.
#[tokio::test]
async fn a_draft_with_a_picture_in_its_body_is_refused_rather_than_flattened() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    let at = "/gmail/v1/users/me/drafts/r-4400000000000000002";
    mount(&server, "GET", at, fixture("gmail_draft_raw_inline.json")).await;
    let mut a = Attaching::new(&db, &server, &["gmail:read", "gmail:draft"]).await;
    let added = a.upload("zestawienie.csv", b"a,b\n1,2\n").await;

    let refused = a
        .client
        .refused(
            "gmail_attach_to_draft",
            json!({"account": "work", "draft_id": "r-4400000000000000002",
                   "attachments": [added["upload_id"]]}),
        )
        .await;
    assert!(refused.contains("chart-q3@example.test"), "{refused}");
    assert!(refused.contains("gmail_update_draft"), "{refused}");
    // Nothing was written, and the file is still there to attach elsewhere.
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .all(|r| r.method.as_str() == "GET" || r.url.path() == "/token"),
        "the draft was written anyway"
    );
    assert_eq!(a.staged_files(), 1);
    drop(server);
}

/// A draft Gmail would refuse is refused here first, with the sizes, and the
/// uploads are left where they are: the person attaches them to something
/// else rather than uploading them again.
#[tokio::test]
async fn files_over_what_one_message_may_carry_are_refused_before_anything_is_written() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    // The draft already carries 20 MB, which is under the cap on its own.
    let carried = 20 * 1024 * 1024;
    Mock::given(http_method("GET"))
        .and(path(DRAFT_AT))
        .respond_with(ResponseTemplate::new(200).set_body_json(raw_draft_carrying(carried)))
        .mount(&server)
        .await;
    let mut a = Attaching::new(&db, &server, &["gmail:read", "gmail:draft"]).await;
    let added = a.upload("zdjęcia.zip", &vec![b'z'; 6 * 1024 * 1024]).await;

    let refused = a
        .client
        .refused(
            "gmail_attach_to_draft",
            json!({"account": "work", "draft_id": DRAFT,
                   "attachments": [added["upload_id"]]}),
        )
        .await;
    assert!(refused.contains("plan.pdf 20.0 MB"), "{refused}");
    assert!(refused.contains("zdjęcia.zip 6.0 MB"), "{refused}");
    assert!(refused.contains("26.0 MB"), "{refused}");
    assert!(refused.contains("25.0 MB"), "{refused}");
    // Nothing was written and the upload is still waiting, as the refusal says.
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .all(|r| r.method.as_str() == "GET" || r.url.path() == "/token"),
        "the draft was written anyway"
    );
    assert_eq!(a.staged_files(), 1);
    drop(server);
}

/// A draft carrying one file of `size` bytes, as `users.drafts.get` with
/// `format=raw` answers. Built here rather than recorded, because the size is
/// what the test is about and a fixture that large belongs in no repository.
fn raw_draft_carrying(size: usize) -> Value {
    use base64::Engine as _;
    let file = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; size]);
    let mime = [
        "From: anna@example.test",
        "To: marta@example.test",
        "Subject: Plan",
        "MIME-Version: 1.0",
        "Content-Type: multipart/mixed; boundary=\"mixed-1\"",
        "",
        "--mixed-1",
        "Content-Type: text/plain; charset=\"utf-8\"",
        "",
        "w załączeniu plan",
        "--mixed-1",
        "Content-Type: application/pdf; name=\"plan.pdf\"",
        "Content-Disposition: attachment; filename=\"plan.pdf\"",
        "Content-Transfer-Encoding: base64",
        "",
        &file,
        "--mixed-1--",
        "",
    ]
    .join("\r\n");
    json!({
        "id": DRAFT,
        "message": {
            "id": "18f0a1b2c3d4e5fb",
            "threadId": "18f0a1b2c3d4e5f0",
            "labelIds": ["DRAFT"],
            "raw": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime),
        }
    })
}

/// Minting a URL is drafting, not reading: a read-only token never sees it.
#[tokio::test]
async fn an_upload_link_needs_the_draft_scope() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    let (anna, _, read_only) =
        one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut reader = Client::new(app(&db, Some(&server)).await, read_only);
    reader.initialize().await;
    assert!(
        !reader
            .names()
            .await
            .contains(&"gmail_upload_link".to_string())
    );
    let refused = reader
        .refused("gmail_upload_link", json!({"filename": "x.pdf"}))
        .await;
    assert!(refused.contains("gmail:draft"), "{refused}");

    // The same person's other token, which may draft.
    let (_, secret) = token(
        &db,
        &["gmail:read", "gmail:draft"],
        Some(&anna),
        ClientProfile::Generic,
    )
    .await;
    let mut writer = Client::new(app(&db, Some(&server)).await, secret);
    writer.initialize().await;
    assert!(
        writer
            .names()
            .await
            .contains(&"gmail_upload_link".to_string())
    );
    let minted = writer
        .ok("gmail_upload_link", json!({"filename": "Faktura.pdf"}))
        .await;
    assert!(
        minted["url"]
            .as_str()
            .unwrap()
            .starts_with("https://gmcp.example/up/"),
        "{minted}"
    );
    assert_eq!(minted["filename"], "Faktura.pdf");
    // The note has to teach the whole dance, because the model reads it and
    // then has to do the middle step itself.
    let note = minted["note"].as_str().unwrap();
    assert!(note.contains("curl"), "{note}");
    assert!(note.contains("upload_id"), "{note}");
    assert!(note.contains("attachments"), "{note}");
    drop(server);
}

// ----- which address a draft is written as ------------------------------------

/// The account's send-as addresses: its own, one verified alias with a display
/// name, and one Google has not verified.
async fn mount_send_as(server: &MockServer) {
    mount(
        server,
        "GET",
        "/gmail/v1/users/me/settings/sendAs",
        fixture("gmail_send_as.json"),
    )
    .await;
}

/// The RFC 2822 message inside the last draft written to the mock server.
async fn draft_mime(server: &MockServer) -> String {
    use base64::Engine;
    let request = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .rfind(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/drafts"))
        .expect("a draft was written");
    let body: Value = serde_json::from_slice(&request.body).expect("the draft request is JSON");
    let raw = body["message"]["raw"].as_str().expect("a raw message");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .expect("the raw message is base64url");
    String::from_utf8(bytes).expect("the message is UTF-8")
}

#[tokio::test]
async fn gmail_list_send_as_says_which_addresses_may_be_written_as() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    let mut c = client(&db, &server, &["gmail:read"]).await;

    // A read token sees the tool, because choosing a From is reading a setting.
    assert!(c.names().await.contains(&"gmail_list_send_as".to_string()));

    let out = c.ok("gmail_list_send_as", json!({"account": "work"})).await;
    assert_eq!(out["count"], 3);
    let addresses = out["addresses"].as_array().unwrap();
    assert_eq!(addresses[0]["address"], "anna@example.test");
    assert_eq!(addresses[0]["is_default"], true);
    assert_eq!(addresses[0]["is_primary"], true);
    assert_eq!(addresses[0]["usable_as_from"], true);
    assert_eq!(addresses[1]["address"], "sales@example.test");
    assert_eq!(
        addresses[1]["from"],
        "\"Anna at Sales\" <sales@example.test>"
    );
    assert_eq!(addresses[1]["usable_as_from"], true);
    assert_eq!(addresses[1]["verification_status"], "accepted");
    // Listed, but Gmail would rewrite it, so it may not be written as.
    assert_eq!(addresses[2]["address"], "old@example.test");
    assert_eq!(addresses[2]["usable_as_from"], false);
    assert_eq!(addresses[2]["verification_status"], "pending");
    assert!(
        out["note"].as_str().unwrap().contains("anna@example.test"),
        "{out}"
    );
    drop(server);
}

#[tokio::test]
async fn a_draft_is_written_as_the_address_that_was_chosen_for_it() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    // A bare address gains the alias's own display name, so the draft reads
    // like one written by hand.
    let out = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Order 4471",
                   "body": "confirmed", "from": "sales@example.test"}),
        )
        .await;
    assert_eq!(out["from"], "\"Anna at Sales\" <sales@example.test>");
    assert!(
        out["from_reason"].as_str().unwrap().contains("`from`"),
        "{out}"
    );
    let mime = draft_mime(&server).await;
    assert!(
        mime.contains("From: \"Anna at Sales\" <sales@example.test>"),
        "{mime}"
    );

    // Nothing chosen is the account's default address, which is what Gmail
    // itself composes with.
    let out = c
        .ok(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "Order 4471",
                   "body": "confirmed"}),
        )
        .await;
    assert_eq!(out["from"], "\"Anna Kowalska\" <anna@example.test>");
    assert!(
        draft_mime(&server)
            .await
            .contains("From: \"Anna Kowalska\" <anna@example.test>")
    );
    drop(server);
}

#[tokio::test]
async fn an_address_gmail_would_rewrite_is_refused_rather_than_written() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    // Nothing may be written at all in this test: both calls are refused
    // before Gmail is asked to make a draft.
    server
        .register(
            Mock::given(http_method("POST"))
                .and(path("/gmail/v1/users/me/drafts"))
                .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_draft.json")))
                .expect(0)
                .named("a From Gmail would rewrite is never drafted"),
        )
        .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let unknown = c
        .refused(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "hello",
                   "body": "hello", "from": "someone@elsewhere.test"}),
        )
        .await;
    assert!(unknown.contains("someone@elsewhere.test"), "{unknown}");
    // The refusal names the addresses that would have worked, and not the one
    // Google has not verified.
    assert!(unknown.contains("anna@example.test"), "{unknown}");
    assert!(unknown.contains("sales@example.test"), "{unknown}");
    assert!(!unknown.contains("old@example.test"), "{unknown}");

    let unverified = c
        .refused(
            "gmail_create_draft",
            json!({"account": "work", "to": ["marta@example.test"], "subject": "hello",
                   "body": "hello", "from": "old@example.test"}),
        )
        .await;
    assert!(unverified.contains("old@example.test"), "{unverified}");
    assert!(unverified.contains("not verified"), "{unverified}");
    assert!(unverified.contains("pending"), "{unverified}");
    assert!(unverified.contains("sales@example.test"), "{unverified}");
    drop(server);
}

#[tokio::test]
async fn a_reply_comes_from_the_address_the_message_was_delivered_to() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f7",
        fixture("gmail_message_to_alias.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6",
        fixture("gmail_message_full.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    // Mail that arrived at the alias is answered from the alias, which is what
    // Gmail's own web UI does.
    let out = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f7",
                   "body": "Confirmed, thank you."}),
        )
        .await;
    assert_eq!(out["from"], "\"Anna at Sales\" <sales@example.test>");
    assert!(
        out["from_reason"].as_str().unwrap().contains("delivered"),
        "{out}"
    );
    let mime = draft_mime(&server).await;
    assert!(
        mime.contains("From: \"Anna at Sales\" <sales@example.test>"),
        "{mime}"
    );
    // The alias is the sender, so it is not also a recipient of its own reply.
    assert!(!mime.contains("To: \"Anna at Sales\""), "{mime}");

    // Mail that arrived at the account's own address is answered from it.
    let out = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6", "body": "thanks"}),
        )
        .await;
    assert_eq!(out["from"], "\"Anna Kowalska\" <anna@example.test>");
    assert!(
        draft_mime(&server)
            .await
            .contains("From: \"Anna Kowalska\" <anna@example.test>")
    );

    // And a named address still wins over the guess.
    let out = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6", "body": "thanks",
                   "from": "Sales desk <sales@example.test>"}),
        )
        .await;
    assert_eq!(out["from"], "Sales desk <sales@example.test>");
    drop(server);
}

#[tokio::test]
async fn a_reply_to_mail_the_account_sent_goes_to_the_people_it_was_sent_to() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f9",
        fixture("gmail_message_sent.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let out = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f9",
                   "body": "One more thing.", "reply_all": true}),
        )
        .await;
    // The account wrote the original, so the reply carries on the thread the
    // person started instead of answering themselves, and the result says so.
    assert!(
        out["to_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("sent"),
        "{out}"
    );

    let mime = draft_mime(&server).await;
    let to = mime
        .lines()
        .find(|l| l.starts_with("To:"))
        .unwrap_or_default();
    assert!(to.contains("marta@example.test"), "{mime}");
    assert!(to.contains("bob@example.test"), "{mime}");
    assert!(!to.contains("anna@example.test"), "{mime}");
    assert!(mime.contains("Cc: team@example.test"), "{mime}");
    assert!(
        mime.contains("From: \"Anna Kowalska\" <anna@example.test>"),
        "{mime}"
    );
    assert!(mime.contains("Subject: Re: Q3 figures"), "{mime}");
    assert!(
        mime.contains("In-Reply-To: <CAF7n2sghi789@mail.example.test>"),
        "{mime}"
    );
    drop(server);
}

#[tokio::test]
async fn an_updated_draft_stays_in_the_conversation_it_belongs_to() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount_send_as(&server).await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6",
        fixture("gmail_message_full.json"),
    )
    .await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/drafts/r-8812345678901234567",
        fixture("gmail_draft_reply.json"),
    )
    .await;
    mount(
        &server,
        "PUT",
        "/gmail/v1/users/me/drafts/r-8812345678901234567",
        fixture("gmail_draft.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/drafts/r-4400000000000000001",
        fixture("gmail_draft_plain.json"),
    )
    .await;
    mount(
        &server,
        "PUT",
        "/gmail/v1/users/me/drafts/r-4400000000000000001",
        fixture("gmail_draft.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let reply = c
        .ok(
            "gmail_reply_draft",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "body": "Thanks, I will read it tonight."}),
        )
        .await;
    assert_eq!(reply["draft_id"], "r-8812345678901234567");

    // One recipient corrected, and nothing else meant: the draft must still
    // answer the message it answered before.
    c.ok(
        "gmail_update_draft",
        json!({"account": "work", "draft_id": "r-8812345678901234567",
               "to": ["marta@example.test"], "subject": "Re: Q3 figures",
               "body": "Thanks, I will read it tonight."}),
    )
    .await;
    let (body, mime) = updated_draft(&server, "r-8812345678901234567").await;
    assert_eq!(body["message"]["threadId"], "18f0a1b2c3d4e5f0");
    assert!(
        mime.contains("In-Reply-To: <CAF7n2sabc123@mail.example.test>"),
        "{mime}"
    );
    assert!(mime.contains("References:"), "{mime}");
    assert!(mime.contains("<20260901T090000.0@example.test>"), "{mime}");
    assert!(mime.contains("<CAF7n2sabc123@mail.example.test>"), "{mime}");

    // A draft that was never a reply keeps its own thread and gains no
    // threading headers it never had.
    c.ok(
        "gmail_update_draft",
        json!({"account": "work", "draft_id": "r-4400000000000000001",
               "to": ["marta@example.test"], "subject": "Order 4471",
               "body": "confirmed, and one pallet more"}),
    )
    .await;
    let (body, mime) = updated_draft(&server, "r-4400000000000000001").await;
    assert_eq!(body["message"]["threadId"], "18f0a1b2c3d4e5fd");
    assert!(!mime.contains("In-Reply-To"), "{mime}");
    assert!(!mime.contains("References"), "{mime}");
    drop(server);
}

/// The last update sent to one draft: the request body, and the RFC 2822
/// message inside it decoded.
async fn updated_draft(server: &MockServer, draft_id: &str) -> (Value, String) {
    use base64::Engine;
    let request = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .rfind(|r| r.method.as_str() == "PUT" && r.url.path().ends_with(draft_id))
        .expect("the draft was updated");
    let body: Value = serde_json::from_slice(&request.body).expect("the update request is JSON");
    let raw = body["message"]["raw"].as_str().expect("a raw message");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .expect("the raw message is base64url");
    (
        body,
        String::from_utf8(bytes).expect("the message is UTF-8"),
    )
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
    // This one read the whole message, so a zero here is a counted zero and
    // means the message carries nothing. The same row in a listing leaves the
    // field out entirely, because there it would be a guess.
    assert_eq!(out["messages"][0]["attachments"], 0);

    // However they are spelled and whichever side they are named on: taking a
    // message out of the bin is not a thing this server does either.
    for (side, label) in [
        ("add", "TRASH"),
        ("add", "spam"),
        ("remove", "TRASH"),
        ("remove", "spam"),
        ("remove", " Trash "),
    ] {
        let refused = c
            .refused(
                "gmail_modify_labels",
                json!({"account": "work", "message_ids": ["18f0a1b2c3d4e5f6"],
                       side: [label]}),
            )
            .await;
        assert!(
            refused.contains("never moves mail to"),
            "{side} {label}: {refused}"
        );
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

/// An argument no tool knows is a refusal, not a silence.
///
/// serde ignores an unknown field by default, so a model that invented
/// `limit` for a search, or passed `size_pt` to a build that predated it,
/// was answered as though the argument had been honoured. Everything this
/// server does about wrong-but-quiet behaviour — the attachment warning, the
/// formula warning, `expect` — is undone if the arguments themselves are
/// read loosely.
#[tokio::test]
async fn an_argument_no_tool_knows_is_refused_by_name() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages",
        fixture("gmail_messages_list.json"),
    )
    .await;
    mount_summaries(&server).await;
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    // `limit` is a plausible invention: the argument is called `max`.
    let refused = c
        .refused(
            "gmail_search",
            json!({"account": "work", "query": "faktura", "limit": 5}),
        )
        .await;
    assert!(refused.contains("limit"), "it names the field: {refused}");
    assert!(refused.contains("max"), "and what it should be: {refused}");

    // Nothing was asked of Google on the way to that refusal: the arguments
    // are read before an account is even resolved.
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/token")
    );

    // The same argument spelled correctly still works.
    c.ok(
        "gmail_search",
        json!({"account": "work", "query": "faktura", "max": 5}),
    )
    .await;
}

/// A file attached inline is still a file. Gmail files anything carrying a
/// Content-ID among the inline parts, so a PDF dropped into a reply is not in
/// `attachments` at all — and until this was fixed, the two tools that fetch
/// a file refused the very id gmail_get_message had just reported. A
/// supplier's invoice could not be read through this server at all.
#[tokio::test]
async fn a_file_attached_inline_is_fetched_like_any_other() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&server)
        .await;
    let (_, _, secret) = one_of_everything(&db, &["gmail:read"], ClientProfile::ClaudeCode).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    // chart.png is an inline part: it has a Content-ID and is not in the
    // message's `attachments` at all.
    let out = c
        .ok(
            "gmail_attachment_link",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "ANGjdJ8chartPNG"}),
        )
        .await;
    assert_eq!(out["filename"], "chart.png");
    assert_eq!(out["mime_type"], "image/png");

    // The body names it by Content-ID, so that works as well.
    let by_content_id = c
        .ok(
            "gmail_attachment_link",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "chart-q3@example.test"}),
        )
        .await;
    assert_eq!(by_content_id["filename"], "chart.png");

    // And an id that really is not there says what the message does hold,
    // counting both kinds. "it has none" was the old answer, to a message
    // carrying two files.
    let refused = c
        .refused(
            "gmail_attachment_link",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "ANGjdJ8nothing"}),
        )
        .await;
    assert!(refused.contains("1 attachments"), "{refused}");
    assert!(refused.contains("1 inline parts"), "{refused}");
    assert!(refused.contains("chart.png"), "{refused}");
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

// ----- the plumbing the tool tests below share -------------------------------

/// One JSON endpoint.
async fn mount(server: &MockServer, verb: &str, at: &str, body: Value) {
    Mock::given(http_method(verb))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A mock server with the token endpoint and the two guards every Gmail test
/// keeps mounted.
async fn gmail_server() -> MockServer {
    let server = MockServer::start().await;
    mount_token(&server).await;
    for mock in expect_no_send() {
        server.register(mock).await;
    }
    server
}

/// One person with everything connected, and an initialised client for a
/// token with these scopes.
async fn client(db: &Db, server: &MockServer, scopes: &[&str]) -> Client {
    let (_, _, secret) = one_of_everything(db, scopes, ClientProfile::Generic).await;
    let mut c = Client::new(app(db, Some(server)).await, secret);
    c.initialize().await;
    c
}

// ----- what survives a call that only partly worked ---------------------------

#[tokio::test]
async fn a_label_change_reports_the_messages_it_could_not_change() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/labels",
        fixture("gmail_labels.json"),
    )
    .await;
    mount(
        &server,
        "POST",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/modify",
        fixture("gmail_message_modified.json"),
    )
    .await;
    Mock::given(http_method("POST"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f7/modify"))
        .respond_with(ResponseTemplate::new(404).set_body_json(fixture("error_not_found.json")))
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:modify"]).await;

    // One id is gone; the other is still labelled, and the answer says both.
    let out = c
        .ok(
            "gmail_modify_labels",
            json!({"account": "work",
                   "message_ids": ["18f0a1b2c3d4e5f6", "18f0a1b2c3d4e5f7"],
                   "add": ["Invoices"]}),
        )
        .await;
    assert_eq!(out["modified"], 1);
    assert_eq!(out["messages"][0]["message_id"], "18f0a1b2c3d4e5f6");
    assert_eq!(out["failed"][0]["message_id"], "18f0a1b2c3d4e5f7");
    assert!(
        out["failed"][0]["error"]
            .as_str()
            .unwrap()
            .contains("Requested entity was not found"),
        "{out}"
    );

    // A call where nothing at all worked is an error, as it always was.
    let refused = c
        .refused(
            "gmail_modify_labels",
            json!({"account": "work", "message_ids": ["18f0a1b2c3d4e5f7"],
                   "add": ["Invoices"]}),
        )
        .await;
    assert!(refused.contains("404"), "{refused}");
    drop(server);
}

#[tokio::test]
async fn a_spreadsheet_that_lost_a_tab_still_comes_back_with_its_id() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let made = "1NeWsPrEaDsHeEtIdExAmPlE0123456789abcd";
    mount(
        &server,
        "POST",
        "/upload/drive/v3/files",
        json!({
            "id": made,
            "name": "Support hours 2027",
            "mimeType": "application/vnd.google-apps.spreadsheet",
            "modifiedTime": "2026-09-09T08:00:00.000Z",
            "webViewLink": format!("https://docs.google.com/spreadsheets/d/{made}/edit"),
            "owners": [{"displayName": "Anna Kowalska", "emailAddress": "anna@example.test"}],
        }),
    )
    .await;
    // The first tab fails, the second is added: the spreadsheet exists either
    // way, and an error here would drop its id and invite a second one.
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"code": 500, "message": "Internal error encountered."}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_add_sheet.json")))
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;

    let out = c
        .ok(
            "sheets_create",
            json!({"account": "work", "title": "Support hours 2027",
                   "tabs": ["October", "November"],
                   "rows": [["Date", "Customer", "Hours"]], "confirmed": true}),
        )
        .await;
    assert_eq!(out["spreadsheet_id"], made);
    assert_eq!(
        out["url"],
        format!("https://docs.google.com/spreadsheets/d/{made}/edit")
    );
    let written = out["written"].as_str().unwrap();
    assert!(written.contains("created with 1 rows"), "{written}");
    assert!(
        written.contains("tab \"October\" could not be added"),
        "{written}"
    );
    assert!(written.contains("sheets_add_tab"), "{written}");
    // The one that worked is not reported as missing.
    assert!(!written.contains("November"), "{written}");
}

// ----- what is fetched, and what is counted ----------------------------------

#[tokio::test]
async fn a_picture_that_is_not_a_picture_is_refused_before_it_is_fetched() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6",
        fixture("gmail_message_full.json"),
    )
    .await;
    server
        .register(
            Mock::given(path_regex(r"/attachments/"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .named("a PDF is downloaded to be shown as a picture"),
        )
        .await;
    let mut c = client(&db, &server, &["gmail:read"]).await;

    let refused = c
        .refused(
            "gmail_view_image",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5f6",
                   "attachment_id": "ANGjdJ8pdfQ3"}),
        )
        .await;
    assert!(refused.contains("q3-figures.pdf"), "{refused}");
    assert!(refused.contains("not a picture"), "{refused}");
    assert!(refused.contains("gmail_attachment_text"), "{refused}");
    assert!(refused.contains("gmail_attachment_link"), "{refused}");
    drop(server);
}

#[test]
fn a_tool_s_own_cap_counts_content_and_not_the_first_cap_s_notice() {
    let content = "x".repeat(1000);
    // What the extractor hands over when it has already cut 500 characters.
    let extracted = format!("{content}{}", truncation_notice(500));

    let (text, cut) = crate::mcp::cap_text(extracted.clone(), 500, Some(100));
    // 900 of the thousand characters left, plus the 500 the extractor cut.
    assert_eq!(cut, 1400);
    assert_eq!(
        text,
        format!("{}{}", "x".repeat(100), truncation_notice(1400))
    );
    // One notice, not two, and the count is a count of characters that were
    // in the document rather than of the notice the extractor wrote.
    assert_eq!(text.matches("more characters were cut off here").count(), 1);

    // A cap that does not fire changes neither the text nor the count.
    let (same, cut) = crate::mcp::cap_text(extracted.clone(), 500, Some(5000));
    assert_eq!(same, extracted);
    assert_eq!(cut, 500);

    // And with nothing cut before, the count is the cap's own.
    let (text, cut) = crate::mcp::cap_text(content, 0, Some(100));
    assert_eq!(cut, 900);
    assert!(text.ends_with(&truncation_notice(900)));
}

// ----- one test per tool, over the same router -------------------------------
//
// Everything above proves the plumbing; what follows walks each remaining tool
// once, against the endpoints it actually calls, and checks the shape it hands
// back. The Gmail tests keep the send and trash guards mounted whatever they
// are about, because the rule they enforce is not about drafts.

/// The `format=metadata` read `gmail_search` and `gmail_list_drafts` make per
/// hit; one mock serves every id.
async fn mount_summaries(server: &MockServer) {
    Mock::given(http_method("GET"))
        .and(path_regex(r"^/gmail/v1/users/me/messages/[^/]+$"))
        .and(query_param("format", "metadata"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_metadata.json")),
        )
        .mount(server)
        .await;
}

/// The Google Doc that `drive_files_list.json` and `docs_document.json` both
/// describe, and its metadata as Drive answers it.
const DOC: &str = "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc";
const PDF: &str = "1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf";
const SHEET: &str = "1SpReAdShEeTiDeXaMpLe0123456789abcdefgh";

fn doc_file() -> Value {
    fixture("drive_files_list.json")["files"][0].clone()
}

// ----- gmail ------------------------------------------------------------------

#[tokio::test]
async fn gmail_search_asks_gmail_the_query_it_was_given_and_answers_in_rows() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages",
        fixture("gmail_messages_list.json"),
    )
    .await;
    mount_summaries(&server).await;
    let mut c = client(&db, &server, &["gmail:read"]).await;

    let out = c
        .ok(
            "gmail_search",
            json!({"account": "work", "query": "from:marta has:attachment",
                   "newer_than": "7d", "max": 5}),
        )
        .await;
    assert_eq!(out["account"], "work");
    assert_eq!(out["count"], 2);
    assert_eq!(out["messages"][0]["subject"], "Q3 figures for Kraków");
    assert_eq!(
        out["messages"][0]["from"],
        "Marta Nowak <marta@example.test>"
    );
    // A listing says nothing about files. Gmail answers `format=metadata`
    // without the part tree, so the old count here was zero against real mail
    // however many files a message carried; an absent field sends a model to
    // gmail_get_message instead of to a wrong conclusion.
    assert!(
        out["messages"][0].get("attachments").is_none(),
        "{}",
        out["messages"][0]
    );

    let asked = server.received_requests().await.unwrap();
    let list = asked
        .iter()
        .find(|r| r.url.path() == "/gmail/v1/users/me/messages")
        .expect("the list call");
    let query: std::collections::HashMap<_, _> = list.url.query_pairs().into_owned().collect();
    assert_eq!(query["q"], "from:marta has:attachment newer_than:7d");
    assert_eq!(query["maxResults"], "5");

    // An age that is not one word is refused before anything is asked.
    let bad = c
        .refused(
            "gmail_search",
            json!({"account": "work", "query": "x", "newer_than": "7 days"}),
        )
        .await;
    assert!(bad.contains("7d, 2m or 1y"), "{bad}");
    drop(server);
}

#[tokio::test]
async fn gmail_get_thread_returns_the_conversation_in_order_and_can_keep_the_end() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/threads/18f0a1b2c3d4e5f0",
        fixture("gmail_thread.json"),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read"]).await;

    let args = json!({"account": "work", "thread_id": "18f0a1b2c3d4e5f0"});
    let out = c.ok("gmail_get_thread", args.clone()).await;
    assert_eq!(out["thread_id"], "18f0a1b2c3d4e5f0");
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["message_id"], "18f0a1b2c3d4e5f6");
    assert_eq!(messages[1]["message_id"], "18f0a1b2c3d4e5fa");
    assert!(messages[0]["text"].as_str().unwrap().contains("quarterly"));

    // A long conversation is read for what was said last.
    let mut trimmed = args.as_object().unwrap().clone();
    trimmed.insert("max_messages".into(), json!(1));
    let last = c.ok("gmail_get_thread", Value::Object(trimmed)).await;
    let messages = last["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["message_id"], "18f0a1b2c3d4e5fa");
    drop(server);
}

/// The CSV attachment of `gmail_message_csv.json`, as the extractor reads it.
const CSV: &str = "date,customer,hours\n2026-09-01,Phoenix,1.5\n2026-09-03,Aurora,2\n";

#[tokio::test]
async fn gmail_attachment_text_extracts_the_attachment_and_says_what_it_cut() {
    use base64::Engine;
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5fc",
        fixture("gmail_message_csv.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5fc/attachments/ANGjdJ8csvSep",
        json!({
            "size": CSV.len(),
            "data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(CSV),
        }),
    )
    .await;
    let mut c = client(&db, &server, &["gmail:read"]).await;

    let args = json!({"account": "work", "message_id": "18f0a1b2c3d4e5fc",
                      "attachment_id": "ANGjdJ8csvSep"});
    let out = c.ok("gmail_attachment_text", args.clone()).await;
    assert_eq!(out["source"], "text");
    assert_eq!(out["filename"], "september-hours.csv");
    assert_eq!(out["truncated_chars"], 0);
    assert_eq!(out["text"], CSV);

    // `max_chars` cuts and says by how much, in the text itself.
    let mut short = args.as_object().unwrap().clone();
    short.insert("max_chars".into(), json!(10));
    let cut = c.ok("gmail_attachment_text", Value::Object(short)).await;
    assert_eq!(cut["truncated_chars"], CSV.chars().count() - 10);
    assert!(
        cut["text"]
            .as_str()
            .unwrap()
            .starts_with("date,custo\n\n[… "),
        "{}",
        cut["text"]
    );

    // An attachment id the message does not have is refused with the ones it
    // does, rather than fetched.
    let missing = c
        .refused(
            "gmail_attachment_text",
            json!({"account": "work", "message_id": "18f0a1b2c3d4e5fc",
                   "attachment_id": "nope"}),
        )
        .await;
    assert!(missing.contains("september-hours.csv"), "{missing}");
    drop(server);
}

#[tokio::test]
async fn gmail_list_drafts_names_every_draft_and_where_to_open_it() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_drafts_list.json"),
    )
    .await;
    mount_summaries(&server).await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let out = c.ok("gmail_list_drafts", json!({"account": "work"})).await;
    assert_eq!(out["count"], 2);
    assert_eq!(out["drafts"][0]["draft_id"], "r-8812345678901234567");
    assert_eq!(
        out["drafts"][0]["url"],
        "https://mail.google.com/mail/u/0/#drafts?compose=r-8812345678901234567"
    );
    assert_eq!(out["drafts"][1]["draft_id"], "r-4451234567890123456");
    drop(server);
}

#[tokio::test]
async fn gmail_update_draft_replaces_the_message_and_still_sends_nothing() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    Mock::given(http_method("PUT"))
        .and(path("/gmail/v1/users/me/drafts/r-8812345678901234567"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_draft.json")))
        .expect(1)
        .mount(&server)
        .await;
    // The update reads the draft first, to keep the conversation it is in.
    mount(
        &server,
        "GET",
        "/gmail/v1/users/me/drafts/r-8812345678901234567",
        fixture("gmail_draft_reply.json"),
    )
    .await;
    mount_send_as(&server).await;
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let out = c
        .ok(
            "gmail_update_draft",
            json!({"account": "work", "draft_id": "r-8812345678901234567",
                   "to": ["marta@example.test"], "subject": "Q3, again",
                   "body": "the corrected figures"}),
        )
        .await;
    assert_eq!(out["draft_id"], "r-8812345678901234567");
    assert_eq!(out["thread_id"], "18f0a1b2c3d4e5f0");
    assert!(out["note"].as_str().unwrap().contains("nothing was sent"));

    // A draft with nobody to send it to is refused here, not by Gmail.
    let empty = c
        .refused(
            "gmail_update_draft",
            json!({"account": "work", "draft_id": "r-8812345678901234567",
                   "to": [" "], "subject": "x", "body": "y"}),
        )
        .await;
    assert!(empty.contains("at least one recipient"), "{empty}");
    drop(server);
}

#[tokio::test]
async fn gmail_delete_draft_deletes_the_draft_and_touches_nothing_else() {
    let db = Db::open_memory().await.unwrap();
    let server = gmail_server().await;
    Mock::given(http_method("DELETE"))
        .and(path("/gmail/v1/users/me/drafts/r-8812345678901234567"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    // The undo for a draft is a delete of that draft and nothing more: no
    // read, no modify, no second write of any kind.
    for verb in ["GET", "POST", "PUT"] {
        server
            .register(
                Mock::given(http_method(verb))
                    .and(path_regex(r"^/gmail/"))
                    .respond_with(ResponseTemplate::new(200))
                    .expect(0)
                    .named("deleting a draft reads or writes something else"),
            )
            .await;
    }
    let mut c = client(&db, &server, &["gmail:read", "gmail:draft"]).await;

    let out = c
        .ok(
            "gmail_delete_draft",
            json!({"account": "work", "draft_id": "r-8812345678901234567"}),
        )
        .await;
    assert_eq!(out["draft_id"], "r-8812345678901234567");
    assert_eq!(out["note"], "the draft is gone");
    drop(server);
}

#[tokio::test]
async fn a_token_with_an_allowlist_cannot_name_a_connection_off_it() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    let work = connect(&db, &anna, "work", &["gmail"], true).await;
    connect(&db, &anna, "personal", &["gmail"], true).await;
    let (_, secret) = token_for(
        &db,
        &["gmail:read"],
        Some(&anna),
        ClientProfile::Generic,
        Some(&[work.id]),
    )
    .await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;

    let out = c.ok("list_accounts", json!({})).await;
    let labels: Vec<&str> = out["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["work"]);

    // The connection exists and belongs to the same person; this token still
    // cannot name it, and is not told that it exists.
    let refused = c
        .refused("gmail_list_labels", json!({"account": "personal"}))
        .await;
    assert_eq!(
        refused,
        "there is no account called `personal`; this token can reach `work`"
    );
}

// ----- drive ------------------------------------------------------------------

/// What Drive's markdown export of `docs_document.json` would be.
const MARKDOWN: &str = "# Q3 report\n\nRevenue held up in September.\n";

async fn mount_markdown_export(server: &MockServer) {
    Mock::given(http_method("GET"))
        .and(path(format!("/drive/v3/files/{DOC}/export")))
        .and(query_param("mimeType", "text/markdown"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(MARKDOWN.as_bytes().to_vec(), "text/markdown"),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn drive_search_answers_the_files_it_found_and_never_the_bin() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        "/drive/v3/files",
        fixture("drive_files_list.json"),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok(
            "drive_search",
            json!({"account": "work", "name_contains": "q3",
                   "modified_after": "2026-09-01T00:00:00Z", "max": 5}),
        )
        .await;
    assert_eq!(out["count"], 2);
    assert_eq!(out["files"][0]["name"], "Q3 report");
    assert_eq!(out["files"][1]["size"], 26112);
    assert_eq!(out["files"][1]["owners"][0], "marta@example.test");

    let asked = server.received_requests().await.unwrap();
    let list = asked
        .iter()
        .find(|r| r.url.path() == "/drive/v3/files")
        .expect("the files.list call");
    let query: std::collections::HashMap<_, _> = list.url.query_pairs().into_owned().collect();
    assert!(
        query["q"].starts_with("trashed = false"),
        "{:?}",
        query["q"]
    );
    assert_eq!(query["pageSize"], "5");

    // Something that is no kind of time is refused, and the refusal says which
    // shapes work and on whose clock they are read.
    let bad = c
        .refused(
            "drive_search",
            json!({"account": "work", "modified_after": "last tuesday"}),
        )
        .await;
    assert!(bad.contains("last tuesday"), "{bad}");
    assert!(bad.contains("Europe/Warsaw"), "{bad}");
}

#[tokio::test]
async fn drive_get_file_answers_the_metadata_and_nothing_more() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{PDF}"),
        fixture("drive_file.json"),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok("drive_get_file", json!({"account": "work", "file_id": PDF}))
        .await;
    assert_eq!(out["file_id"], PDF);
    assert_eq!(out["name"], "q3-figures.pdf");
    assert_eq!(out["mime_type"], "application/pdf");
    assert_eq!(out["size"], 26112);
    // Stored as UTC, shown on the person's clock: September in Warsaw is +02:00.
    assert_eq!(out["modified_time"], "2026-09-03T11:12:00+02:00");
}

#[tokio::test]
async fn drive_download_link_mints_a_link_for_bytes_and_refuses_a_google_file() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{PDF}"),
        fixture("drive_file.json"),
    )
    .await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{DOC}"),
        doc_file(),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok(
            "drive_download_link",
            json!({"account": "work", "file_id": PDF}),
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
    assert_eq!(out["size"], 26112);
    assert!(out["note"].as_str().unwrap().contains("15 minutes"));

    // A Google Doc has no bytes of its own, and the answer says which tool has.
    let refused = c
        .refused(
            "drive_download_link",
            json!({"account": "work", "file_id": DOC}),
        )
        .await;
    assert!(refused.contains("drive_export_link"), "{refused}");
}

#[tokio::test]
async fn drive_export_link_refuses_a_format_the_file_cannot_produce() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{DOC}"),
        doc_file(),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok(
            "drive_export_link",
            json!({"account": "work", "file_id": DOC, "format": "markdown"}),
        )
        .await;
    assert_eq!(out["filename"], "Q3 report.md");
    assert_eq!(out["mime_type"], "text/markdown");
    // An export has no size until it is made.
    assert!(out["size"].is_null(), "{out}");

    // A Doc does not export as a spreadsheet, and the refusal says what it does
    // export as rather than leaving Google to answer that.
    let refused = c
        .refused(
            "drive_export_link",
            json!({"account": "work", "file_id": DOC, "format": "xlsx"}),
        )
        .await;
    assert!(refused.contains("cannot be exported as xlsx"), "{refused}");
    assert!(refused.contains("markdown, pdf, docx"), "{refused}");

    // And a format that is not a format at all is refused before any call.
    let unknown = c
        .refused(
            "drive_export_link",
            json!({"account": "work", "file_id": DOC, "format": "epub"}),
        )
        .await;
    assert!(unknown.contains("unknown export format"), "{unknown}");
}

#[tokio::test]
async fn drive_read_text_reads_a_google_doc_as_markdown() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{DOC}"),
        doc_file(),
    )
    .await;
    mount_markdown_export(&server).await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok(
            "drive_read_text",
            json!({"account": "work", "file_id": DOC}),
        )
        .await;
    assert_eq!(out["source"], "google-doc");
    assert_eq!(out["filename"], "Q3 report");
    assert_eq!(out["text"], MARKDOWN);
    assert_eq!(out["chars"], MARKDOWN.chars().count());
    assert_eq!(out["truncated_chars"], 0);
}

#[tokio::test]
async fn drive_view_image_downscales_the_file_it_was_pointed_at() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let picture = "1PiCtUrEiDeXaMpLe0123456789abcdefghijk";
    Mock::given(http_method("GET"))
        .and(path(format!("/drive/v3/files/{picture}")))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(png(), "image/png"))
        .mount(&server)
        .await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{picture}"),
        json!({
            "id": picture,
            "name": "chart.png",
            "mimeType": "image/png",
            "modifiedTime": "2026-09-05T10:00:00.000Z",
            "size": "24096",
            "webViewLink": format!("https://drive.google.com/file/d/{picture}/view"),
            "owners": [{"displayName": "Anna Kowalska", "emailAddress": "anna@example.test"}],
        }),
    )
    .await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{PDF}"),
        fixture("drive_file.json"),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let v = c
        .call(
            "drive_view_image",
            json!({"account": "work", "file_id": picture}),
        )
        .await;
    assert!(!is_error(&v), "{}", error_text(&v));
    let result = &v["result"];
    assert!(result["structuredContent"].is_null(), "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].as_str().unwrap().contains("chart.png"));
    assert_eq!(content[1]["type"], "image");
    assert_eq!(
        image_size(content[1]["data"].as_str().unwrap()),
        (1568, 1176)
    );

    // Anything that is not a picture is refused rather than downloaded.
    let refused = c
        .refused(
            "drive_view_image",
            json!({"account": "work", "file_id": PDF}),
        )
        .await;
    assert!(refused.contains("not a picture"), "{refused}");
    assert!(refused.contains("drive_read_text"), "{refused}");
}

#[tokio::test]
async fn drive_list_comments_reads_the_margin_on_the_persons_clock() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{DOC}/comments"),
        fixture("drive_comments.json"),
    )
    .await;
    let mut c = client(&db, &server, &["drive:read"]).await;

    let out = c
        .ok(
            "drive_list_comments",
            json!({"account": "work", "file_id": DOC}),
        )
        .await;
    // The resolved thread is left out, and the answer says so rather than
    // letting a model believe the margin holds two comments.
    assert_eq!(out["count"], 2);
    assert_eq!(out["file_id"], DOC);
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("1 resolved thread is not shown"), "{note}");
    assert!(note.contains("include_resolved=true"), "{note}");

    let first = &out["comments"][0];
    assert_eq!(first["author"], "marta@example.test");
    assert_eq!(
        first["text"],
        "Is this the number before or after the refund?"
    );
    assert_eq!(first["quoted_text"], "Revenue held up in September.");
    assert_eq!(first["resolved"], false);
    // Stored as UTC, read on the person's clock: September in Warsaw is +02:00.
    assert_eq!(first["created_time"], "2026-09-03T11:12:00+02:00");
    assert_eq!(first["modified_time"], "2026-09-03T12:05:00+02:00");

    // The replies are the thread as it reads, oldest first.
    let replies = first["replies"].as_array().unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["text"], "After.");
    assert_eq!(replies[0]["author"], "anna@example.test");
    assert_eq!(replies[0]["created_time"], "2026-09-03T11:40:00+02:00");
    assert_eq!(replies[1]["text"], "Then say so in the sentence.");
    assert_eq!(replies[1]["created_time"], "2026-09-03T12:05:00+02:00");

    // A comment on the whole file is anchored to nothing at all.
    assert!(out["comments"][1]["quoted_text"].is_null(), "{out}");
    assert_eq!(out["comments"][1]["author"], "Redakcja");

    // Asked for, the resolved thread comes back with the rest and nothing is
    // left to say.
    let all = c
        .ok(
            "drive_list_comments",
            json!({"account": "work", "file_id": DOC, "include_resolved": true}),
        )
        .await;
    assert_eq!(all["count"], 3);
    assert_eq!(all["comments"][1]["resolved"], true);
    assert_eq!(all["comments"][1]["text"], "Title case here, please.");
    assert!(all["note"].is_null(), "{all}");

    // `max` cuts the list and says how much it cut.
    let one = c
        .ok(
            "drive_list_comments",
            json!({"account": "work", "file_id": DOC, "max": 1}),
        )
        .await;
    assert_eq!(one["count"], 1);
    let note = one["note"].as_str().unwrap();
    assert!(note.contains("1 more thread was left out"), "{note}");
}

#[tokio::test]
async fn drive_list_comments_is_refused_before_google_without_the_drive_service() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let anna = user(&db, "anna", "anna@example.test").await;
    connect(&db, &anna, "work", &["gmail"], true).await;
    let (_, secret) = token(&db, &["drive:read"], Some(&anna), ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, Some(&server)).await, secret);
    c.initialize().await;

    let refused = c
        .refused(
            "drive_list_comments",
            json!({"account": "work", "file_id": DOC}),
        )
        .await;
    assert_eq!(
        refused,
        "connection `work` has no `drive` service; it was connected with `gmail`. \
         The person can add it by reconnecting the account at https://gmcp.example/connections"
    );
    // Refused here and not by Google: nothing was sent at all, not even the
    // refresh every call starts with.
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ----- docs -------------------------------------------------------------------

#[tokio::test]
async fn docs_read_answers_the_document_with_its_tabs_and_its_markdown() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/v1/documents/{DOC}"),
        fixture("docs_document.json"),
    )
    .await;
    mount_markdown_export(&server).await;
    let mut c = client(&db, &server, &["docs:read", "drive:read"]).await;

    let out = c
        .ok("docs_read", json!({"account": "work", "doc_id": DOC}))
        .await;
    assert_eq!(out["doc_id"], DOC);
    assert_eq!(out["title"], "Q3 report");
    assert_eq!(
        out["url"],
        format!("https://docs.google.com/document/d/{DOC}/edit")
    );
    // Child tabs are flattened, because a model asking for one wants the list.
    assert_eq!(out["tabs"][0]["title"], "Summary");
    assert_eq!(out["tabs"][1]["title"], "Appendix");
    assert_eq!(out["text"], MARKDOWN);
}

const PICTURE_DOC: &str = "1PiCtUrEsDoCiDeXaMpLe0123456789abcdefgh";

/// A document of pictures whose first picture this same mock serves, so the
/// contentUri the tools follow is one a test can answer.
async fn mount_document_pictures(server: &MockServer) {
    let mut document = fixture("docs_document_images.json");
    document["inlineObjects"]["kix.chart"]["inlineObjectProperties"]["embeddedObject"]["imageProperties"]
        ["contentUri"] = json!(format!("{}/docs-image/chart", server.uri()));
    mount(
        server,
        "GET",
        &format!("/v1/documents/{PICTURE_DOC}"),
        document,
    )
    .await;
    Mock::given(http_method("GET"))
        .and(path("/docs-image/chart"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(png())
                .insert_header("content-type", "image/png"),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_document_says_which_pictures_it_holds_and_what_to_call_them() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount_document_pictures(&server).await;
    let mut c = client(&db, &server, &["docs:read"]).await;

    let out = c
        .ok(
            "docs_list_images",
            json!({"account": "work", "doc_id": PICTURE_DOC}),
        )
        .await;
    assert_eq!(out["doc_id"], PICTURE_DOC);
    assert_eq!(out["title"], "Five screenshots");
    assert_eq!(out["count"], 3);
    // In the order the document holds them, which is not the order the
    // inlineObjects map carries them in.
    let images = out["images"].as_array().unwrap();
    assert_eq!(
        images
            .iter()
            .map(|i| (
                i["image"].as_str().unwrap(),
                i["object_id"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            ("image1", "kix.chart"),
            ("image2", "kix.screenshot"),
            ("image3", "kix.drawing"),
        ]
    );
    assert_eq!(images[0]["alt_title"], "Revenue");
    assert_eq!(images[0]["alt_text"], "Revenue by quarter, in thousands");
    assert_eq!(images[0]["width_pt"], 320);
    assert_eq!(images[0]["height_pt"], 180);
    assert_eq!(images[0]["fetchable"], true);
    assert!(images[1]["alt_title"].is_null());
    // A drawing has no picture of its own, and the row says so before a model
    // spends a call finding out.
    assert_eq!(images[2]["fetchable"], false);
    assert!(
        out["note"].as_str().unwrap().contains("docs_view_image"),
        "{}",
        out["note"]
    );

    // A label nobody gave out and an object id nobody has are both refused
    // with what the document does hold.
    for wanted in ["image9", "kix.nothing"] {
        for tool in ["docs_view_image", "docs_image_link"] {
            let refused = c
                .refused(
                    tool,
                    json!({"account": "work", "doc_id": PICTURE_DOC, "image": wanted}),
                )
                .await;
            assert!(
                refused.contains("image1, image2, image3"),
                "{tool}: {refused}"
            );
            assert!(refused.contains(wanted), "{tool}: {refused}");
        }
    }

    // And a drawing says why it cannot be fetched rather than failing to
    // decode something it never had.
    let refused = c
        .refused(
            "docs_view_image",
            json!({"account": "work", "doc_id": PICTURE_DOC, "image": "image3"}),
        )
        .await;
    assert!(refused.contains("drawing"), "{refused}");
}

#[tokio::test]
async fn a_picture_in_a_document_is_shaped_for_the_client_and_linked_at_full_size() {
    for (profile, blocks) in [
        (ClientProfile::ClaudeCode, 2),
        (ClientProfile::OpenWebUi, 3),
    ] {
        let db = Db::open_memory().await.unwrap();
        let server = google_server().await;
        mount_document_pictures(&server).await;
        let (_, work, secret) = one_of_everything(&db, &["docs:read"], profile).await;
        let mut c = Client::new(app(&db, Some(&server)).await, secret);
        c.initialize().await;

        let v = c
            .call(
                "docs_view_image",
                json!({"account": "work", "doc_id": PICTURE_DOC, "image": "image1"}),
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
                .contains("Five screenshots image1.png"),
            "{}",
            content[0]["text"]
        );
        assert_eq!(content[1]["type"], "image");
        // The original is 1600 px wide and never leaves as it is.
        let (width, _) = image_size(content[1]["data"].as_str().unwrap());
        assert_eq!(
            width,
            if profile == ClientProfile::ClaudeCode {
                1024
            } else {
                1568
            }
        );
        if profile == ClientProfile::OpenWebUi {
            assert_eq!(content[2]["type"], "resource");
            assert_eq!(
                content[2]["resource"]["uri"],
                format!("gmcp://{}/docs/{PICTURE_DOC}/kix.chart", work.id)
            );
        }

        // The link is the way to the picture as it is, and the bytes never
        // come back through MCP.
        let out = c
            .ok(
                "docs_image_link",
                json!({"account": "work", "doc_id": PICTURE_DOC, "image": "image1"}),
            )
            .await;
        assert!(
            out["url"]
                .as_str()
                .unwrap()
                .starts_with("https://gmcp.example/dl/"),
            "{out}"
        );
        assert_eq!(out["filename"], "Five screenshots image1.png");
        assert_eq!(out["mime_type"], "image/png");
        assert_eq!(out["size"], png().len());
        // The object id is what the link keeps, so it still names this picture
        // after somebody adds one above it.
        let link = db.list_audit(Default::default()).await.unwrap();
        assert_eq!(link[0].kind, crate::db::AuditKind::LinkCreated);
    }
}

#[tokio::test]
async fn docs_create_previews_the_document_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    // The one call that would make a document, which must not be made.
    server
        .register(
            Mock::given(path("/upload/drive/v3/files"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(fixture("drive_file_created.json")),
                )
                .expect(0)
                .named("an unconfirmed docs_create reaches Drive"),
        )
        .await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let out = c
        .ok(
            "docs_create",
            json!({"account": "work", "title": "Meeting notes",
                   "markdown": "# Meeting notes\n\n- one\n- two\n", "confirmed": false}),
        )
        .await;
    assert_eq!(out["confirmed"], false);
    assert_eq!(out["written"], false);
    assert!(out["action"].as_str().unwrap().contains("Meeting notes"));
    assert!(
        out["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("# Meeting notes")),
        "{out}"
    );
    assert!(out["next"].as_str().unwrap().contains("confirmed=true"));

    // A document with no title is refused before the question is even asked.
    let bad = c
        .refused(
            "docs_create",
            json!({"account": "work", "title": "  ", "markdown": "x", "confirmed": true}),
        )
        .await;
    assert!(bad.contains("needs a title"), "{bad}");
    drop(server);
}

#[tokio::test]
async fn docs_create_makes_the_document_once_it_is_confirmed() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("POST"))
        .and(path("/upload/drive/v3/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("drive_file_created.json")))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let out = c
        .ok(
            "docs_create",
            json!({"account": "work", "title": "Meeting notes",
                   "markdown": "# Meeting notes\n", "confirmed": true}),
        )
        .await;
    assert_eq!(out["doc_id"], "1NeWlYcReAtEdDoCiDeXaMpLe0123456789abcd");
    assert_eq!(out["title"], "Meeting notes");
    assert!(
        out["url"]
            .as_str()
            .unwrap()
            .contains("1NeWlYcReAtEdDoCiDeXaMpLe0123456789abcd"),
        "{out}"
    );
    assert!(out["written"].as_str().unwrap().contains("markdown"));
    drop(server);
}

#[tokio::test]
async fn docs_append_previews_and_then_writes_at_the_end_of_the_document() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/v1/documents/{DOC}"),
        fixture("docs_document.json"),
    )
    .await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v1/documents/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_batch_update.json")))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": DOC, "text": "One more line.\n"});
    let mut preview = args.as_object().unwrap().clone();
    preview.insert("confirmed".into(), json!(false));
    let shown = c.ok("docs_append", Value::Object(preview)).await;
    assert_eq!(shown["written"], false);
    assert!(shown["action"].as_str().unwrap().contains(DOC));
    assert!(
        shown["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("One more line.")),
        "{shown}"
    );

    let mut confirmed = args.as_object().unwrap().clone();
    confirmed.insert("confirmed".into(), json!(true));
    let out = c.ok("docs_append", Value::Object(confirmed)).await;
    assert_eq!(out["doc_id"], DOC);
    // The document ends at 32, so the text goes in at 31.
    assert_eq!(out["written"], "15 characters appended at index 31");
    drop(server);
}

#[tokio::test]
async fn docs_replace_text_reports_how_many_occurrences_it_changed() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v1/documents/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_replace_reply.json")))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": DOC,
                      "find": "September", "replace": "October"});
    let mut preview = args.as_object().unwrap().clone();
    preview.insert("confirmed".into(), json!(false));
    let shown = c.ok("docs_replace_text", Value::Object(preview)).await;
    assert_eq!(shown["written"], false);
    assert!(
        shown["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("cannot be undone")),
        "{shown}"
    );

    let mut confirmed = args.as_object().unwrap().clone();
    confirmed.insert("confirmed".into(), json!(true));
    let out = c.ok("docs_replace_text", Value::Object(confirmed)).await;
    assert_eq!(out["replacements"], 3);
    assert_eq!(out["written"], "3 occurrences replaced");

    // An empty needle would match everywhere and is refused.
    let bad = c
        .refused(
            "docs_replace_text",
            json!({"account": "work", "doc_id": DOC, "find": "",
                   "replace": "x", "confirmed": true}),
        )
        .await;
    assert!(bad.contains("match everywhere"), "{bad}");
    drop(server);
}

// ----- docs, paragraph by paragraph -------------------------------------------

const ARTICLE: &str = "1ArTiClEdOcIdExAmPlE0123456789abcdef";
/// What `docs_article.json` says the document is at.
const ARTICLE_REVISION: &str = "ALm37BW0Article1";

/// The Polish article every test below reads before it writes.
async fn article_server() -> MockServer {
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/v1/documents/{ARTICLE}"),
        fixture("docs_article.json"),
    )
    .await;
    server
}

/// The write endpoint, counted. `times` is verified when the server is
/// dropped, so a preview that writes is a test that fails.
async fn expect_batches(server: &MockServer, times: u64) {
    Mock::given(http_method("POST"))
        .and(path(format!("/v1/documents/{ARTICLE}:batchUpdate")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_batch_update.json")))
        .expect(times)
        .named("one batch per write and none for a preview")
        .mount(server)
        .await;
}

/// The same arguments, answered or approved.
fn confirming(args: &Value, yes: bool) -> Value {
    let mut map = args.as_object().unwrap().clone();
    map.insert("confirmed".into(), json!(yes));
    Value::Object(map)
}

fn details(shown: &Value) -> String {
    shown["details"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn docs_list_paragraphs_numbers_the_body_and_answers_the_revision() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    let mut c = client(&db, &server, &["docs:read"]).await;

    let out = c
        .ok(
            "docs_list_paragraphs",
            json!({"account": "work", "doc_id": ARTICLE}),
        )
        .await;
    assert_eq!(out["revision_id"], ARTICLE_REVISION);
    assert_eq!(out["title"], "Wywiad z Anną");
    assert_eq!(out["count"], 5);
    assert_eq!(
        out["url"],
        format!("https://docs.google.com/document/d/{ARTICLE}/edit")
    );
    let rows = out["paragraphs"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0]["style"], "TITLE");
    assert_eq!(rows[1]["paragraph"], 2);
    assert_eq!(rows[1]["style"], "HEADING_2");
    assert_eq!(rows[1]["text"], "Część pierwsza");
    // Characters, not bytes and not the UTF-16 units Docs counts in.
    assert_eq!(rows[2]["chars"], 29);
    assert_eq!(rows[2]["text"], "Zażółć gęślą jaźń 😀 już i już");
    // The paragraph in the table cell is numbered where the body meets it.
    assert_eq!(rows[3]["paragraph"], 4);
    assert_eq!(rows[3]["in_table"], true);
    assert_eq!(rows[4]["text"], "Koniec.");
    assert!(
        out["note"].as_str().unwrap().contains("revision_id"),
        "{out}"
    );

    // A range answers that range and still says how long the document is.
    let one = c
        .ok(
            "docs_list_paragraphs",
            json!({"account": "work", "doc_id": ARTICLE, "from": 3, "to": 3}),
        )
        .await;
    assert_eq!(one["count"], 5);
    assert_eq!(one["from"], 3);
    assert_eq!(one["to"], 3);
    let only = one["paragraphs"].as_array().unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0]["paragraph"], 3);

    // Past the end is not an error: it says what the document holds.
    let past = c
        .ok(
            "docs_list_paragraphs",
            json!({"account": "work", "doc_id": ARTICLE, "from": 9}),
        )
        .await;
    assert!(past["paragraphs"].as_array().unwrap().is_empty());
    assert!(
        past["note"].as_str().unwrap().contains("5 paragraphs"),
        "{past}"
    );
}

#[tokio::test]
async fn a_long_paragraph_is_cut_until_it_is_asked_for_in_full() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let long = "Dłuższy akapit. ".repeat(60);
    let chars = long.chars().count();
    let mut document = fixture("docs_article.json");
    document["body"]["content"][5]["endIndex"] = json!(79 + chars + 1);
    document["body"]["content"][5]["paragraph"]["elements"][0]["endIndex"] = json!(79 + chars + 1);
    document["body"]["content"][5]["paragraph"]["elements"][0]["textRun"]["content"] =
        json!(format!("{long}\n"));
    mount(
        &server,
        "GET",
        &format!("/v1/documents/{ARTICLE}"),
        document,
    )
    .await;
    let mut c = client(&db, &server, &["docs:read"]).await;

    let out = c
        .ok(
            "docs_list_paragraphs",
            json!({"account": "work", "doc_id": ARTICLE}),
        )
        .await;
    let cut = &out["paragraphs"][4];
    assert_eq!(cut["chars"], chars);
    assert_eq!(cut["truncated"], true);
    assert_eq!(cut["text"].as_str().unwrap().chars().count(), 400);

    let whole = c
        .ok(
            "docs_list_paragraphs",
            json!({"account": "work", "doc_id": ARTICLE, "from": 5, "full": true}),
        )
        .await;
    assert_eq!(whole["paragraphs"][0]["truncated"], false);
    assert_eq!(
        whole["paragraphs"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        chars
    );
}

#[tokio::test]
async fn docs_edit_paragraph_previews_the_change_and_then_writes_it_once() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 1).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": ARTICLE, "paragraph": 3,
                      "find": "już", "occurrence": 2, "replace": "jutro",
                      "expect": "Zażółć", "revision_id": ARTICLE_REVISION});

    let shown = c.ok("docs_edit_paragraph", confirming(&args, false)).await;
    assert_eq!(shown["written"], false);
    let lines = details(&shown);
    assert!(lines.contains("Zażółć gęślą jaźń 😀 już i już"), "{lines}");
    assert!(
        lines.contains("Zażółć gęślą jaźń 😀 już i jutro"),
        "{lines}"
    );
    assert!(lines.contains("occurrence 2"), "{lines}");
    assert_eq!(batch_calls(&server).await, 0, "a preview wrote something");

    let out = c.ok("docs_edit_paragraph", confirming(&args, true)).await;
    assert_eq!(out["paragraph"], 3);
    assert_eq!(out["text"], "Zażółć gęślą jaźń 😀 już i jutro");
    assert!(
        out["next"]
            .as_str()
            .unwrap()
            .contains("docs_list_paragraphs"),
        "{out}"
    );
    // The second `już`, at the UTF-16 index the emoji before it decides.
    let body = last_batch(&server).await;
    assert_eq!(
        body["requests"],
        json!([
            {"insertText": {"text": "jutro", "location": {"index": 57}}},
            {"deleteContentRange": {"range": {"startIndex": 62, "endIndex": 65}}}
        ])
    );
    assert_eq!(body["writeControl"]["requiredRevisionId"], ARTICLE_REVISION);
    assert_eq!(batch_calls(&server).await, 1);
    drop(server);
}

#[tokio::test]
async fn a_stale_revision_or_an_expect_that_does_not_match_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 0).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let stale = c
        .refused(
            "docs_edit_paragraph",
            json!({"account": "work", "doc_id": ARTICLE, "paragraph": 3, "find": "już",
                   "occurrence": 1, "replace": "jutro", "expect": "Zażółć",
                   "revision_id": "ALm37BW0Older", "confirmed": true}),
        )
        .await;
    assert!(stale.contains("docs_list_paragraphs"), "{stale}");
    assert!(stale.contains("ALm37BW0Older"), "{stale}");

    let counted = c
        .refused(
            "docs_edit_paragraph",
            json!({"account": "work", "doc_id": ARTICLE, "paragraph": 3, "find": "już",
                   "occurrence": 1, "replace": "jutro", "expect": "Koniec",
                   "revision_id": ARTICLE_REVISION, "confirmed": true}),
        )
        .await;
    assert!(counted.contains("does not start with"), "{counted}");
    assert!(counted.contains("Zażółć gęślą"), "{counted}");

    let gone = c
        .refused(
            "docs_style_paragraph",
            json!({"account": "work", "doc_id": ARTICLE, "paragraph": 9, "style": "TITLE",
                   "expect": "Koniec", "revision_id": ARTICLE_REVISION, "confirmed": true}),
        )
        .await;
    assert!(gone.contains("5 paragraphs"), "{gone}");
    assert_eq!(batch_calls(&server).await, 0);
    drop(server);
}

#[tokio::test]
async fn docs_insert_text_puts_a_styled_paragraph_where_it_was_told() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 1).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                      "text": "Nowy akapit", "style": "heading_3", "expect": "Część",
                      "revision_id": ARTICLE_REVISION});
    let shown = c.ok("docs_insert_text", confirming(&args, false)).await;
    assert_eq!(shown["written"], false);
    let lines = details(&shown);
    assert!(lines.contains("Część pierwsza"), "{lines}");
    assert!(lines.contains("Nowy akapit"), "{lines}");
    assert!(lines.contains("HEADING_3"), "{lines}");

    let out = c.ok("docs_insert_text", confirming(&args, true)).await;
    assert_eq!(out["paragraph"], 2);
    assert_eq!(out["text"], "Nowy akapit");
    assert_eq!(
        last_batch(&server).await["requests"],
        json!([
            {"insertText": {"text": "Nowy akapit\n", "location": {"index": 30}}},
            {"updateParagraphStyle": {
                "range": {"startIndex": 30, "endIndex": 41},
                "paragraphStyle": {"namedStyleType": "HEADING_3"},
                "fields": "namedStyleType"
            }}
        ])
    );

    // Where it goes has to be said once and exactly once.
    for (where_it_goes, wanted) in [
        (
            json!({"after_paragraph": 2, "before_paragraph": 3}),
            "not both",
        ),
        (json!({}), "after_paragraph or before_paragraph"),
    ] {
        let mut bad = json!({"account": "work", "doc_id": ARTICLE, "text": "Nowy akapit",
                             "revision_id": ARTICLE_REVISION, "confirmed": true});
        for (key, value) in where_it_goes.as_object().unwrap() {
            bad[key] = value.clone();
        }
        let refused = c.refused("docs_insert_text", bad).await;
        assert!(refused.contains(wanted), "{refused}");
    }
    assert_eq!(batch_calls(&server).await, 1);
    drop(server);
}

#[tokio::test]
async fn docs_style_paragraph_changes_the_style_and_leaves_the_words() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 1).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": ARTICLE, "paragraph": 2,
                      "style": "HEADING_3", "expect": "Część pierwsza",
                      "revision_id": ARTICLE_REVISION});
    let shown = c.ok("docs_style_paragraph", confirming(&args, false)).await;
    let lines = details(&shown);
    assert!(lines.contains("HEADING_2"), "{lines}");
    assert!(lines.contains("HEADING_3"), "{lines}");
    assert!(lines.contains("Część pierwsza"), "{lines}");

    let out = c.ok("docs_style_paragraph", confirming(&args, true)).await;
    assert_eq!(out["text"], "Część pierwsza");
    assert!(
        out["written"].as_str().unwrap().contains("HEADING_3"),
        "{out}"
    );
    assert_eq!(
        last_batch(&server).await["requests"],
        json!([{"updateParagraphStyle": {
            "range": {"startIndex": 15, "endIndex": 30},
            "paragraphStyle": {"namedStyleType": "HEADING_3"},
            "fields": "namedStyleType"
        }}])
    );

    // A style Docs does not have never reaches Google.
    let refused = c
        .refused(
            "docs_style_paragraph",
            json!({"account": "work", "doc_id": ARTICLE, "paragraph": 2, "style": "BODY",
                   "expect": "Część", "revision_id": ARTICLE_REVISION, "confirmed": true}),
        )
        .await;
    assert!(refused.contains("NORMAL_TEXT"), "{refused}");
    assert_eq!(batch_calls(&server).await, 1);
    drop(server);
}

#[tokio::test]
async fn docs_insert_code_writes_the_text_the_font_and_every_span_in_one_batch() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 1).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let args = json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
    "code": "let ż = \"😀\";", "expect": "Część",
    "revision_id": ARTICLE_REVISION, "size_pt": 8,
    "spans": [
        {"start": 0, "end": 3, "colour": "#ff0000", "bold": true},
        {"start": 8, "end": 11, "color": "#00ff00", "italic": true}
    ]});

    let shown = c.ok("docs_insert_code", confirming(&args, false)).await;
    let lines = details(&shown);
    assert!(
        lines.contains("2 spans"),
        "the preview counts the spans: {lines}"
    );
    // The size is named, not merely accepted. A person approving a listing has
    // to see it, and a model has to see that the size it asked for was
    // understood rather than dropped by a build that never knew the argument.
    assert!(lines.contains("Courier New 8 pt"), "{lines}");
    assert!(
        lines.contains("let ż = \"😀\";"),
        "the preview shows the code: {lines}"
    );
    assert!(
        !lines.contains("#ff0000"),
        "the preview lists no spans: {lines}"
    );

    let out = c.ok("docs_insert_code", confirming(&args, true)).await;
    assert!(
        out["written"].as_str().unwrap().contains("one batch"),
        "{out}"
    );
    let body = last_batch(&server).await;
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 4, "the text, the font and one per span");
    assert_eq!(
        requests[0]["insertText"],
        json!({"text": "let ż = \"😀\";\n", "location": {"index": 30}})
    );
    assert_eq!(
        requests[1]["updateTextStyle"]["textStyle"]["weightedFontFamily"]["fontFamily"],
        "Courier New"
    );
    assert_eq!(
        requests[1]["updateTextStyle"]["range"],
        json!({"startIndex": 30, "endIndex": 43})
    );
    assert_eq!(
        requests[2]["updateTextStyle"]["range"],
        json!({"startIndex": 30, "endIndex": 33})
    );
    // Characters 8 to 11 of the code are the quoted emoji, which is units 8
    // to 12: the American spelling of `colour` is taken too.
    assert_eq!(
        requests[3]["updateTextStyle"]["range"],
        json!({"startIndex": 38, "endIndex": 42})
    );
    assert_eq!(
        requests[3]["updateTextStyle"]["textStyle"]["foregroundColor"]["color"]["rgbColor"],
        json!({"red": 0.0, "green": 1.0, "blue": 0.0})
    );
    assert_eq!(
        requests[3]["updateTextStyle"]["fields"],
        "foregroundColor,italic"
    );
    assert_eq!(body["writeControl"]["requiredRevisionId"], ARTICLE_REVISION);

    // A listing that cannot be coloured completely is not written at all.
    let mut overlapping = args.as_object().unwrap().clone();
    overlapping.insert(
        "spans".into(),
        json!([{"start": 0, "end": 5, "colour": "#ff0000"},
               {"start": 3, "end": 8, "colour": "#00ff00"}]),
    );
    overlapping.insert("confirmed".into(), json!(true));
    let refused = c
        .refused("docs_insert_code", Value::Object(overlapping))
        .await;
    assert!(refused.contains("overlap"), "{refused}");
    assert_eq!(batch_calls(&server).await, 1);
    drop(server);
}

/// The two reads a table write makes: the article as it is, and then the
/// article with the empty two-by-two table Google has just put in it.
async fn table_server() -> MockServer {
    let server = google_server().await;
    let at = format!("/v1/documents/{ARTICLE}");
    Mock::given(http_method("GET"))
        .and(path(at.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_article.json")))
        .up_to_n_times(1)
        .named("the document as the caller read it")
        .mount(&server)
        .await;
    mount(&server, "GET", &at, fixture("docs_article_table.json")).await;
    server
}

/// The grid the person in the example is putting in their article.
fn table_args(header: bool) -> Value {
    json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
           "rows": [["Model", "Parametry"], ["Mistral-7B", "7 mld"]],
           "header": header, "expect": "Część",
           "revision_id": ARTICLE_REVISION})
}

#[tokio::test]
async fn docs_insert_table_shows_the_grid_and_writes_nothing_until_it_is_confirmed() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 0).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let shown = c
        .ok("docs_insert_table", confirming(&table_args(true), false))
        .await;
    assert_eq!(shown["written"], false);
    assert!(
        shown["action"].as_str().unwrap().contains("2 by 2 table"),
        "{shown}"
    );
    let lines = details(&shown);
    assert!(lines.contains("Część pierwsza"), "{lines}");
    assert!(lines.contains("Model | Parametry"), "{lines}");
    assert!(lines.contains("Mistral-7B | 7 mld"), "{lines}");
    assert!(lines.contains("first row is set bold"), "{lines}");
    // The preview says what is unusual about this tool before it does it.
    assert!(lines.contains("writes twice"), "{lines}");
    assert_eq!(batch_calls(&server).await, 0);

    // A grid that is not a table is refused in the same words, confirmed or
    // not, and nothing is read back or written.
    let mut ragged = table_args(false);
    ragged["rows"] = json!([["Model", "Parametry"], ["Mistral-7B"]]);
    let refused = c
        .refused("docs_insert_table", confirming(&ragged, true))
        .await;
    assert!(refused.contains("row 2 holds 1 cell"), "{refused}");
    assert!(refused.contains("Nothing was written"), "{refused}");
    assert_eq!(batch_calls(&server).await, 0);
    drop(server);
}

#[tokio::test]
async fn docs_insert_table_fills_the_cells_from_the_document_it_reads_back() {
    let db = Db::open_memory().await.unwrap();
    let server = table_server().await;
    expect_batches(&server, 2).await;
    let mut c = client(&db, &server, &["docs:read", "docs:write"]).await;

    let out = c
        .ok("docs_insert_table", confirming(&table_args(true), true))
        .await;
    assert_eq!(out["paragraph"], 2);
    assert_eq!(out["text"], "Model | Parametry\nMistral-7B | 7 mld");
    let written = out["written"].as_str().unwrap();
    // The answer says where the cells are, in the numbers the next call uses.
    assert!(written.contains("paragraphs 4 to 7"), "{written}");
    assert!(written.contains("two writes"), "{written}");
    assert!(written.contains("first row is bold"), "{written}");

    let sent = batch_bodies(&server).await;
    assert_eq!(sent.len(), 2, "the empty grid, and then its cells");
    assert_eq!(
        sent[0],
        json!({
            "requests": [{"insertTable": {
                "rows": 2, "columns": 2, "location": {"index": 30}
            }}],
            "writeControl": {"requiredRevisionId": ARTICLE_REVISION}
        })
    );
    // Every index here is one the re-read answered with, and they are used
    // from the last cell to the first so that no insert moves the next one.
    assert_eq!(
        sent[1]["requests"],
        json!([
            {"insertText": {"text": "7 mld", "location": {"index": 40}}},
            {"insertText": {"text": "Mistral-7B", "location": {"index": 38}}},
            {"insertText": {"text": "Parametry", "location": {"index": 35}}},
            {"updateTextStyle": {
                "range": {"startIndex": 35, "endIndex": 44},
                "textStyle": {"bold": true},
                "fields": "bold"
            }},
            {"insertText": {"text": "Model", "location": {"index": 33}}},
            {"updateTextStyle": {
                "range": {"startIndex": 33, "endIndex": 38},
                "textStyle": {"bold": true},
                "fields": "bold"
            }}
        ])
    );
    // The second batch is planned against this server's own re-read, not
    // against the revision the caller was given.
    assert_eq!(
        sent[1]["writeControl"]["requiredRevisionId"],
        "ALm37BW0Article2"
    );
    assert_ne!(
        sent[1]["writeControl"]["requiredRevisionId"],
        ARTICLE_REVISION
    );
    drop(server);
}

const LISTING: &str = "1LiStInGdOcIdExAmPlE0123456789abcdef";

/// What docs_read is unable to answer: a paragraph nobody styled as one bare
/// run, and a coloured listing in the character offsets that wrote it.
#[tokio::test]
async fn docs_read_formatting_answers_runs_a_span_can_be_written_from() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/v1/documents/{LISTING}"),
        fixture("docs_listing.json"),
    )
    .await;
    let mut c = client(&db, &server, &["docs:read"]).await;

    let plain = c
        .ok(
            "docs_read_formatting",
            json!({"account": "work", "doc_id": LISTING, "paragraph": 1}),
        )
        .await;
    assert_eq!(plain["revision_id"], "ALm37BW0Listing1");
    assert_eq!(plain["count"], 2);
    assert_eq!(plain["from"], 1);
    assert_eq!(plain["to"], 1);
    let runs = plain["paragraphs"][0]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    // A wall of defaults is what this tool exists not to send: a run the
    // document says nothing about carries three keys and no more.
    assert_eq!(runs[0], json!({"start": 0, "end": 8, "text": "Przykład"}));

    let listing = c
        .ok(
            "docs_read_formatting",
            json!({"account": "work", "doc_id": LISTING, "paragraph": 2}),
        )
        .await;
    let runs = listing["paragraphs"][0]["runs"].as_array().unwrap();
    // Four runs, where docs_insert_code would have written two spans and one
    // font over the whole block: Docs merged what shares a style, which is
    // why the note says to compare the colour at an offset.
    assert_eq!(runs.len(), 4);
    assert_eq!(runs[0]["colour"], "#ff0000");
    assert_eq!(runs[0]["bold"], true);
    assert!(runs[0]["italic"].is_null(), "{}", runs[0]);
    // The quoted emoji: three characters, four UTF-16 units. The offsets are
    // the ones docs_insert_code takes.
    assert_eq!(
        (runs[2]["start"].as_u64(), runs[2]["end"].as_u64()),
        (Some(8), Some(11))
    );
    assert_eq!(runs[2]["text"], "\"😀\"");
    assert_eq!(runs[2]["colour"], "#00ff00");
    assert_eq!(runs[3]["font"], "Courier New");
    assert_eq!(runs[3]["size"], 10.0);
    assert!(
        listing["note"].as_str().unwrap().contains("merges"),
        "{}",
        listing["note"]
    );

    // The whole document, and then a range that says nothing at all.
    let all = c
        .ok(
            "docs_read_formatting",
            json!({"account": "work", "doc_id": LISTING}),
        )
        .await;
    assert_eq!(all["paragraphs"].as_array().unwrap().len(), 2);
    let refused = c
        .refused(
            "docs_read_formatting",
            json!({"account": "work", "doc_id": LISTING, "paragraph": 1, "to": 2}),
        )
        .await;
    assert!(refused.contains("once"), "{refused}");
    drop(server);
}

// ----- a picture in a document ------------------------------------------------

/// A small PNG, the size a diagram actually is. `png()` is 1600x1200 and slow
/// to encode; nothing about these tests needs it to be large.
fn small_png(width: u32, height: u32) -> Vec<u8> {
    use image::{ImageFormat, Rgb, RgbImage};
    let img = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

/// The article, on a router whose staging directory this test owns.
async fn inserting(db: &Db, server: &MockServer) -> Attaching {
    Attaching::new(db, server, &["docs:read", "docs:write"]).await
}

async fn upload_picture(a: &mut Attaching, filename: &str, bytes: &[u8]) -> Value {
    a.upload_with("docs_upload_link", json!({"filename": filename}), bytes)
        .await
}

/// The whole path: a ticket, the bytes, a preview that writes nothing, and
/// then one batchUpdate whose picture Google can actually fetch.
#[tokio::test]
async fn docs_insert_image_puts_a_staged_picture_where_google_can_fetch_it() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 1).await;
    let mut a = inserting(&db, &server).await;
    let picture = small_png(320, 240);

    let uploaded = upload_picture(&mut a, "wykres.png", &picture).await;
    assert_eq!(uploaded["mime_type"], "image/png");
    assert_eq!(a.staged_files(), 1);

    let args = json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                      "upload_id": uploaded["upload_id"], "width_pt": 300,
                      "expect": "Część", "revision_id": ARTICLE_REVISION});

    let shown = a
        .client
        .ok("docs_insert_image", confirming(&args, false))
        .await;
    let lines = details(&shown);
    assert!(lines.contains("wykres.png"), "{lines}");
    assert!(lines.contains("320×240 pixels"), "{lines}");
    assert!(lines.contains("300 points wide"), "{lines}");
    assert!(lines.contains("no alt text"), "{lines}");
    // A preview spends nothing: the file is still staged and the id still works.
    assert_eq!(a.staged_files(), 1);
    assert_eq!(batch_calls(&server).await, 0);

    let out = a
        .client
        .ok("docs_insert_image", confirming(&args, true))
        .await;
    assert_eq!(out["paragraph"], 2);
    assert_eq!(out["text"], "wykres.png");
    assert!(
        out["written"].as_str().unwrap().contains("wykres.png"),
        "{out}"
    );

    // The batch is the break and the picture, in that order, in one write
    // guarded by the revision the read answered with.
    let body = last_batch(&server).await;
    let requests = body["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2, "{body}");
    assert_eq!(
        requests[0]["insertText"],
        json!({"text": "\n", "location": {"index": 30}})
    );
    assert_eq!(
        requests[1]["insertInlineImage"]["location"],
        json!({"index": 30})
    );
    assert_eq!(
        requests[1]["insertInlineImage"]["objectSize"],
        json!({"width": {"magnitude": 300.0, "unit": "PT"}})
    );
    assert_eq!(body["writeControl"]["requiredRevisionId"], ARTICLE_REVISION);

    // And the URI Google was handed is a download link on this server that
    // serves the bytes that were staged.
    let uri = requests[1]["insertInlineImage"]["uri"].as_str().unwrap();
    assert!(uri.starts_with("https://gmcp.example/dl/"), "{uri}");
    let (status, body, headers) = a.fetch(uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, picture);
    assert_eq!(headers.get("content-type").unwrap(), "image/png");

    // The link is spent like any other: three fetches and then nothing, and
    // the upload id is gone whether or not it is fetched again.
    for _ in 0..2 {
        assert_eq!(a.fetch(uri).await.0, StatusCode::OK);
    }
    assert_eq!(a.fetch(uri).await.0, StatusCode::NOT_FOUND);
    let refused = a
        .client
        .refused("docs_insert_image", confirming(&args, true))
        .await;
    assert!(refused.contains("there is no staged upload"), "{refused}");
    assert!(refused.contains("docs_upload_link"), "{refused}");
    assert_eq!(batch_calls(&server).await, 1);
    drop(server);
}

/// A file Docs would not fetch, refused before a call is spent on it.
#[tokio::test]
async fn a_file_docs_cannot_hold_is_refused_before_any_call() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 0).await;
    let mut a = inserting(&db, &server).await;

    // A PDF, named as one by the uploader.
    let pdf = upload_picture(&mut a, "raport.pdf", b"%PDF-1.7 not a picture").await;
    let args = json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                      "upload_id": pdf["upload_id"], "revision_id": ARTICLE_REVISION,
                      "confirmed": true});
    let refused = a.client.refused("docs_insert_image", args).await;
    assert!(refused.contains("application/pdf"), "{refused}");
    assert!(refused.contains("image/png"), "{refused}");
    // The upload survives a refusal, so the same file can be converted and
    // the id used again.
    assert_eq!(a.staged_files(), 1);

    // A picture whose header claims more pixels than Docs takes. The bytes
    // are a real PNG header and nothing else: the count is read from it
    // rather than from what the caller said.
    let vast = upload_picture(&mut a, "mapa.png", &png_claiming(6000, 5000)).await;
    let refused = a
        .client
        .refused(
            "docs_insert_image",
            json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                   "upload_id": vast["upload_id"], "revision_id": ARTICLE_REVISION,
                   "confirmed": true}),
        )
        .await;
    assert!(refused.contains("6000×5000"), "{refused}");
    assert!(refused.contains("25 megapixels"), "{refused}");
    drop(server);
}

/// A PNG header claiming a size, and nothing behind it. Small enough to be a
/// test and large enough to be refused.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    for chunk in [ihdr, b"IDAT".to_vec(), b"IEND".to_vec()] {
        out.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    }
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// The two locks, and somebody else's file. None of the three writes, and
/// none of them spends the upload.
#[tokio::test]
async fn a_picture_is_not_inserted_on_a_stale_read_or_somebody_elses_upload() {
    let db = Db::open_memory().await.unwrap();
    let server = article_server().await;
    expect_batches(&server, 0).await;
    let mut anna = inserting(&db, &server).await;
    let uploaded = upload_picture(&mut anna, "wykres.png", &small_png(64, 48)).await;

    // A revision that has moved on: the document is read, the plan is built
    // and refused, and nothing is sent.
    let stale = anna
        .client
        .refused(
            "docs_insert_image",
            json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                   "upload_id": uploaded["upload_id"], "revision_id": "ALm37BW0Older",
                   "confirmed": true}),
        )
        .await;
    assert!(stale.contains("changed since it was read"), "{stale}");
    assert_eq!(anna.staged_files(), 1, "a refusal spends no upload");

    // `expect` against the wrong paragraph, the same.
    let wrong = anna
        .client
        .refused(
            "docs_insert_image",
            json!({"account": "work", "doc_id": ARTICLE, "after_paragraph": 2,
                   "upload_id": uploaded["upload_id"], "expect": "Koniec",
                   "revision_id": ARTICLE_REVISION, "confirmed": true}),
        )
        .await;
    assert!(wrong.contains("does not start with"), "{wrong}");
    assert_eq!(anna.staged_files(), 1);

    // And a second person, on the same server and so the same staging store,
    // cannot put Anna's file in their own document.
    let marta = user(&db, "marta", "marta@example.test").await;
    connect(&db, &marta, "marta-work", &["docs"], false).await;
    let (_, secret) = token(
        &db,
        &["docs:read", "docs:write"],
        Some(&marta),
        ClientProfile::Generic,
    )
    .await;
    let mut hers = Client::new(anna.app.clone(), secret);
    hers.initialize().await;
    let refused = hers
        .refused(
            "docs_insert_image",
            json!({"account": "marta-work", "doc_id": ARTICLE, "after_paragraph": 2,
                   "upload_id": uploaded["upload_id"], "revision_id": ARTICLE_REVISION,
                   "confirmed": true}),
        )
        .await;
    assert!(refused.contains("there is no staged upload"), "{refused}");
    assert_eq!(anna.staged_files(), 1, "and it is still Anna's");
    drop(server);
}

// ----- sheets -----------------------------------------------------------------

#[tokio::test]
async fn sheets_list_tabs_answers_the_tabs_and_their_dimensions() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/v4/spreadsheets/{SHEET}"),
        fixture("sheets_spreadsheet.json"),
    )
    .await;
    let mut c = client(&db, &server, &["sheets:read"]).await;

    let out = c
        .ok(
            "sheets_list_tabs",
            json!({"account": "work", "spreadsheet_id": SHEET}),
        )
        .await;
    assert_eq!(out["title"], "Support hours 2026");
    assert_eq!(out["spreadsheet_id"], SHEET);
    assert_eq!(out["tabs"][0]["title"], "September");
    assert_eq!(out["tabs"][0]["rows"], 200);
    assert_eq!(out["tabs"][0]["columns"], 12);
    assert_eq!(out["tabs"][1]["title"], "Rates");
}

#[tokio::test]
async fn sheets_read_range_answers_rows_and_says_when_it_cut_them() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("GET"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_values.json")))
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read"]).await;

    let out = c
        .ok(
            "sheets_read_range",
            json!({"account": "work", "spreadsheet_id": SHEET, "range": "September!A1:D4"}),
        )
        .await;
    assert_eq!(out["range"], "September!A1:D4");
    assert_eq!(out["row_count"], 4);
    assert_eq!(out["truncated"], false);
    assert_eq!(out["rows"][0][0], "Date");
    assert_eq!(out["rows"][3][1], "Phoenix");

    // `max_rows` cuts, and says so rather than leaving it to be guessed.
    let short = c
        .ok(
            "sheets_read_range",
            json!({"account": "work", "spreadsheet_id": SHEET,
                   "range": "September!A1:D4", "max_rows": 2}),
        )
        .await;
    assert_eq!(short["row_count"], 2);
    assert_eq!(short["truncated"], true);
}

#[tokio::test]
async fn sheets_update_range_previews_first_and_then_writes_once() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("PUT"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_update.json")))
        .expect(1)
        .named("values.update, exactly once")
        .mount(&server)
        .await;
    // The preview reads the range as formulas before it says anything.
    Mock::given(http_method("GET"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_values.json")))
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;

    let args = json!({"account": "work", "spreadsheet_id": SHEET,
                      "range": "September!A2:D2",
                      "rows": [["2026-09-01", "Phoenix", "1.5", "Restarted the exporter"]]});
    let mut preview = args.as_object().unwrap().clone();
    preview.insert("confirmed".into(), json!(false));
    let shown = c.ok("sheets_update_range", Value::Object(preview)).await;
    assert_eq!(shown["written"], false);
    assert!(
        shown["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("there is no undo here")),
        "{shown}"
    );

    let mut confirmed = args.as_object().unwrap().clone();
    confirmed.insert("confirmed".into(), json!(true));
    let out = c.ok("sheets_update_range", Value::Object(confirmed)).await;
    assert_eq!(out["updated_range"], "September!A2:D2");
    assert_eq!(out["updated_rows"], 1);
    assert_eq!(out["updated_cells"], 4);
    assert!(out["written"].as_str().unwrap().contains("1 rows written"));
    drop(server);
}

#[tokio::test]
async fn sheets_add_tab_previews_and_then_adds_the_tab() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_add_sheet.json")))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;

    let args = json!({"account": "work", "spreadsheet_id": SHEET, "title": "October"});
    let mut preview = args.as_object().unwrap().clone();
    preview.insert("confirmed".into(), json!(false));
    let shown = c.ok("sheets_add_tab", Value::Object(preview)).await;
    assert_eq!(shown["written"], false);
    assert!(shown["action"].as_str().unwrap().contains("\"October\""));

    let mut confirmed = args.as_object().unwrap().clone();
    confirmed.insert("confirmed".into(), json!(true));
    let out = c.ok("sheets_add_tab", Value::Object(confirmed)).await;
    assert_eq!(out["updated_range"], "October");
    assert_eq!(out["updated_rows"], 1000);
    assert_eq!(out["written"], "tab \"October\" added");

    // A tab with no title is refused before anything is asked of Google.
    let bad = c
        .refused(
            "sheets_add_tab",
            json!({"account": "work", "spreadsheet_id": SHEET,
                   "title": " ", "confirmed": true}),
        )
        .await;
    assert!(bad.contains("needs a title"), "{bad}");
    drop(server);
}

#[tokio::test]
async fn sheets_create_previews_the_spreadsheet_and_writes_nothing() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    server
        .register(
            Mock::given(path("/upload/drive/v3/files"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .named("an unconfirmed sheets_create reaches Drive"),
        )
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;

    let out = c
        .ok(
            "sheets_create",
            json!({"account": "work", "title": "Support hours 2027",
                   "tabs": ["October", "November"],
                   "rows": [["Date", "Customer", "Hours"]], "confirmed": false}),
        )
        .await;
    assert_eq!(out["confirmed"], false);
    assert_eq!(out["written"], false);
    assert!(
        out["action"]
            .as_str()
            .unwrap()
            .contains("Support hours 2027"),
        "{out}"
    );
    assert_eq!(
        out["details"][0],
        "1 rows on the first tab, plus empty tabs October, November"
    );
    assert_eq!(out["details"][1], "Date | Customer | Hours");
    drop(server);
}

/// The body of the last `batchUpdate` the mock server saw.
async fn last_batch(server: &MockServer) -> Value {
    let request = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .rfind(|r| r.method.as_str() == "POST" && r.url.path().ends_with(":batchUpdate"))
        .expect("a batchUpdate was sent");
    serde_json::from_slice(&request.body).expect("the batchUpdate body is JSON")
}

/// Every `batchUpdate` body the mock server saw, oldest first.
async fn batch_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with(":batchUpdate"))
        .map(|r| serde_json::from_slice(&r.body).expect("a batchUpdate body is JSON"))
        .collect()
}

/// How many structural changes the mock server was asked for.
async fn batch_calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().ends_with(":batchUpdate"))
        .count()
}

/// The tab lookup every structural tool starts with.
async fn mount_spreadsheet(server: &MockServer) {
    mount(
        server,
        "GET",
        &format!("/v4/spreadsheets/{SHEET}"),
        fixture("sheets_spreadsheet.json"),
    )
    .await;
}

#[tokio::test]
async fn sheets_read_range_reads_each_mode_and_answers_strings_in_all_of_them() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    for (option, body) in [
        ("FORMATTED_VALUE", "sheets_values.json"),
        ("FORMULA", "sheets_values_formulas.json"),
        ("UNFORMATTED_VALUE", "sheets_values_unformatted.json"),
    ] {
        Mock::given(http_method("GET"))
            .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
            .and(query_param("valueRenderOption", option))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture(body)))
            .mount(&server)
            .await;
    }
    let mut c = client(&db, &server, &["sheets:read"]).await;
    let args = |render: Value| {
        json!({"account": "work", "spreadsheet_id": SHEET,
               "range": "September!A1:D4", "render": render})
    };

    // The default is what the sheet shows, and it says which mode it read in.
    let formatted = c.ok("sheets_read_range", args(Value::Null)).await;
    assert_eq!(formatted["render"], "formatted");
    assert_eq!(formatted["rows"][1][2], "1.5");

    let formula = c.ok("sheets_read_range", args(json!("formula"))).await;
    assert_eq!(formula["render"], "formula");
    assert_eq!(formula["rows"][0][0], "=SUM(C10:C20)");

    // UNFORMATTED_VALUE answers JSON numbers and booleans; the tool's rows
    // are strings whatever the mode, so nothing downstream changes shape.
    let raw = c.ok("sheets_read_range", args(json!("unformatted"))).await;
    assert_eq!(raw["render"], "unformatted");
    assert_eq!(raw["rows"][1][0], "46266");
    assert_eq!(raw["rows"][1][2], "1.5");
    assert_eq!(raw["rows"][1][3], "TRUE");

    // A mode that does not exist is refused with the modes that do, rather
    // than quietly read as the default.
    let bad = c.refused("sheets_read_range", args(json!("raw"))).await;
    assert!(bad.contains("formatted"), "{bad}");
    assert!(bad.contains("unformatted"), "{bad}");
}

#[tokio::test]
async fn an_unconfirmed_update_names_the_formulas_it_would_overwrite() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    Mock::given(http_method("GET"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
        .and(query_param("valueRenderOption", "FORMULA"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("sheets_values_formulas.json")),
        )
        .mount(&server)
        .await;
    server
        .register(
            Mock::given(http_method("PUT"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .named("an unconfirmed update writes nothing"),
        )
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;

    let shown = c
        .ok(
            "sheets_update_range",
            json!({"account": "work", "spreadsheet_id": SHEET,
                   "range": "September!C2:D4",
                   "rows": [["10", "20"], ["11", "21"], ["12", "22"]],
                   "confirmed": false}),
        )
        .await;
    assert_eq!(shown["written"], false);
    let warning = shown["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .find(|d| d.contains("formula"))
        .unwrap_or_else(|| panic!("no formula warning in {shown}"))
        .to_string();
    // Two of the six cells hold a formula, and the preview says which, by
    // the reference the person reads off the sheet.
    assert!(warning.starts_with("2 of the cells"), "{warning}");
    assert!(warning.contains("C2 =SUM(C10:C20)"), "{warning}");
    assert!(warning.contains("D3 =C3*1.23"), "{warning}");
    assert!(warning.contains("render=\"formula\""), "{warning}");
    drop(server);
}

#[tokio::test]
async fn sheets_insert_rows_counts_from_one_and_inherits_from_the_row_above() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount_spreadsheet(&server).await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_batch_rows.json")))
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;
    let args = |at_row: u32, confirmed: bool| {
        json!({"account": "work", "spreadsheet_id": SHEET, "tab": "September",
               "at_row": at_row, "count": 3, "confirmed": confirmed})
    };

    let shown = c.ok("sheets_insert_rows", args(5, false)).await;
    assert_eq!(shown["written"], false);
    assert!(
        shown["action"].as_str().unwrap().contains("row 5"),
        "{shown}"
    );
    assert_eq!(batch_calls(&server).await, 0, "a preview changes nothing");

    // Row 5 on the sheet is startIndex 4 for the API, and the tab was looked
    // up by title: September is sheet 0 in the fixture.
    c.ok("sheets_insert_rows", args(5, true)).await;
    assert_eq!(
        last_batch(&server).await,
        json!({"requests": [{"insertDimension": {
            "range": {"sheetId": 0, "dimension": "ROWS",
                      "startIndex": 4, "endIndex": 7},
            "inheritFromBefore": true}}]})
    );

    // At the top of the sheet there is no row above to inherit from, and the
    // API refuses the flag there.
    c.ok("sheets_insert_rows", args(1, true)).await;
    assert_eq!(
        last_batch(&server).await,
        json!({"requests": [{"insertDimension": {
            "range": {"sheetId": 0, "dimension": "ROWS",
                      "startIndex": 0, "endIndex": 3},
            "inheritFromBefore": false}}]})
    );

    // Rows are numbered the way the sheet numbers them, and a tab that is not
    // there is named along with the ones that are.
    let zero = c.refused("sheets_insert_rows", args(0, true)).await;
    assert!(zero.contains("counts from 1"), "{zero}");
    let missing = c
        .refused(
            "sheets_insert_rows",
            json!({"account": "work", "spreadsheet_id": SHEET, "tab": "October",
                   "at_row": 2, "count": 1, "confirmed": true}),
        )
        .await;
    assert!(missing.contains("\"September\""), "{missing}");
    assert_eq!(
        batch_calls(&server).await,
        2,
        "one write per confirmed call"
    );
}

#[tokio::test]
async fn sheets_delete_rows_shows_what_would_go_and_then_deletes_once() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount_spreadsheet(&server).await;
    Mock::given(http_method("GET"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_values.json")))
        .mount(&server)
        .await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_batch_rows.json")))
        .expect(1)
        .named("deleteDimension, exactly once")
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;
    let args = |confirmed: bool| {
        json!({"account": "work", "spreadsheet_id": SHEET, "tab": "September",
               "from_row": 2, "count": 3, "confirmed": confirmed})
    };

    // The preview reads the doomed rows and prints them, because a wrong row
    // number is the only way this tool goes wrong.
    let shown = c.ok("sheets_delete_rows", args(false)).await;
    assert_eq!(shown["written"], false);
    let details: Vec<&str> = shown["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    assert!(details[0].contains("rows 2 to 4"), "{details:?}");
    assert!(
        details.iter().any(|d| d.contains("Restarted the exporter")),
        "{details:?}"
    );
    assert_eq!(batch_calls(&server).await, 0, "a preview deletes nothing");

    let out = c.ok("sheets_delete_rows", args(true)).await;
    assert_eq!(out["updated_range"], "'September'!2:4");
    assert_eq!(out["updated_rows"], 3);
    assert_eq!(
        last_batch(&server).await,
        json!({"requests": [{"deleteDimension": {
            "range": {"sheetId": 0, "dimension": "ROWS",
                      "startIndex": 1, "endIndex": 4}}}]})
    );
    drop(server);
}

#[tokio::test]
async fn sheets_copy_format_pastes_the_formatting_of_one_whole_row() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount_spreadsheet(&server).await;
    Mock::given(http_method("POST"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+:batchUpdate$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_batch_rows.json")))
        .expect(1)
        .named("copyPaste, exactly once")
        .mount(&server)
        .await;
    let mut c = client(&db, &server, &["sheets:read", "sheets:write"]).await;
    let args = |confirmed: bool| {
        json!({"account": "work", "spreadsheet_id": SHEET, "tab": "September",
               "from_row": 4, "to_row": 5, "count": 2, "confirmed": confirmed})
    };

    let shown = c.ok("sheets_copy_format", args(false)).await;
    assert_eq!(shown["written"], false);
    assert!(
        shown["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("only the formatting travels")),
        "{shown}"
    );

    let out = c.ok("sheets_copy_format", args(true)).await;
    assert_eq!(out["updated_range"], "'September'!5:6");
    assert_eq!(
        last_batch(&server).await,
        json!({"requests": [{"copyPaste": {
            "source": {"sheetId": 0, "startRowIndex": 3, "endRowIndex": 4},
            "destination": {"sheetId": 0, "startRowIndex": 4, "endRowIndex": 6},
            "pasteType": "PASTE_FORMAT",
            "pasteOrientation": "NORMAL"}}]})
    );
    drop(server);
}

#[tokio::test]
async fn the_row_tools_belong_to_sheets_write() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let rows = [
        "sheets_insert_rows",
        "sheets_delete_rows",
        "sheets_copy_format",
    ];

    let anna = user(&db, "anna", "anna@example.test").await;
    connect(&db, &anna, "work", &["sheets"], true).await;
    let (_, read_only) = token(&db, &["sheets:read"], Some(&anna), ClientProfile::Generic).await;
    let (_, may_write) = token(
        &db,
        &["sheets:read", "sheets:write"],
        Some(&anna),
        ClientProfile::Generic,
    )
    .await;
    let app = Arc::new(app(&db, Some(&server)).await);

    let mut reader = Client::new((*app).clone(), read_only);
    reader.initialize().await;
    let visible = reader.names().await;
    for tool in rows {
        assert!(!visible.contains(&tool.to_string()), "{tool} is a write");
    }
    assert!(visible.contains(&"sheets_read_range".to_string()));
    let refused = reader
        .refused(
            "sheets_delete_rows",
            json!({"account": "work", "spreadsheet_id": SHEET, "tab": "September",
                   "from_row": 2, "count": 1, "confirmed": true}),
        )
        .await;
    assert!(refused.contains("sheets:write"), "{refused}");

    let mut writer = Client::new((*app).clone(), may_write);
    writer.initialize().await;
    let visible = writer.names().await;
    for tool in rows {
        assert!(visible.contains(&tool.to_string()), "{tool} is missing");
    }
}

// ----- clocks -----------------------------------------------------------------
//
// Every instant a tool emits is the acting person's own wall-clock time with
// the offset in force that day, and every time an argument carries without an
// offset is read on that same clock. A model that was told "11:35Z" would tell
// the person their mail arrived two hours before it did, and one that booked
// "3pm" as 15:00Z would put the meeting an hour off; these say so.

#[test]
fn an_instant_carries_the_offset_that_was_in_force_that_day() {
    let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
    let january = super::dto::instant(Some(at("2026-01-15T12:00:00Z")), HOUSE);
    assert_eq!(january.as_deref(), Some("2026-01-15T13:00:00+01:00"));
    let july = super::dto::instant(Some(at("2026-07-15T12:00:00Z")), HOUSE);
    assert_eq!(july.as_deref(), Some("2026-07-15T14:00:00+02:00"));
    // Nothing is invented for an absent instant.
    assert_eq!(super::dto::instant(None, HOUSE), None);
}

#[tokio::test]
async fn one_stored_instant_is_shown_on_each_person_s_own_clock() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    mount(
        &server,
        "GET",
        &format!("/drive/v3/files/{PDF}"),
        fixture("drive_file.json"),
    )
    .await;

    let anna = user_in(&db, "anna", "anna@example.test", HOUSE).await;
    connect(&db, &anna, "work", &["drive"], false).await;
    let (_, for_anna) = token(&db, &["drive:read"], Some(&anna), ClientProfile::Generic).await;
    let bruno = user_in(
        &db,
        "bruno",
        "bruno@example.test",
        chrono_tz::America::New_York,
    )
    .await;
    connect(&db, &bruno, "work", &["drive"], false).await;
    let (_, for_bruno) = token(&db, &["drive:read"], Some(&bruno), ClientProfile::Generic).await;

    let ask = json!({"account": "work", "file_id": PDF});
    let mut a = Client::new(app(&db, Some(&server)).await, for_anna);
    a.initialize().await;
    let mut b = Client::new(app(&db, Some(&server)).await, for_bruno);
    b.initialize().await;
    let anna_saw = a.ok("drive_get_file", ask.clone()).await;
    let bruno_saw = b.ok("drive_get_file", ask).await;

    // One row in Drive, one instant, two clocks.
    assert_eq!(anna_saw["modified_time"], "2026-09-03T11:12:00+02:00");
    assert_eq!(bruno_saw["modified_time"], "2026-09-03T05:12:00-04:00");
    drop(server);
}

#[tokio::test]
async fn list_accounts_says_which_clock_and_what_time_it_is_on_it() {
    let db = Db::open_memory().await.unwrap();
    let server = google_server().await;
    let mut c = client(&db, &server, &["gmail:read"]).await;
    let out = c.ok("list_accounts", json!({})).await;
    assert_eq!(out["timezone"], "Europe/Warsaw");
    let now = out["now"].as_str().expect("the time where the person is");
    let now = DateTime::parse_from_rfc3339(now).expect("an RFC 3339 instant");
    assert!(!now.to_rfc3339().ends_with('Z'), "{now}");
    assert!((Utc::now() - now.with_timezone(&Utc)).num_seconds().abs() < 60);
    drop(server);
}

#[test]
fn a_time_without_an_offset_is_read_on_the_person_s_clock_in_both_seasons() {
    let read = |s: &str| super::drive::instant(s, HOUSE).unwrap();
    // Warsaw is +01:00 in January and +02:00 in July, so the same wall-clock
    // time is two different instants.
    assert_eq!(
        read("2026-01-15T14:00").at.to_rfc3339(),
        "2026-01-15T13:00:00+00:00"
    );
    assert_eq!(
        read("2026-07-15T14:00").at.to_rfc3339(),
        "2026-07-15T12:00:00+00:00"
    );
    // Seconds, and a space in place of the T, are the same time.
    assert_eq!(read("2026-07-15 14:00:00").at, read("2026-07-15T14:00").at);
    // A bare date is the start of that day where the person is.
    assert_eq!(
        read("2026-07-15").at.to_rfc3339(),
        "2026-07-14T22:00:00+00:00"
    );
    assert!(read("2026-07-15T14:00").note.is_none());
}

#[test]
fn an_explicit_offset_is_honoured_rather_than_reinterpreted() {
    let read = |s: &str| super::drive::instant(s, HOUSE).unwrap().at.to_rfc3339();
    // 15:00 in London is 16:00 in Warsaw, and what was written is what is meant.
    assert_eq!(
        read("2026-07-15T15:00:00+01:00"),
        "2026-07-15T14:00:00+00:00"
    );
    assert_eq!(read("2026-07-15T14:00:00Z"), "2026-07-15T14:00:00+00:00");
    // Not the same instant as the bare wall-clock time, which is the point.
    assert_ne!(read("2026-07-15T15:00:00+01:00"), read("2026-07-15T15:00"));
}

#[test]
fn the_hour_the_clock_skips_is_refused_by_name() {
    // In Warsaw the night of 2026-03-29 has no 02:30: the clock goes straight
    // from 02:00 to 03:00. Booking it would silently become another hour.
    let refused = super::drive::instant("2026-03-29T02:30", HOUSE).unwrap_err();
    let message = refused.message.to_string();
    assert!(message.contains("2026-03-29T02:30"), "{message}");
    assert!(message.contains("Europe/Warsaw"), "{message}");
    assert!(message.contains("02:00"), "{message}");
    assert!(message.contains("03:00"), "{message}");
    assert!(message.contains("2026-03-29"), "{message}");
    // The hour either side of the gap is an ordinary time.
    assert!(super::drive::instant("2026-03-29T01:30", HOUSE).is_ok());
    assert!(super::drive::instant("2026-03-29T03:30", HOUSE).is_ok());
}

#[test]
fn the_hour_the_clock_repeats_takes_the_earlier_of_the_two_and_says_so() {
    // 2026-10-25 02:30 happens twice in Warsaw. The first one — still summer
    // time, +02:00 — is the one taken.
    let twice = super::drive::instant("2026-10-25T02:30", HOUSE).unwrap();
    assert_eq!(twice.at.to_rfc3339(), "2026-10-25T00:30:00+00:00");
    let note = twice.note.expect("the reply says which of the two it took");
    assert!(note.contains("twice"), "{note}");
    assert!(note.contains("2026-10-25T02:30:00+02:00"), "{note}");
}

#[tokio::test]
async fn a_calendar_write_carries_the_person_s_zone_beside_the_time() {
    let db = Db::open_memory().await.unwrap();
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(http_method("POST"))
        .and(path("/calendar/v3/calendars/primary/events"))
        .and(query_param("sendUpdates", "none"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("calendar_event_created.json")),
        )
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

    // "three in the afternoon", with no offset anywhere.
    c.ok(
        "calendar_create_event",
        json!({"account": "work", "title": "Deep work", "start": "2026-09-11T15:00",
               "end": "2026-09-11T16:00", "confirmed": true}),
    )
    .await;

    let asked = server.received_requests().await.unwrap();
    let write = asked
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/events"))
        .expect("the events.insert call");
    let body: Value = serde_json::from_slice(&write.body).unwrap();
    // The pair says what the person meant, so Calendar stores the intent
    // rather than a time converted into somebody else's zone.
    assert_eq!(body["start"]["dateTime"], "2026-09-11T15:00:00+02:00");
    assert_eq!(body["start"]["timeZone"], "Europe/Warsaw");
    assert_eq!(body["end"]["dateTime"], "2026-09-11T16:00:00+02:00");
    assert_eq!(body["end"]["timeZone"], "Europe/Warsaw");
    assert!(!String::from_utf8_lossy(&write.body).contains("attendees"));
    drop(server);
}

/// What a tool publishes and what it accepts must be the same list.
///
/// A client discovers arguments from the schema, so a field the struct takes
/// and the schema omits cannot be used by anyone who trusts the published
/// list — and since arguments are now denied when unknown, a field the schema
/// advertises and the struct rejects would be worse still. This walks every
/// tool and compares the two.
#[tokio::test]
async fn every_tool_publishes_the_arguments_it_accepts() {
    let db = Db::open_memory().await.unwrap();
    let (_, _, secret) = one_of_everything(&db, EVERYTHING, ClientProfile::Generic).await;
    let mut c = Client::new(app(&db, None).await, secret);
    c.initialize().await;
    let tools = c.tools().await;

    let listing = tools
        .iter()
        .find(|t| t["name"] == "docs_insert_code")
        .expect("docs_insert_code");
    let properties = listing["inputSchema"]["properties"].as_object().unwrap();
    assert!(
        properties.contains_key("size_pt"),
        "the size is advertised: {:?}",
        properties.keys().collect::<Vec<_>>()
    );

    // Nothing publishes an empty argument list by accident, which is what a
    // schema generated from the wrong type would look like.
    for tool in &tools {
        let schema = &tool["inputSchema"];
        assert!(
            schema["properties"].is_object(),
            "{} publishes no properties: {schema}",
            tool["name"]
        );
        // Every tool takes an account, except the three that belong to a
        // person rather than to a mailbox or a document.
        let accountless = ["list_accounts", "gmail_upload_link", "docs_upload_link"];
        assert!(
            schema["properties"]["account"].is_object()
                || accountless.contains(&tool["name"].as_str().unwrap()),
            "{} does not publish `account`",
            tool["name"]
        );
    }
}
