//! Every Google call this module makes, against a wiremock server with the
//! hand-trimmed responses under `tests/fixtures/google/`. Nothing here reaches
//! the network, needs a database or reads a file the repository does not
//! carry, and every harness mounts a guard that fails the test if anything
//! ever asks Google to send a message.
//!
//! This file holds the harness — the mock connection store, the mock server
//! and the fixture loader — and the tests for the client itself. One suite
//! per service lives beside it.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::client::{BoxFuture, Client, ConnectionStore, Error, Result};
use crate::config::GoogleConfig;
use crate::domain::scope::Service;
use crate::domain::seal;

const SECRET: &[u8] = b"a development secret of thirty-two bytes or more";
const REFRESH_TOKEN: &str = "1//09exampleRefreshTokenForTests";
/// The connection every call in these tests is made for.
const CONNECTION: i64 = 7;
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/google/");
/// The spreadsheet the Sheets and text-extraction fixtures describe.
const SHEET: &str = "1SpReAdShEeTiDeXaMpLe0123456789abcdefgh";
/// A stand-in endpoint for the tests that exercise the client and not a
/// service. Its shape does not matter; that it is reached does.
const PROBE: &str = "probe/v1/thing";
const PROBE_PATH: &str = "/probe/v1/thing";

fn raw(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}{name}"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn fixture(name: &str) -> Value {
    serde_json::from_str(&raw(name)).unwrap_or_else(|e| panic!("fixture {name} is not JSON: {e}"))
}

/// The `connections` table, as far as the token cache is concerned.
struct TestStore {
    sealed: Vec<u8>,
    reauth: Mutex<Vec<(i64, String)>>,
}

impl TestStore {
    fn new() -> Self {
        Self {
            sealed: seal::seal(SECRET, REFRESH_TOKEN),
            reauth: Mutex::new(Vec::new()),
        }
    }

    fn marked(&self) -> Vec<(i64, String)> {
        self.reauth.lock().unwrap().clone()
    }
}

impl ConnectionStore for TestStore {
    fn sealed_refresh_token(&self, _connection_id: i64) -> BoxFuture<'_, Result<Vec<u8>>> {
        let sealed = self.sealed.clone();
        Box::pin(async move { Ok(sealed) })
    }

    fn mark_needs_reauth<'a>(&'a self, connection_id: i64, detail: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.reauth
                .lock()
                .unwrap()
                .push((connection_id, detail.to_string()));
        })
    }
}

struct Harness {
    server: MockServer,
    client: Client,
    store: Arc<TestStore>,
}

fn google_config(server: &MockServer) -> GoogleConfig {
    let base: url::Url = server.uri().parse().unwrap();
    GoogleConfig {
        client_id: Some("gmcp-test.apps.googleusercontent.com".into()),
        client_secret: Some("test-client-secret".into()),
        api_base: base.clone(),
        oauth_base: base.clone(),
        accounts_base: base,
    }
}

