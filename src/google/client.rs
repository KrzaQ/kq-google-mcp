//! The one HTTP client every Google call goes through: bearer injection, the
//! access-token cache, error mapping and the single refresh-and-retry on 401.
//!
//! The three base URLs come from [`crate::config::GoogleConfig`] so a test can
//! point them at wiremock; nothing below ever spells out a Google hostname.
//!
//! The token cache does not know about the database. It reaches the sealed
//! refresh token, and reports a dead one, through [`ConnectionStore`], which
//! the HTTP layer implements over `Db` in step 4. That keeps this module
//! testable with no database at all.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Duration as StdDuration;

use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use futures_util::Stream;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::config::GoogleConfig;
use crate::domain::limits::DOWNLOAD_MAX_BYTES;
use crate::domain::seal;

/// How long before an access token actually expires it is treated as expired.
/// Google's tokens live an hour; a minute of slack costs nothing and keeps a
/// long call from starting with a token that dies halfway through.
const EXPIRY_SKEW_SECONDS: i64 = 60;
/// What a token response without `expires_in` is assumed to be worth.
const DEFAULT_TOKEN_LIFETIME_SECONDS: i64 = 3000;
/// Neither Google nor a wiremock server should ever need longer than this.
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(60);
/// How much of an error response is read before the message is truncated.
const ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

/// Google's own error, as its JSON envelope reports it. This is what a tool
/// error says: the HTTP status and Google's message, passed through.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("google returned {status}: {message}")]
pub struct GoogleError {
    pub status: u16,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Google refused the refresh token. The connection has been marked
    /// `needs_reauth`; the tool tells the person to reconnect in the portal.
    #[error("the Google account of connection {connection_id} must be connected again: {detail}")]
    NeedsReauth { connection_id: i64, detail: String },
    #[error(transparent)]
    Google(#[from] GoogleError),
    /// `GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET` are unset.
    #[error("the Google client is not configured: {0}")]
    NotConfigured(&'static str),
    /// The connection row is gone, or its refresh token cannot be opened
    /// because `GMCP_SECRET` was rotated.
    #[error("{0}")]
    Connection(String),
    #[error("could not reach Google: {0}")]
    Transport(String),
    /// Google answered something this code cannot make sense of.
    #[error("{0}")]
    Malformed(String),
    /// Asked for something the curated surface does not do.
    #[error("{0}")]
    Unsupported(String),
    #[error("poppler's pdftotext is not installed, so PDF text cannot be extracted here")]
    PdftotextMissing,
    #[error("the file is larger than the {} MB download cap", DOWNLOAD_MAX_BYTES / (1024 * 1024))]
    TooLarge,
    /// A path that would not stay inside the endpoint it was built for. Ids
    /// come from a model, so this is a refusal and not a panic.
    #[error("{0}")]
    Path(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e.to_string())
    }
}

/// A boxed future, because [`ConnectionStore`] is used behind `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The two things the token cache needs from the `connections` table. The
/// HTTP layer implements this over `Db` in step 4; the tests here implement it
/// over a struct, which is why `google/` has no database dependency.
pub trait ConnectionStore: Send + Sync + 'static {
    /// The `refresh_token_sealed` blob of a connection, exactly as stored.
    fn sealed_refresh_token(&self, connection_id: i64) -> BoxFuture<'_, Result<Vec<u8>>>;

    /// Record that Google refused the refresh token: `status = needs_reauth`
    /// and `status_detail = detail`. Best effort — the caller is already on
    /// its way to an error and a failure to write must not mask it.
    fn mark_needs_reauth<'a>(&'a self, connection_id: i64, detail: &'a str) -> BoxFuture<'a, ()>;
}

/// The shared reqwest client. Redirects are off: an access token must never
/// follow one to somewhere that is not Google.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(Into::into)
}

#[derive(Debug, Clone)]
struct Cached {
    access_token: String,
    expires_at: DateTime<Utc>,
}

