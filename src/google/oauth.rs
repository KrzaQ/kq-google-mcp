//! The connect flow: the consent URL, the code exchange and the userinfo
//! lookup that tells the callback which Google account was just connected.
//!
//! What Google is asked for is fixed by the plan and not by the caller:
//! `access_type=offline` so a refresh token comes back at all,
//! `include_granted_scopes=true` so a second connection of the same account
//! keeps what the first one was granted, PKCE always, and
//! `prompt=consent select_account` on a first connect — the `consent` half is
//! what makes Google issue a refresh token again, the `select_account` half is
//! what lets a person pick a second account. A reconnect drops
//! `select_account` and passes `login_hint` instead, because a reconnect must
//! land on the account it already has.
//!
//! [`FlowState`] is what the callback needs to remember in between. It is
//! serialisable and nothing more: the private cookie that carries it is the
//! HTTP layer's business (step 4).

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope as OauthScope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use url::Url;

use super::client::{Error, GoogleError, Result};
use crate::config::GoogleConfig;
use crate::domain::scope::{Service, google_scopes_for};

/// Where the browser comes back to. Derived from `GMCP_PUBLIC_URL` and never
/// from a request header, so a forwarded `Host` cannot move it.
pub const CALLBACK_PATH: &str = "/api/google/callback";

/// The authorization endpoint, under `GMCP_GOOGLE_ACCOUNTS_BASE`.
const AUTHORIZE_PATH: &str = "o/oauth2/v2/auth";
/// The token and revocation endpoints, under `GMCP_GOOGLE_OAUTH_BASE`.
const TOKEN_PATH: &str = "token";
const REVOKE_PATH: &str = "revoke";
/// Userinfo, under `GMCP_GOOGLE_API_BASE`.
const USERINFO_PATH: &str = "oauth2/v3/userinfo";

type ConnectClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// What the login handler puts in the private cookie and the callback reads
/// back. Everything the callback needs and nothing that identifies the person:
/// the session cookie beside it already does that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowState {
    /// The `state` parameter, echoed by Google and compared here.
    pub csrf: String,
    pub pkce_verifier: String,
    /// What the person ticked, in the shape the scope registry uses.
    pub services: Vec<Service>,
    /// The connection being reconnected, if this is not a first connect.
    pub reconnect: Option<i64>,
    /// The label the new connection gets, or the one the old one keeps.
    pub label: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlowError {
    #[error("the consent flow has no state; start again from the connections page")]
    Missing,
    #[error("the consent flow came back with a state this server did not start")]
    Mismatch,
    #[error("google reported {0}")]
    Refused(String),
}

impl FlowState {
    /// The callback's first check: Google must echo the `state` this server
    /// generated, or the code is not one this browser asked for.
    pub fn check(&self, returned_state: &str) -> std::result::Result<(), FlowError> {
        if returned_state.is_empty() {
            return Err(FlowError::Missing);
        }
        // Both halves are this server's own random token, so a plain
        // comparison leaks nothing an attacker does not already control.
        if returned_state == self.csrf {
            Ok(())
        } else {
            Err(FlowError::Mismatch)
        }
    }
}

/// A consent URL and the state that has to survive until the callback.
#[derive(Debug, Clone)]
pub struct Authorization {
    pub url: Url,
    pub state: FlowState,
}

/// What the code exchange yielded. `refresh_token` is `None` when Google
/// decided not to issue one, which is the failure the callback has to explain
/// rather than store.
#[derive(Debug, Clone)]
pub struct Grant {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// What Google actually granted, which may be narrower than what was
    /// asked for: the consent screen lets people untick.
    pub granted_scopes: Vec<String>,
    pub expires_in: Option<Duration>,
}

/// Who the grant belongs to, from the userinfo endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    /// Google's stable account id.
    pub sub: String,
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: bool,
}

pub struct OAuth {
    client: ConnectClient,
    http: reqwest::Client,
    userinfo_url: Url,
    revoke_url: Url,
}

impl OAuth {
    /// Fails when the Google client is unconfigured, which is what the
    /// connections page reports instead of offering a Connect button.
    pub fn new(http: reqwest::Client, google: &GoogleConfig, public_url: &Url) -> Result<Self> {
        let (client_id, client_secret) = match (&google.client_id, &google.client_secret) {
            (Some(id), Some(secret)) => (id.clone(), secret.clone()),
            _ => {
                return Err(Error::NotConfigured(
                    "GMCP_GOOGLE_CLIENT_ID and GMCP_GOOGLE_CLIENT_SECRET are unset",
                ));
            }
        };
        let redirect = public_url
            .join(CALLBACK_PATH)
            .map_err(|e| Error::Malformed(format!("GMCP_PUBLIC_URL: {e}")))?;
        let client = BasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_uri(AuthUrl::from_url(url(
                &google.accounts_base,
                AUTHORIZE_PATH,
            )))
            .set_token_uri(TokenUrl::from_url(url(&google.oauth_base, TOKEN_PATH)))
            .set_redirect_uri(RedirectUrl::from_url(redirect));
        Ok(Self {
            client,
            http,
            userinfo_url: url(&google.api_base, USERINFO_PATH),
            revoke_url: url(&google.oauth_base, REVOKE_PATH),
        })
    }

