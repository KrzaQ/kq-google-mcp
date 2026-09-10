//! What the API sends and accepts. Row types stay in `db`; these are their
//! public shape, which is where the sealed refresh token and the token hash
//! stop existing as far as the outside world is concerned.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::db::{ApiToken, AuditEntry, Connection, User};
use crate::domain::scope::{Service, google_scopes_for};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UserDto {
    pub id: i64,
    pub email: Option<String>,
    pub name: Option<String>,
    /// The IANA zone this person's times are shown in, in the portal and in
    /// every MCP tool.
    pub timezone: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl From<User> for UserDto {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            email: u.email,
            name: u.name,
            timezone: u.timezone,
            created_at: u.created_at,
            last_login_at: u.last_login_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionDto {
    pub id: i64,
    pub label: String,
    pub google_email: String,
    pub services: Vec<String>,
    /// What Google actually granted, which the consent screen lets a person
    /// narrow.
    pub granted_scopes: Vec<String>,
    /// True when the grant is narrower than the ticked services need: the UI
    /// shows such a connection as partial and offers a reconnect.
    pub partial: bool,
    /// "ok", "needs_reauth" or "revoked"
    pub status: String,
    pub status_detail: Option<String>,
    pub delegate_ok: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl From<Connection> for ConnectionDto {
    fn from(c: Connection) -> Self {
        let services: Vec<Service> = c.services.iter().filter_map(|s| s.parse().ok()).collect();
        let partial = google_scopes_for(&services)
            .into_iter()
            .any(|wanted| !c.granted_scopes.iter().any(|got| got == wanted));
        Self {
            id: c.id,
            label: c.label,
            google_email: c.google_email,
            services: c.services,
            granted_scopes: c.granted_scopes,
            partial,
            status: c.status.to_string(),
            status_detail: c.status_detail,
            delegate_ok: c.delegate_ok,
            created_at: c.created_at,
            last_used_at: c.last_used_at,
        }
    }
}

/// `POST /api/connections/start`: a label and the services to ask Google for.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ConnectInput {
    pub label: String,
    /// Registry service names; `drive` is added when `docs` or `sheets` is
    /// ticked, because both read through Drive.
    pub services: Vec<String>,
}

/// `POST /api/connections/{id}/reconnect`: the same account, optionally with a
/// different set of services.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct ReconnectInput {
    pub services: Option<Vec<String>>,
}

/// Where the browser is sent to consent.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsentUrl {
    pub url: String,
}

/// `PATCH /api/connections/{id}`. Everything else about a connection comes
/// from Google.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct ConnectionPatchInput {
    pub label: Option<String>,
    /// Whether a delegate token may reach this connection.
    pub delegate_ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TokenDto {
    pub id: i64,
    pub name: String,
    pub scopes: Vec<String>,
    /// "generic", "openwebui", "claude-code" or "opencode"
    pub client: String,
    /// The person the token acts as; null for a delegate token.
    pub user_id: Option<i64>,
    pub all_connections: bool,
    /// The allowlist, when there is one.
    pub connection_ids: Vec<i64>,
    pub delegate: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl TokenDto {
    pub fn from(t: ApiToken, connection_ids: Vec<i64>) -> Self {
        Self {
            id: t.id,
            name: t.name,
            scopes: t.scopes,
            client: t.client.to_string(),
            delegate: t.user_id.is_none(),
            user_id: t.user_id,
            all_connections: t.all_connections,
            connection_ids,
            created_at: t.created_at,
            last_used_at: t.last_used_at,
            revoked_at: t.revoked_at,
        }
    }
}

/// `POST /api/tokens`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TokenInput {
    pub name: String,
    /// `service:level` strings, plus `delegate` for a gateway token.
    pub scopes: Vec<String>,
    /// The client profile; "generic" when absent.
    pub client: Option<String>,
    /// Every current and future connection of the person. Personal tokens only.
    #[serde(default)]
    pub all_connections: bool,
    /// The allowlist, when `all_connections` is not set. Personal tokens only.
    #[serde(default)]
    pub connection_ids: Vec<i64>,
}

/// The one time the secret is ever shown.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TokenCreated {
    #[serde(flatten)]
    pub token: TokenDto,
    /// Store it now; the server keeps only its hash.
    pub secret: String,
}

/// One row of the log, as the activity page reads it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditDto {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub user_id: Option<i64>,
    pub token_id: Option<i64>,
    pub connection_id: Option<i64>,
    pub tool: Option<String>,
    pub args: Option<serde_json::Value>,
    pub outcome: String,
    pub detail: Option<String>,
    pub duration_ms: Option<i64>,
    pub ip: Option<String>,
}

impl From<AuditEntry> for AuditDto {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id,
            at: e.at,
            kind: e.kind.to_string(),
            user_id: e.user_id,
            token_id: e.token_id,
            connection_id: e.connection_id,
            tool: e.tool,
            args: e.args,
            outcome: e.outcome.to_string(),
            detail: e.detail,
            duration_ms: e.duration_ms,
            ip: e.ip,
        }
    }
}

/// A page of the log and where the next one continues.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuditPage {
    pub entries: Vec<AuditDto>,
    /// The `before` value that continues this page, or null at the end.
    pub next: Option<String>,
}

/// `/api/scopes`: the registry the token grid is drawn from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScopeRegistry {
    pub services: Vec<ServiceDto>,
    /// The one non-matrix scope.
    pub delegate: ScopeDto,
    /// The client profiles a token may carry.
    pub clients: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServiceDto {
    pub service: String,
    /// What connecting this service asks Google for.
    pub google_scopes: Vec<String>,
    pub levels: Vec<ScopeDto>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScopeDto {
    /// The string a token stores, e.g. `docs:write`.
    pub scope: String,
    /// The level on its own, e.g. `write`; null for `delegate`.
    pub level: Option<String>,
    /// The scope this one is useless without, which the UI ticks along.
    pub requires: Option<String>,
    /// The MCP tools this scope unlocks.
    pub tools: Vec<String>,
}