/// Access tokens, cached in memory per connection and refreshed on demand.
///
/// The lock is per connection so two calls on different accounts never wait
/// for each other: a `std::sync::Mutex` guards the map — it is held only long
/// enough to clone one `Arc` and never across an await — and each entry is a
/// `tokio::sync::Mutex` that is held across the refresh, so a burst of calls
/// on one connection produces exactly one token request. `DashMap` would buy
/// nothing here: the map is a handful of entries and the contended lock is the
/// inner one.
pub struct TokenSource {
    http: reqwest::Client,
    token_url: Url,
    client_id: String,
    client_secret: String,
    /// `GMCP_SECRET`, the key material the refresh token is sealed under.
    secret: Vec<u8>,
    store: Arc<dyn ConnectionStore>,
    entries: SyncMutex<HashMap<i64, Arc<tokio::sync::Mutex<Option<Cached>>>>>,
}

impl TokenSource {
    /// Fails when the Google client is unconfigured: without credentials no
    /// token can be refreshed, and saying so here is better than a 401 later.
    pub fn new(
        http: reqwest::Client,
        google: &GoogleConfig,
        secret: Vec<u8>,
        store: Arc<dyn ConnectionStore>,
    ) -> Result<Self> {
        let (client_id, client_secret) = match (&google.client_id, &google.client_secret) {
            (Some(id), Some(secret)) => (id.clone(), secret.clone()),
            _ => {
                return Err(Error::NotConfigured(
                    "GMCP_GOOGLE_CLIENT_ID and GMCP_GOOGLE_CLIENT_SECRET are unset",
                ));
            }
        };
        Ok(Self {
            http,
            token_url: join(&google.oauth_base, "token")?,
            client_id,
            client_secret,
            secret,
            store,
            entries: SyncMutex::new(HashMap::new()),
        })
    }

    fn entry(&self, connection_id: i64) -> Arc<tokio::sync::Mutex<Option<Cached>>> {
        self.entries
            .lock()
            .expect("the token cache lock is never held across a panic")
            .entry(connection_id)
            .or_default()
            .clone()
    }

    /// The cached token, refreshed when it is missing or about to expire.
    pub async fn access_token(&self, connection_id: i64) -> Result<String> {
        let entry = self.entry(connection_id);
        let mut cached = entry.lock().await;
        if let Some(current) = cached.as_ref()
            && current.expires_at > Utc::now() + Duration::seconds(EXPIRY_SKEW_SECONDS)
        {
            return Ok(current.access_token.clone());
        }
        let fresh = self.refresh(connection_id).await?;
        let token = fresh.access_token.clone();
        *cached = Some(fresh);
        Ok(token)
    }

    /// What a 401 asks for: one refresh, and only when the token that was
    /// refused is still the cached one. Two calls that raced into the same 401
    /// therefore share a single refresh instead of asking for two.
    pub async fn refresh_after_401(&self, connection_id: i64, stale: &str) -> Result<String> {
        let entry = self.entry(connection_id);
        let mut cached = entry.lock().await;
        if let Some(current) = cached.as_ref()
            && current.access_token != stale
        {
            return Ok(current.access_token.clone());
        }
        let fresh = self.refresh(connection_id).await?;
        let token = fresh.access_token.clone();
        *cached = Some(fresh);
        Ok(token)
    }

    /// Forget a connection's token, for when it is disconnected or resealed.
    pub fn forget(&self, connection_id: i64) {
        self.entries
            .lock()
            .expect("the token cache lock is never held across a panic")
            .remove(&connection_id);
    }

    /// One token request against `{oauth_base}/token`. Called with the
    /// connection's lock held.
    async fn refresh(&self, connection_id: i64) -> Result<Cached> {
        let sealed = self.store.sealed_refresh_token(connection_id).await?;
        let refresh_token = seal::open(&self.secret, &sealed)
            .map_err(|e| Error::Connection(format!("connection {connection_id}: {e}")))?;
        let response = self
            .http
            .post(self.token_url.clone())
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            let (code, message) = oauth_error(&body);
            if code.as_deref() == Some("invalid_grant") {
                let detail = message.clone();
                self.store.mark_needs_reauth(connection_id, &detail).await;
                return Err(Error::NeedsReauth {
                    connection_id,
                    detail,
                });
            }
            return Err(GoogleError {
                status: status.as_u16(),
                message,
            }
            .into());
        }
        let token: RefreshResponse = serde_json::from_str(&body).map_err(|e| {
            Error::Malformed(format!("the token response is not what it should be: {e}"))
        })?;
        Ok(Cached {
            expires_at: Utc::now()
                + Duration::seconds(token.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS)),
            access_token: token.access_token,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    expires_in: Option<i64>,
}

/// The Google REST client. Cheap to clone; the token cache is shared.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_base: Url,
    tokens: Arc<TokenSource>,
}

