//! One type per table, plus the small enums the CHECK constraints spell out.
//!
//! Instants are `DateTime<Utc>`: sqlx writes them as RFC 3339 TEXT and reads
//! them back the same way, which is what the schema promises. Lists are
//! `Vec<String>` behind `#[sqlx(json)]`, stored as JSON arrays.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// A TEXT enum: the Rust spelling of one of the schema's CHECK constraints.
/// `as_str` is the single source of truth for SQL, serde and the CLI, so the
/// three can never drift apart.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
        pub enum $name {
            $(
                #[serde(rename = $text)]
                #[sqlx(rename = $text)]
                $variant,
            )+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s.trim() {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!(
                        "{other:?} is not one of {}",
                        Self::ALL
                            .iter()
                            .map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
        }
    };
}

text_enum! {
    /// Health of a connection. `revoked` is kept for the log's sake and is
    /// never reachable by a principal.
    ConnectionStatus {
        Ok => "ok",
        NeedsReauth => "needs_reauth",
        Revoked => "revoked",
    }
}

text_enum! {
    /// Which client a token was minted for; it decides how images come back.
    ClientProfile {
        Generic => "generic",
        OpenWebUi => "openwebui",
        ClaudeCode => "claude-code",
        OpenCode => "opencode",
    }
}

text_enum! {
    /// What a download link points at.
    LinkKind {
        GmailAttachment => "gmail_attachment",
        DriveDownload => "drive_download",
        DriveExport => "drive_export",
    }
}

text_enum! {
    /// What happened, for the activity page.
    AuditKind {
        ToolCall => "tool_call",
        LinkCreated => "link_created",
        LinkUsed => "link_used",
        LinkRefused => "link_refused",
        Connect => "connect",
        Reconnect => "reconnect",
        ConnectionRemoved => "connection_removed",
        TokenCreated => "token_created",
        TokenRevoked => "token_revoked",
    }
}

text_enum! {
    /// How it ended.
    AuditOutcome {
        Ok => "ok",
        Error => "error",
        Forbidden => "forbidden",
    }
}