    /// The consent URL for a first connect.
    pub fn authorize(&self, label: &str, services: &[Service]) -> Authorization {
        self.build(label, services, None, None)
    }

    /// The consent URL for a reconnect: the same label, the account named as
    /// a hint, and no `select_account` — picking a different Google account
    /// here is the one thing a reconnect must not do, and the callback refuses
    /// it if it happens anyway.
    pub fn reconnect(
        &self,
        connection_id: i64,
        label: &str,
        google_email: &str,
        services: &[Service],
    ) -> Authorization {
        self.build(label, services, Some(connection_id), Some(google_email))
    }

    fn build(
        &self,
        label: &str,
        services: &[Service],
        reconnect: Option<i64>,
        login_hint: Option<&str>,
    ) -> Authorization {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = self
            .client
            .authorize_url(CsrfToken::new_random)
            .add_scopes(
                google_scopes_for(services)
                    .into_iter()
                    .map(|s| OauthScope::new(s.to_string())),
            )
            .set_pkce_challenge(challenge)
            // Without this Google issues an access token and no refresh
            // token, and the connection would die in an hour.
            .add_extra_param("access_type", "offline")
            // A second connection keeps what an earlier one was granted.
            .add_extra_param("include_granted_scopes", "true")
            .add_extra_param(
                "prompt",
                match reconnect {
                    Some(_) => "consent",
                    None => "consent select_account",
                },
            );
        if let Some(hint) = login_hint {
            request = request.add_extra_param("login_hint", hint.to_string());
        }
        let (url, csrf) = request.url();
        Authorization {
            url,
            state: FlowState {
                csrf: csrf.secret().clone(),
                pkce_verifier: verifier.secret().clone(),
                services: services.to_vec(),
                reconnect,
                label: label.to_string(),
            },
        }
    }

    /// Exchange the code the callback was given. The granted scopes come from
    /// the token response, because they are what gets stored: the consent
    /// screen lets a person untick one and the UI shows such a grant as
    /// partial.
    pub async fn exchange(&self, code: &str, state: &FlowState) -> Result<Grant> {
        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(PkceCodeVerifier::new(state.pkce_verifier.clone()))
            .request_async(&self.http)
            .await
            .map_err(exchange_error)?;
        Ok(Grant {
            access_token: response.access_token().secret().clone(),
            refresh_token: response.refresh_token().map(|t| t.secret().clone()),
            granted_scopes: response
                .scopes()
                .map(|scopes| scopes.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
            expires_in: response.expires_in(),
        })
    }

    /// Which account the grant belongs to. The callback stores the email and
    /// compares it on a reconnect.
    pub async fn userinfo(&self, access_token: &str) -> Result<UserInfo> {
        let response = self
            .http
            .get(self.userinfo_url.clone())
            .bearer_auth(access_token)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(GoogleError {
                status: status.as_u16(),
                message: format!(
                    "userinfo: {}",
                    body.trim().chars().take(300).collect::<String>()
                ),
            }
            .into());
        }
        serde_json::from_str(&body)
            .map_err(|e| Error::Malformed(format!("userinfo answered something unexpected: {e}")))
    }

    /// Best effort, for when a connection is removed: Google forgets the
    /// grant so the account's permissions page stops listing this app. A
    /// failure here is logged and ignored — the row goes away either way.
    pub async fn revoke(&self, refresh_token: &str) -> Result<()> {
        let response = self
            .http
            .post(self.revoke_url.clone())
            .form(&[("token", refresh_token)])
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(GoogleError {
            status: status.as_u16(),
            message: response.text().await.unwrap_or_default(),
        }
        .into())
    }
}

/// oauth2's error type says a lot; a tool error wants Google's own words.
fn exchange_error<E: std::error::Error>(
    error: oauth2::RequestTokenError<
        E,
        oauth2::StandardErrorResponse<oauth2::basic::BasicErrorResponseType>,
    >,
) -> Error {
    use oauth2::RequestTokenError;
    match error {
        RequestTokenError::ServerResponse(response) => Error::Google(GoogleError {
            status: 400,
            message: match response.error_description() {
                Some(description) => format!("{}: {description}", response.error()),
                None => response.error().to_string(),
            },
        }),
        RequestTokenError::Request(e) => Error::Transport(e.to_string()),
        RequestTokenError::Parse(e, _) => {
            Error::Malformed(format!("the token response is not what it should be: {e}"))
        }
        RequestTokenError::Other(e) => Error::Transport(e),
    }
}

fn url(base: &Url, path: &str) -> Url {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        let with_slash = format!("{}/", base.path());
        base.set_path(&with_slash);
    }
    base.join(path).expect("a static OAuth path")
}