impl Client {
    pub fn new(http: reqwest::Client, api_base: Url, tokens: Arc<TokenSource>) -> Self {
        Self {
            http,
            api_base,
            tokens,
        }
    }

    /// Everything a `serve` process needs, from the parsed configuration.
    pub fn from_config(
        google: &GoogleConfig,
        secret: Vec<u8>,
        store: Arc<dyn ConnectionStore>,
    ) -> Result<Self> {
        let http = http_client()?;
        let tokens = TokenSource::new(http.clone(), google, secret, store)?;
        Ok(Self::new(http, google.api_base.clone(), Arc::new(tokens)))
    }

    pub fn tokens(&self) -> &Arc<TokenSource> {
        &self.tokens
    }

    /// An absolute URL for an API path, against the configured base.
    pub fn url(&self, path: &str) -> Result<Url> {
        join(&self.api_base, path)
    }

    /// The same base with its host swapped for `<subdomain>.googleapis.com`.
    ///
    /// Drive, Calendar and userinfo are served from `www.googleapis.com`, but
    /// Docs, Sheets and Gmail have their own hosts and answer nothing on the
    /// shared one. The rule is stated once, here: a base that is a Google host
    /// gets the service's own subdomain, and a base that is anything else — a
    /// test's wiremock — is left exactly as configured, so one mock server
    /// still carries every service.
    pub fn service(&self, subdomain: &str) -> ServiceClient<'_> {
        let mut base = self.api_base.clone();
        if base
            .host_str()
            .is_some_and(|host| host.ends_with("googleapis.com"))
        {
            base.set_host(Some(&format!("{subdomain}.googleapis.com")))
                .expect("a googleapis.com subdomain is a valid host");
        }
        ServiceClient {
            http: &self.http,
            base,
        }
    }

    pub fn get(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.get(self.url(path)?))
    }

    pub fn post(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.post(self.url(path)?))
    }

    pub fn patch(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.patch(self.url(path)?))
    }

    pub fn put(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.put(self.url(path)?))
    }

    pub fn delete(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.delete(self.url(path)?))
    }

    /// Attach the connection's access token, send, and on a 401 refresh once
    /// and send again. Exactly one retry: a second 401 is the caller's error.
    pub async fn send(&self, connection_id: i64, request: RequestBuilder) -> Result<Response> {
        let retry = request.try_clone().ok_or_else(|| {
            Error::Malformed("a Google request with a streaming body cannot be retried".into())
        })?;
        let token = self.tokens.access_token(connection_id).await?;
        let first = request.bearer_auth(&token).send().await?;
        if first.status() != StatusCode::UNAUTHORIZED {
            return check(first).await;
        }
        let fresh = self.tokens.refresh_after_401(connection_id, &token).await?;
        check(retry.bearer_auth(fresh).send().await?).await
    }

    /// Send and decode the JSON body, which is capped like every other body
    /// this client reads. Gmail hands attachments back base64 inside JSON, so
    /// the download cap has to apply here too or a 200 MB attachment arrives
    /// as a 270 MB string in memory.
    pub async fn json<T: serde::de::DeserializeOwned>(
        &self,
        connection_id: i64,
        request: RequestBuilder,
    ) -> Result<T> {
        let body = self
            .download(connection_id, request)
            .await?
            .collect()
            .await?;
        serde_json::from_slice(&body)
            .map_err(|e| Error::Malformed(format!("google answered something unexpected: {e}")))
    }

    /// Send and discard the body, for the calls whose answer says nothing.
    pub async fn drain(&self, connection_id: i64, request: RequestBuilder) -> Result<()> {
        self.send(connection_id, request).await?;
        Ok(())
    }

    /// Send and read the whole body, refusing anything over the download cap.
    pub async fn bytes(&self, connection_id: i64, request: RequestBuilder) -> Result<Vec<u8>> {
        self.download(connection_id, request).await?.collect().await
    }

    /// Send and hand back the response for streaming, with what the headers
    /// said about it. Used by the download route, which must never hold a
    /// 50 MB file in memory.
    pub async fn download(&self, connection_id: i64, request: RequestBuilder) -> Result<Download> {
        let response = self.send(connection_id, request).await?;
        Ok(Download::new(response))
    }
}