/// A server with no token endpoint, for the tests that mount their own.
async fn bare() -> Harness {
    let server = MockServer::start().await;
    // The house rule, enforced on every test in this module: no request this
    // code makes may ever reach a send endpoint. Google spells all of them
    // with `send` as a whole path segment (`messages/send`, `drafts/send`),
    // which is what this matches; the word inside a percent-encoded id is a
    // segment of its own and goes nowhere near them.
    Mock::given(path_regex(r"(?i)(^|/)send(/|$)"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .named("nothing is ever sent")
        .mount(&server)
        .await;
    let store = Arc::new(TestStore::new());
    let client =
        Client::from_config(&google_config(&server), SECRET.to_vec(), store.clone()).unwrap();
    Harness {
        server,
        client,
        store,
    }
}

/// The usual case: a token endpoint that hands out an access token.
async fn harness() -> Harness {
    let h = bare().await;
    h.mount_token().await;
    h
}

impl Harness {
    async fn mount_token(&self) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fixture("oauth_refresh.json")))
            .named("token endpoint")
            .mount(&self.server)
            .await;
    }

    async fn mount_json(&self, http_method: &str, at: &str, body: Value) {
        Mock::given(method(http_method))
            .and(path(at.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// Every request the server saw, oldest first.
    async fn requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }

    /// The last request to a path, for asserting on what was sent.
    async fn last(&self, http_method: &str, at: &str) -> Request {
        self.requests()
            .await
            .into_iter()
            .rfind(|r| r.method.as_str() == http_method && r.url.path() == at)
            .unwrap_or_else(|| panic!("nothing was sent to {http_method} {at}"))
    }

    async fn last_body(&self, http_method: &str, at: &str) -> Value {
        let request = self.last(http_method, at).await;
        serde_json::from_slice(&request.body).expect("the request body is JSON")
    }

    /// A plain GET through the client. The tests below are about bearer
    /// injection, refresh and error mapping, so they go through no service.
    async fn probe(&self) -> Result<Value> {
        let request = self.client.get(PROBE)?;
        self.client.json(CONNECTION, request).await
    }

    /// How many times the token endpoint was asked for a fresh token.
    async fn refreshes(&self) -> usize {
        self.requests()
            .await
            .iter()
            .filter(|r| r.url.path() == "/token")
            .count()
    }
}

// One suite per concern, named for what it exercises rather than for the
// module it exercises, so the names do not collide with the modules
// themselves.
mod connect;
mod documents;
mod events;
mod extraction;
mod files;
mod mail;
mod spreadsheets;

// ---------------------------------------------------------------------------
// The client: bearer, refresh, retry and error mapping.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_call_carries_the_connection_bearer_token() {
    let h = harness().await;
    h.mount_json("GET", PROBE_PATH, json!({"ok": true})).await;
    h.probe().await.unwrap();

    let request = h.last("GET", PROBE_PATH).await;
    assert_eq!(
        request.headers["authorization"],
        "Bearer ya29.a0AfB_refreshedAccessTokenForTests"
    );
    // The refresh token itself never leaves the token endpoint.
    let token_request = h.last("POST", "/token").await;
    let form = String::from_utf8_lossy(&token_request.body).into_owned();
    assert!(form.contains("grant_type=refresh_token"), "{form}");
    assert!(form.contains("refresh_token=1%2F%2F09example"), "{form}");
}

#[tokio::test]
async fn a_cached_token_is_reused_across_calls() {
    let h = harness().await;
    h.mount_json("GET", PROBE_PATH, json!({"ok": true})).await;
    for _ in 0..3 {
        h.probe().await.unwrap();
    }
    assert_eq!(h.refreshes().await, 1);
}

#[tokio::test]
async fn a_401_refreshes_once_and_retries_once() {
    let h = harness().await;
    // The first read is refused; the second, with the fresh token, works.
    Mock::given(method("GET"))
        .and(path(PROBE_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"code": 401, "message": "Invalid Credentials", "status": "UNAUTHENTICATED"}
        })))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    h.mount_json("GET", PROBE_PATH, json!({"ok": true})).await;

    assert_eq!(h.probe().await.unwrap(), json!({"ok": true}));

    // One token at the start, one after the 401, and no more.
    assert_eq!(h.refreshes().await, 2);
    let reads = h
        .requests()
        .await
        .iter()
        .filter(|r| r.url.path() == PROBE_PATH)
        .count();
    assert_eq!(reads, 2);
    assert!(h.store.marked().is_empty());
}

#[tokio::test]
async fn a_second_401_is_the_callers_error() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(PROBE_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"code": 401, "message": "Invalid Credentials"}
        })))
        .mount(&h.server)
        .await;

    let error = h.probe().await.unwrap_err();
    match error {
        Error::Google(google) => {
            assert_eq!(google.status, 401);
            assert_eq!(google.message, "Invalid Credentials");
        }
        other => panic!("expected a Google error, got {other:?}"),
    }
    assert_eq!(h.refreshes().await, 2);
}

#[tokio::test]
async fn invalid_grant_on_refresh_asks_for_a_reconnection() {
    let h = bare().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(fixture("oauth_invalid_grant.json")))
        .mount(&h.server)
        .await;

    let error = h.probe().await.unwrap_err();
    match error {
        Error::NeedsReauth {
            connection_id,
            detail,
        } => {
            assert_eq!(connection_id, CONNECTION);
            assert!(detail.contains("invalid_grant"), "{detail}");
            assert!(detail.contains("expired or revoked"), "{detail}");
        }
        other => panic!("expected NeedsReauth, got {other:?}"),
    }
    // The connection was marked, which is what puts the amber card on the
    // home page and the reconnect message in the tool error.
    let marked = h.store.marked();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].0, CONNECTION);
    assert!(marked[0].1.contains("invalid_grant"));
    // Nothing was attempted against the API without a token.
    assert!(h.requests().await.iter().all(|r| r.url.path() == "/token"));
}