#[derive(Debug, Clone, FromRow, Serialize, PartialEq)]
pub struct User {
    pub id: i64,
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
    /// An IANA name. Every time this person is shown is rendered on this
    /// clock, and every time they type without an offset is read on it.
    pub timezone: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl User {
    /// Name, else email, else the subject: what the UI and the CLI show.
    pub fn display(&self) -> &str {
        self.name
            .as_deref()
            .or(self.email.as_deref())
            .unwrap_or(&self.subject)
    }

    /// The person's clock. A name the tz database no longer knows falls back
    /// to the house zone: a stale row must not fail every call this person
    /// makes.
    pub fn zone(&self, house: Tz) -> Tz {
        self.timezone.parse().unwrap_or(house)
    }
}

#[derive(Debug, Clone, FromRow, Serialize, PartialEq)]
pub struct Connection {
    pub id: i64,
    pub user_id: i64,
    pub label: String,
    pub google_email: String,
    #[sqlx(json)]
    pub services: Vec<String>,
    #[sqlx(json)]
    pub granted_scopes: Vec<String>,
    /// Opaque here: sealing and opening it is `domain::crypto`'s business.
    #[serde(skip)]
    pub refresh_token_sealed: Vec<u8>,
    pub status: ConnectionStatus,
    pub status_detail: Option<String>,
    pub delegate_ok: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// What the Google callback has when it stores a fresh grant.
#[derive(Debug, Clone)]
pub struct NewConnection {
    pub user_id: i64,
    pub label: String,
    pub google_email: String,
    pub services: Vec<String>,
    pub granted_scopes: Vec<String>,
    pub refresh_token_sealed: Vec<u8>,
    pub delegate_ok: bool,
}

/// `None` leaves a column alone. Only these two are editable; everything else
/// about a connection comes from Google.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ConnectionPatch {
    pub label: Option<String>,
    pub delegate_ok: Option<bool>,
}

#[derive(Debug, Clone, FromRow, Serialize, PartialEq)]
pub struct ApiToken {
    pub id: i64,
    pub name: String,
    #[serde(skip)]
    pub token_hash: String,
    #[sqlx(json)]
    pub scopes: Vec<String>,
    pub client: ClientProfile,
    /// The person a personal token acts as; NULL for a delegate token.
    pub user_id: Option<i64>,
    /// Personal token: every current and future connection of its user.
    pub all_connections: bool,
    pub created_by: i64,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl ApiToken {
    pub fn is_delegate(&self) -> bool {
        self.user_id.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct NewToken {
    pub name: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub client: ClientProfile,
    pub user_id: Option<i64>,
    pub all_connections: bool,
    pub created_by: i64,
}

/// Whose connections a principal may see. The three kinds of the plan, in the
/// shape the query needs; `Db` never sees an HTTP principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A browser session: the person's own connections.
    Session { user_id: i64 },
    /// A personal token: its allowlist, or all of its user's connections.
    Token {
        user_id: i64,
        token_id: i64,
        all_connections: bool,
    },
    /// A delegate token acting for a person: their gateway connections.
    Delegate { user_id: i64 },
}

impl Reach {
    /// The reach of a resolved bearer token. `user_id` is the token's own for
    /// a personal token and the one named in `X-Gmcp-User` for a delegate.
    pub fn of_token(token: &ApiToken, user_id: i64) -> Self {
        match token.user_id {
            Some(_) => Self::Token {
                user_id,
                token_id: token.id,
                all_connections: token.all_connections,
            },
            None => Self::Delegate { user_id },
        }
    }

    pub fn user_id(self) -> i64 {
        match self {
            Self::Session { user_id }
            | Self::Token { user_id, .. }
            | Self::Delegate { user_id } => user_id,
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize, PartialEq)]
pub struct Link {
    pub id: String,
    pub connection_id: i64,
    pub token_id: i64,
    pub kind: LinkKind,
    #[sqlx(json)]
    pub target: serde_json::Value,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub uses_left: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewLink {
    pub id: String,
    pub connection_id: i64,
    pub token_id: i64,
    pub kind: LinkKind,
    pub target: serde_json::Value,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<i64>,
    pub expires_at: DateTime<Utc>,
    pub uses_left: i64,
}

/// Why a hit on `/dl/{id}` gets nothing. The route answers 404 to all three
/// without saying which, but the log records the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkRefusal {
    #[error("no such link")]
    Unknown,
    #[error("link has expired")]
    Expired,
    #[error("link has no uses left")]
    Exhausted,
}

#[derive(Debug, Clone, FromRow, Serialize, PartialEq)]
pub struct AuditEntry {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub kind: AuditKind,
    pub user_id: Option<i64>,
    pub token_id: Option<i64>,
    pub connection_id: Option<i64>,
    pub tool: Option<String>,
    #[sqlx(json(nullable))]
    pub args: Option<serde_json::Value>,
    pub outcome: AuditOutcome,
    pub detail: Option<String>,
    pub duration_ms: Option<i64>,
    pub ip: Option<String>,
}

impl AuditEntry {
    /// Where a page that ends with this row continues.
    pub fn cursor(&self) -> AuditCursor {
        AuditCursor {
            at: self.at,
            id: self.id,
        }
    }
}

/// The caller fills in what it knows; `at` is passed in rather than defaulted
/// so a tool logs the instant the call started.
#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    pub at: DateTime<Utc>,
    pub kind: AuditKind,
    pub user_id: Option<i64>,
    pub token_id: Option<i64>,
    pub connection_id: Option<i64>,
    pub tool: Option<String>,
    pub args: Option<serde_json::Value>,
    pub outcome: AuditOutcome,
    pub detail: Option<String>,
    pub duration_ms: Option<i64>,
    pub ip: Option<String>,
}

impl NewAuditEntry {
    /// The common case: everything else is filled in by the caller.
    pub fn new(at: DateTime<Utc>, kind: AuditKind, outcome: AuditOutcome) -> Self {
        Self {
            at,
            kind,
            user_id: None,
            token_id: None,
            connection_id: None,
            tool: None,
            args: None,
            outcome,
            detail: None,
            duration_ms: None,
            ip: None,
        }
    }
}

/// Where the previous audit page ended. The log is ordered `(at, id)`
/// descending, so a page continues strictly before its last row.
///
/// It travels in a query string, so it is text: the instant in RFC 3339, a
/// comma, the id. The instant round-trips exactly, which matters because the
/// comparison happens on the stored TEXT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    pub at: DateTime<Utc>,
    pub id: i64,
}

impl std::fmt::Display for AuditCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{},{}",
            self.at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false),
            self.id
        )
    }
}

impl std::str::FromStr for AuditCursor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (at, id) = s
            .trim()
            .rsplit_once(',')
            .ok_or_else(|| format!("{s:?} is not a cursor: expected <instant>,<id>"))?;
        Ok(Self {
            at: DateTime::parse_from_rfc3339(at)
                .map_err(|e| format!("{at:?} is not an RFC 3339 instant: {e}"))?
                .with_timezone(&Utc),
            id: id
                .parse()
                .map_err(|e| format!("{id:?} is not a row id: {e}"))?,
        })
    }
}

impl Serialize for AuditCursor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AuditCursor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The activity page's filters, all optional and all combined with AND.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct AuditFilter {
    pub user_id: Option<i64>,
    pub token_id: Option<i64>,
    pub connection_id: Option<i64>,
    pub kind: Option<AuditKind>,
    pub tool: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Continue strictly before this row.
    pub before: Option<AuditCursor>,
    /// Rows per page; defaults to 50 and is capped at 500.
    pub limit: Option<i64>,
}

/// What `gmcp prune` threw away.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Pruned {
    pub links: u64,
    pub audit: u64,
}