/// One service's endpoints, on whichever host serves them. Requests built
/// here are sent through the [`Client`] that made it, so they get the same
/// bearer injection and the same refresh-and-retry.
pub struct ServiceClient<'a> {
    http: &'a reqwest::Client,
    base: Url,
}

impl ServiceClient<'_> {
    pub fn url(&self, path: &str) -> Result<Url> {
        join(&self.base, path)
    }

    pub fn get(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.get(self.url(path)?))
    }

    pub fn post(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.post(self.url(path)?))
    }

    pub fn patch(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.patch(self.url(path)?))
    }

    pub fn put(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.put(self.url(path)?))
    }

    pub fn delete(&self, path: &str) -> Result<RequestBuilder> {
        Ok(self.http.delete(self.url(path)?))
    }
}

/// A response being read as bytes, capped at [`DOWNLOAD_MAX_BYTES`].
pub struct Download {
    mime_type: Option<String>,
    size: Option<u64>,
    response: Response,
}

impl Download {
    fn new(response: Response) -> Self {
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Self {
            mime_type: header(reqwest::header::CONTENT_TYPE)
                .map(|v| v.split(';').next().unwrap_or_default().trim().to_string()),
            size: response.content_length(),
            response,
        }
    }

    /// What Google said the bytes are, without its charset parameter.
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    /// `Content-Length`, when Google sent one.
    pub fn size(&self) -> Option<u64> {
        self.size
    }

    /// The whole body, refused before it is buffered when it is too big.
    pub async fn collect(mut self) -> Result<Vec<u8>> {
        if self.size.is_some_and(|n| n > DOWNLOAD_MAX_BYTES) {
            return Err(Error::TooLarge);
        }
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = self.response.chunk().await? {
            if out.len() as u64 + chunk.len() as u64 > DOWNLOAD_MAX_BYTES {
                return Err(Error::TooLarge);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// The body as a stream of chunks, so `/dl/{id}` can pass it straight to
    /// the client. The cap is enforced as the bytes go past: a response that
    /// lied about its length ends as an error mid-stream rather than filling
    /// the disk of whoever clicked the link.
    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes>> {
        futures_util::stream::try_unfold((self.response, 0u64), |(mut response, seen)| async move {
            match response.chunk().await? {
                Some(chunk) => {
                    let seen = seen + chunk.len() as u64;
                    if seen > DOWNLOAD_MAX_BYTES {
                        return Err(Error::TooLarge);
                    }
                    Ok(Some((chunk, (response, seen))))
                }
                None => Ok(None),
            }
        })
    }
}

/// `base` and a path, with exactly one slash between them — and the path that
/// comes out is exactly the path that went in.
///
/// The literal is written in this crate; the ids inside it come from a model.
/// `Url` removes dot segments and treats `?` and `#` as delimiters, so an id
/// like `x/../../drafts/send` or `../trash?` would silently move a call to an
/// endpoint this server never makes. Every id is [`urlencode`]d before it is
/// formatted in, which is what keeps that from happening; the check here is
/// the second lock, and it refuses rather than sends.
fn join(base: &Url, path: &str) -> Result<Url> {
    let path = path.trim_start_matches('/');
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }
    let expected = format!("{}{path}", base.path());
    let joined = base
        .join(path)
        .map_err(|e| Error::Path(format!("{path} is not a path this server builds: {e}")))?;
    if joined.path() != expected || joined.query().is_some() || joined.fragment().is_some() {
        return Err(Error::Path(format!(
            "{path} would leave {expected}, so the call was not made"
        )));
    }
    Ok(joined)
}

/// Percent-encode everything outside RFC 3986's unreserved set, so an id can
/// carry any byte at all and still be exactly one path segment. Every id
/// interpolated into a Google URL goes through this: the ids are what a model
/// hands over, and a `/`, `..`, `?` or `#` in one of them would otherwise
/// point the call somewhere else entirely.
pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A successful response passes through; anything else becomes the
/// [`GoogleError`] the tool reports, with Google's own message when it sent
/// one and the status line when it did not.
async fn check(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = error_body(response).await;
    Err(GoogleError {
        status: status.as_u16(),
        message: api_error(&body).unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("unknown error")
                .to_string()
        }),
    }
    .into())
}