#[tokio::test]
async fn a_google_error_keeps_its_status_and_message() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(PROBE_PATH))
        .respond_with(ResponseTemplate::new(404).set_body_json(fixture("error_not_found.json")))
        .mount(&h.server)
        .await;

    let error = h.probe().await.unwrap_err();
    match error {
        Error::Google(google) => {
            assert_eq!(google.status, 404);
            assert_eq!(google.message, "Requested entity was not found.");
            assert_eq!(
                google.to_string(),
                "google returned 404: Requested entity was not found."
            );
        }
        other => panic!("expected a Google error, got {other:?}"),
    }
}

#[test]
fn an_unconfigured_google_client_says_so_rather_than_failing_later() {
    let unconfigured = GoogleConfig {
        client_id: None,
        client_secret: None,
        api_base: "https://www.googleapis.com".parse().unwrap(),
        oauth_base: "https://oauth2.googleapis.com".parse().unwrap(),
        accounts_base: "https://accounts.google.com".parse().unwrap(),
    };
    let store = Arc::new(TestStore::new());
    let error = match Client::from_config(&unconfigured, SECRET.to_vec(), store) {
        Err(error) => error,
        Ok(_) => panic!("an unconfigured client was built"),
    };
    assert!(matches!(error, Error::NotConfigured(_)), "{error:?}");
    assert!(error.to_string().contains("GMCP_GOOGLE_CLIENT_ID"));
}

#[tokio::test]
async fn a_sealed_token_from_another_secret_cannot_be_opened() {
    let server = MockServer::start().await;
    struct Wrong;
    impl ConnectionStore for Wrong {
        fn sealed_refresh_token(&self, _: i64) -> BoxFuture<'_, Result<Vec<u8>>> {
            Box::pin(async { Ok(seal::seal(b"a different secret, also long enough!!", "x")) })
        }
        fn mark_needs_reauth<'a>(&'a self, _: i64, _: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }
    let client =
        Client::from_config(&google_config(&server), SECRET.to_vec(), Arc::new(Wrong)).unwrap();
    let request = client.get(PROBE).unwrap();
    let error = client.json::<Value>(CONNECTION, request).await.unwrap_err();
    assert!(matches!(error, Error::Connection(_)), "{error:?}");
    assert!(error.to_string().contains("GMCP_SECRET"));
}

#[tokio::test]
async fn a_service_with_its_own_host_still_answers_on_the_configured_base() {
    // Docs, Sheets and Gmail live on their own Google hosts. A base that is
    // not a Google host — a test's wiremock — is left exactly as configured,
    // so one mock server carries every service.
    let h = harness().await;
    h.mount_json("GET", PROBE_PATH, json!({"ok": true})).await;
    let request = h.client.service("docs").get(PROBE).unwrap();
    let answer: Value = h.client.json(CONNECTION, request).await.unwrap();
    assert_eq!(answer, json!({"ok": true}));

    // And a real base does get the service's own host.
    let real = Client::from_config(
        &GoogleConfig {
            client_id: Some("id".into()),
            client_secret: Some("secret".into()),
            api_base: "https://www.googleapis.com".parse().unwrap(),
            oauth_base: "https://oauth2.googleapis.com".parse().unwrap(),
            accounts_base: "https://accounts.google.com".parse().unwrap(),
        },
        SECRET.to_vec(),
        Arc::new(TestStore::new()),
    )
    .unwrap();
    assert_eq!(
        real.service("sheets")
            .url("v4/spreadsheets/x")
            .unwrap()
            .as_str(),
        "https://sheets.googleapis.com/v4/spreadsheets/x"
    );
    assert_eq!(
        real.url("drive/v3/files").unwrap().as_str(),
        "https://www.googleapis.com/drive/v3/files"
    );
}