/// As much of a failed response as an error message can possibly need. The
/// body of an error is read to be quoted at a person, so it is read with a cap
/// of its own rather than with the download cap.
async fn error_body(mut response: Response) -> String {
    let mut out: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        out.extend_from_slice(&chunk);
        if out.len() >= ERROR_BODY_MAX_BYTES {
            out.truncate(ERROR_BODY_MAX_BYTES);
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Google's REST envelope: `{"error": {"code": 404, "message": "…"}}`, with
/// the OAuth endpoints' `{"error": "…", "error_description": "…"}` as the
/// other shape the same field takes.
fn api_error(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
        return Some(message.to_string());
    }
    let (_, message) = split_oauth_error(&value)?;
    Some(message)
}

/// The OAuth endpoints' error shape, as `(code, message)`. The code is what
/// tells `invalid_grant` — a dead refresh token — from everything else.
fn oauth_error(body: &str) -> (Option<String>, String) {
    let fallback = || {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            "google refused the refresh token without saying why".to_string()
        } else {
            trimmed.chars().take(300).collect()
        }
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, fallback());
    };
    match split_oauth_error(&value) {
        Some((code, message)) => (Some(code), message),
        None => (None, api_error(body).unwrap_or_else(fallback)),
    }
}

fn split_oauth_error(value: &serde_json::Value) -> Option<(String, String)> {
    let code = value.get("error")?.as_str()?.to_string();
    let message = match value.get("error_description").and_then(|d| d.as_str()) {
        Some(description) => format!("{code}: {description}"),
        None => code.clone(),
    };
    Some((code, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        "https://gmail.googleapis.com".parse().unwrap()
    }

    #[test]
    fn an_encoded_id_is_one_path_segment_whatever_is_in_it() {
        for id in [
            "x/../../drafts/send",
            "../trash?",
            "18f#fragment",
            "a/b",
            "zażółć",
        ] {
            let path = format!("gmail/v1/users/me/messages/{}/modify", urlencode(id));
            let url = join(&base(), &path).expect("an encoded id joins");
            let segments: Vec<&str> = url.path_segments().unwrap().collect();
            assert_eq!(
                segments,
                [
                    "gmail",
                    "v1",
                    "users",
                    "me",
                    "messages",
                    &urlencode(id),
                    "modify"
                ],
                "{url}"
            );
            assert_eq!(url.query(), None, "{url}");
            assert_eq!(url.fragment(), None, "{url}");
        }
        // Unreserved characters are left alone, so an ordinary id reads as
        // itself in the log and in a mock's path matcher.
        assert_eq!(urlencode("18f0a1b2c3d4e5f6-_.~"), "18f0a1b2c3d4e5f6-_.~");
    }

    #[test]
    fn a_raw_id_that_would_leave_the_endpoint_is_refused_rather_than_sent() {
        for path in [
            "gmail/v1/users/me/messages/x/../../drafts/send/modify",
            "gmail/v1/users/me/messages/../trash?/modify",
            "gmail/v1/users/me/messages/18f#x/modify",
            "gmail/v1/users/me/messages/./modify",
        ] {
            let error = join(&base(), path).expect_err(path);
            assert!(matches!(error, Error::Path(_)), "{path}: {error:?}");
        }
    }
}
