//! The scope registry: which capabilities exist, which MCP tools each one
//! unlocks, and which Google scopes a connection asks for. It is a table in
//! code and never a migration; adding a service is a change here plus the
//! tools that go with it.
//!
//! Token scopes are `service:level` strings plus the non-matrix `delegate`.
//! Levels are deliberately not cumulative in storage — `gmail:draft` does not
//! imply `gmail:read` — so that what a token may do is exactly what its row
//! says. [`Scope::requires`] is how the UI ticks the lower level along and
//! how the CLI refuses a write level on its own.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::token::SCOPE_DELEGATE;

/// A Google service the portal can be granted. The order is the order the UI
/// and every canonical scope list uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Service {
    Gmail,
    Drive,
    Docs,
    Sheets,
    Calendar,
}

/// What a token may do with a service. Not every level exists for every
/// service; [`Service::levels`] is the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Read,
    /// Gmail only: create, update and delete drafts. Never send.
    Draft,
    /// Gmail only: labels, archive, read state.
    Modify,
    /// Docs, Sheets and Calendar: create and change content.
    Write,
}

/// The matrix, in canonical order.
const REGISTRY: &[(Service, &[Level])] = &[
    (Service::Gmail, &[Level::Read, Level::Draft, Level::Modify]),
    (Service::Drive, &[Level::Read]),
    (Service::Docs, &[Level::Read, Level::Write]),
    (Service::Sheets, &[Level::Read, Level::Write]),
    (Service::Calendar, &[Level::Read, Level::Write]),
];

impl Service {
    pub const ALL: [Service; 5] = [
        Service::Gmail,
        Service::Drive,
        Service::Docs,
        Service::Sheets,
        Service::Calendar,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Service::Gmail => "gmail",
            Service::Drive => "drive",
            Service::Docs => "docs",
            Service::Sheets => "sheets",
            Service::Calendar => "calendar",
        }
    }

    /// The levels this service has, lowest first.
    pub fn levels(self) -> &'static [Level] {
        REGISTRY
            .iter()
            .find(|(s, _)| *s == self)
            .map(|(_, l)| *l)
            .unwrap_or(&[])
    }

    /// The Google OAuth scopes this service is connected with. The grant is
    /// broader than what the tools do — Google has no "drafts but no send" —
    /// which is the reason this server is the policy layer.
    pub fn google_scopes(self) -> &'static [&'static str] {
        match self {
            Service::Gmail => &["https://www.googleapis.com/auth/gmail.modify"],
            Service::Drive => &[
                "https://www.googleapis.com/auth/drive.readonly",
                "https://www.googleapis.com/auth/drive.file",
            ],
            Service::Docs => &["https://www.googleapis.com/auth/documents"],
            Service::Sheets => &["https://www.googleapis.com/auth/spreadsheets"],
            Service::Calendar => &[
                "https://www.googleapis.com/auth/calendar.readonly",
                "https://www.googleapis.com/auth/calendar.events",
            ],
        }
    }
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Draft => "draft",
            Level::Modify => "modify",
            Level::Write => "write",
        }
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of a token's capability list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Scope {
    Service(Service, Level),
    Delegate,
}

impl Scope {
    /// The scope this one is useless without: a write level needs the read
    /// level of the same service, because everything a write tool does starts
    /// by looking at what is there. The UI ticks it along, the CLI refuses
    /// the write level without it.
    pub fn requires(self) -> Option<Scope> {
        match self {
            Scope::Service(service, level) if level != Level::Read => {
                Some(Scope::Service(service, Level::Read))
            }
            _ => None,
        }
    }

    pub fn service(self) -> Option<Service> {
        match self {
            Scope::Service(s, _) => Some(s),
            Scope::Delegate => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Service(service, level) => write!(f, "{service}:{level}"),
            Scope::Delegate => f.write_str(SCOPE_DELEGATE),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScopeError {
    #[error("unknown scope {scope:?}; valid scopes are {valid}", scope = .0, valid = valid_scopes().join(", "))]
    Unknown(String),
    #[error("a token needs at least one service scope; valid scopes are {valid}", valid = valid_scopes().join(", "))]
    Empty,
    /// Raised by [`check_requirements`], not by parsing: the strings are all
    /// valid, the set they make is not.
    #[error("{0} is useless without {1}; add it")]
    Missing(Scope, Scope),
}

impl FromStr for Scope {
    type Err = ScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let unknown = || ScopeError::Unknown(s.to_string());
        if s == SCOPE_DELEGATE {
            return Ok(Scope::Delegate);
        }
        let (service, level) = s.split_once(':').ok_or_else(unknown)?;
        let service = Service::ALL
            .into_iter()
            .find(|x| x.as_str() == service)
            .ok_or_else(unknown)?;
        let level = service
            .levels()
            .iter()
            .copied()
            .find(|l| l.as_str() == level)
            .ok_or_else(unknown)?;
        Ok(Scope::Service(service, level))
    }
}

impl TryFrom<String> for Scope {
    type Error = ScopeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Scope> for String {
    fn from(s: Scope) -> String {
        s.to_string()
    }
}

impl FromStr for Service {
    type Err = ScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Service::ALL
            .into_iter()
            .find(|x| x.as_str() == s.trim())
            .ok_or_else(|| ScopeError::Unknown(s.to_string()))
    }
}

impl TryFrom<String> for Service {
    type Error = ScopeError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Service> for String {
    fn from(s: Service) -> String {
        s.as_str().to_string()
    }
}

/// Every scope in the registry, in canonical order, `delegate` last. A
/// session carries all of the service ones, because a person is not narrowed
/// by anything but their own connections.
pub fn all_scopes() -> Vec<Scope> {
    REGISTRY
        .iter()
        .flat_map(|(service, levels)| {
            levels
                .iter()
                .map(move |level| Scope::Service(*service, *level))
        })
        .chain(std::iter::once(Scope::Delegate))
        .collect()
}

/// Every string a token may carry, in canonical order. This is what an error
/// message lists, and what `/api/scopes` is built from.
pub fn valid_scopes() -> Vec<String> {
    all_scopes().iter().map(ToString::to_string).collect()
}

/// Validate a stored or submitted scope list against the registry. The result
/// is deduplicated and in canonical order, so two equivalent lists compare
/// equal. An unknown string is refused with the list of valid ones; a list
/// with no service scope is refused too, because a token that can reach
/// nothing is a mistake, not a capability.
pub fn parse_scopes(scopes: &[String]) -> Result<Vec<Scope>, ScopeError> {
    let mut parsed: Vec<Scope> = Vec::with_capacity(scopes.len());
    for raw in scopes {
        let scope: Scope = raw.trim().parse()?;
        if !parsed.contains(&scope) {
            parsed.push(scope);
        }
    }
    if !parsed.iter().any(|s| s.service().is_some()) {
        return Err(ScopeError::Empty);
    }
    parsed.sort();
    Ok(parsed)
}

/// True when the list carries `delegate`: the token belongs to nobody and
/// acts for the user named in `X-Gmcp-User`.
pub fn is_delegate(scopes: &[Scope]) -> bool {
    scopes.contains(&Scope::Delegate)
}

/// Every write level in the list has its read level. The CLI and the token
/// API call this after [`parse_scopes`].
pub fn check_requirements(scopes: &[Scope]) -> Result<(), ScopeError> {
    for scope in scopes {
        if let Some(needed) = scope.requires()
            && !scopes.contains(&needed)
        {
            return Err(ScopeError::Missing(*scope, needed));
        }
    }
    Ok(())
}

/// The scopes a set must be extended with to satisfy [`Scope::requires`].
/// The token UI ticks these along instead of refusing.
#[allow(dead_code)]
pub fn with_requirements(scopes: &[Scope]) -> Vec<Scope> {
    let mut out = scopes.to_vec();
    for scope in scopes {
        if let Some(needed) = scope.requires()
            && !out.contains(&needed)
        {
            out.push(needed);
        }
    }
    out.sort();
    out.dedup();
    out
}

const fn needs(service: Service, level: Level) -> Option<Scope> {
    Some(Scope::Service(service, level))
}

/// Every MCP tool and the scope it needs. `None` means any token: only
/// `list_accounts`, which is how a model finds out what it can reach at all.
/// `tools/list` is filtered through this table so a model never sees a tool
/// it cannot call, and `call_tool` checks it again.
pub const TOOLS: &[(&str, Option<Scope>)] = &[
    ("list_accounts", None),
    ("gmail_search", needs(Service::Gmail, Level::Read)),
    ("gmail_get_thread", needs(Service::Gmail, Level::Read)),
    ("gmail_get_message", needs(Service::Gmail, Level::Read)),
    ("gmail_list_labels", needs(Service::Gmail, Level::Read)),
    ("gmail_list_send_as", needs(Service::Gmail, Level::Read)),
    ("gmail_attachment_link", needs(Service::Gmail, Level::Read)),
    ("gmail_attachment_text", needs(Service::Gmail, Level::Read)),
    ("gmail_view_image", needs(Service::Gmail, Level::Read)),
    ("gmail_upload_link", needs(Service::Gmail, Level::Draft)),
    ("gmail_create_draft", needs(Service::Gmail, Level::Draft)),
    ("gmail_reply_draft", needs(Service::Gmail, Level::Draft)),
    ("gmail_update_draft", needs(Service::Gmail, Level::Draft)),
    ("gmail_attach_to_draft", needs(Service::Gmail, Level::Draft)),
    ("gmail_list_drafts", needs(Service::Gmail, Level::Draft)),
    ("gmail_delete_draft", needs(Service::Gmail, Level::Draft)),
    ("gmail_modify_labels", needs(Service::Gmail, Level::Modify)),
    ("drive_search", needs(Service::Drive, Level::Read)),
    ("drive_get_file", needs(Service::Drive, Level::Read)),
    ("drive_download_link", needs(Service::Drive, Level::Read)),
    ("drive_export_link", needs(Service::Drive, Level::Read)),
    ("drive_read_text", needs(Service::Drive, Level::Read)),
    ("drive_view_image", needs(Service::Drive, Level::Read)),
    ("docs_read", needs(Service::Docs, Level::Read)),
    ("docs_list_paragraphs", needs(Service::Docs, Level::Read)),
    ("docs_read_formatting", needs(Service::Docs, Level::Read)),
    ("docs_list_images", needs(Service::Docs, Level::Read)),
    ("docs_view_image", needs(Service::Docs, Level::Read)),
    ("docs_image_link", needs(Service::Docs, Level::Read)),
    ("docs_create", needs(Service::Docs, Level::Write)),
    ("docs_append", needs(Service::Docs, Level::Write)),
    ("docs_replace_text", needs(Service::Docs, Level::Write)),
    ("docs_insert_text", needs(Service::Docs, Level::Write)),
    ("docs_edit_paragraph", needs(Service::Docs, Level::Write)),
    ("docs_style_paragraph", needs(Service::Docs, Level::Write)),
    ("docs_insert_code", needs(Service::Docs, Level::Write)),
    ("docs_upload_link", needs(Service::Docs, Level::Write)),
    ("docs_insert_image", needs(Service::Docs, Level::Write)),
    ("sheets_list_tabs", needs(Service::Sheets, Level::Read)),
    ("sheets_read_range", needs(Service::Sheets, Level::Read)),
    ("sheets_append_rows", needs(Service::Sheets, Level::Write)),
    ("sheets_update_range", needs(Service::Sheets, Level::Write)),
    ("sheets_insert_rows", needs(Service::Sheets, Level::Write)),
    ("sheets_delete_rows", needs(Service::Sheets, Level::Write)),
    ("sheets_copy_format", needs(Service::Sheets, Level::Write)),
    ("sheets_add_tab", needs(Service::Sheets, Level::Write)),
    ("sheets_create", needs(Service::Sheets, Level::Write)),
    ("calendar_list", needs(Service::Calendar, Level::Read)),
    (
        "calendar_list_events",
        needs(Service::Calendar, Level::Read),
    ),
    ("calendar_get_event", needs(Service::Calendar, Level::Read)),
    (
        "calendar_create_event",
        needs(Service::Calendar, Level::Write),
    ),
    (
        "calendar_update_event",
        needs(Service::Calendar, Level::Write),
    ),
    (
        "calendar_delete_event",
        needs(Service::Calendar, Level::Write),
    ),
];

/// The tools a token with these scopes may see and call, in table order.
pub fn tools_for(scopes: &[Scope]) -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|(_, required)| required.is_none_or(|r| scopes.contains(&r)))
        .map(|(name, _)| *name)
        .collect()
}

/// The scope a tool needs, or `None` for a tool any token may call and for a
/// name that is not a tool at all; [`is_tool`] tells those two apart.
pub fn scope_for_tool(name: &str) -> Option<Scope> {
    TOOLS
        .iter()
        .find(|(tool, _)| *tool == name)
        .and_then(|(_, required)| *required)
}

pub fn is_tool(name: &str) -> bool {
    TOOLS.iter().any(|(tool, _)| *tool == name)
}

/// The tools each scope unlocks, for `/api/scopes` and the token grid.
pub fn tools_of_scope(scope: Scope) -> Vec<&'static str> {
    TOOLS
        .iter()
        .filter(|(_, required)| *required == Some(scope))
        .map(|(name, _)| *name)
        .collect()
}

/// Asked for on every connection so the callback learns which Google account
/// was connected.
const IDENTITY_SCOPES: [&str; 2] = ["openid", "https://www.googleapis.com/auth/userinfo.email"];

/// Drive is implied by Docs and Sheets: reads and exports of both go through
/// Drive. The connect form ticks it along, and this is where that is decided.
pub fn services_implied(services: &[Service]) -> Vec<Service> {
    let implied = services
        .iter()
        .any(|s| matches!(s, Service::Docs | Service::Sheets));
    Service::ALL
        .into_iter()
        .filter(|s| services.contains(s) || (implied && *s == Service::Drive))
        .collect()
}

/// The Google OAuth scope bundle for a set of services: identity always,
/// then each service's scopes with Drive filled in where it is implied.
pub fn google_scopes_for(services: &[Service]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = IDENTITY_SCOPES.to_vec();
    for service in services_implied(services) {
        for scope in service.google_scopes() {
            if !out.contains(scope) {
                out.push(scope);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(list: &[&str]) -> Vec<Scope> {
        parse_scopes(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    fn parse(list: &[&str]) -> Result<Vec<Scope>, ScopeError> {
        parse_scopes(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn scopes_round_trip_through_their_strings() {
        for s in valid_scopes() {
            assert_eq!(s.parse::<Scope>().unwrap().to_string(), s);
        }
        assert_eq!(valid_scopes().len(), 11);
        assert_eq!(valid_scopes()[0], "gmail:read");
        assert_eq!(valid_scopes().last().unwrap(), "delegate");
    }

    #[test]
    fn the_registry_is_the_matrix_the_plan_names() {
        assert_eq!(
            Service::Gmail.levels(),
            [Level::Read, Level::Draft, Level::Modify]
        );
        assert_eq!(Service::Drive.levels(), [Level::Read]);
        for service in [Service::Docs, Service::Sheets, Service::Calendar] {
            assert_eq!(service.levels(), [Level::Read, Level::Write]);
        }
    }

    #[test]
    fn unknown_scopes_are_refused_with_the_valid_ones() {
        let err = parse(&["gmail:send"]).unwrap_err();
        assert_eq!(err, ScopeError::Unknown("gmail:send".into()));
        let message = err.to_string();
        assert!(message.contains("gmail:read"), "{message}");
        assert!(message.contains("calendar:write"), "{message}");
        assert!(message.contains("delegate"), "{message}");
        // Levels do not leak across services.
        assert!(parse(&["drive:write"]).is_err());
        assert!(parse(&["docs:draft"]).is_err());
        assert!(parse(&["gmail"]).is_err());
        assert!(parse(&["youtube:read"]).is_err());
    }

    #[test]
    fn a_scope_list_is_canonical_and_needs_a_service() {
        assert_eq!(
            scopes(&["docs:read", " gmail:read ", "docs:read"]),
            [
                Scope::Service(Service::Gmail, Level::Read),
                Scope::Service(Service::Docs, Level::Read)
            ]
        );
        assert_eq!(parse(&[]).unwrap_err(), ScopeError::Empty);
        // A delegate token still has to name what it may do.
        assert_eq!(parse(&["delegate"]).unwrap_err(), ScopeError::Empty);
        assert!(is_delegate(&scopes(&["gmail:read", "delegate"])));
        assert!(!is_delegate(&scopes(&["gmail:read"])));
    }

    #[test]
    fn a_write_level_needs_its_read_level() {
        let gmail_read = Scope::Service(Service::Gmail, Level::Read);
        assert_eq!(
            Scope::Service(Service::Gmail, Level::Draft).requires(),
            Some(gmail_read)
        );
        assert_eq!(
            Scope::Service(Service::Gmail, Level::Modify).requires(),
            Some(gmail_read)
        );
        assert_eq!(
            Scope::Service(Service::Docs, Level::Write).requires(),
            Some(Scope::Service(Service::Docs, Level::Read))
        );
        assert_eq!(gmail_read.requires(), None);
        assert_eq!(Scope::Delegate.requires(), None);

        assert_eq!(
            check_requirements(&scopes(&["docs:write"])).unwrap_err(),
            ScopeError::Missing(
                Scope::Service(Service::Docs, Level::Write),
                Scope::Service(Service::Docs, Level::Read)
            )
        );
        assert!(check_requirements(&scopes(&["docs:write", "docs:read"])).is_ok());
        assert_eq!(
            with_requirements(&scopes(&["sheets:write", "delegate"])),
            scopes(&["sheets:read", "sheets:write", "delegate"])
        );
    }

    #[test]
    fn tools_are_listed_per_token() {
        let read_only = tools_for(&scopes(&["gmail:read"]));
        assert_eq!(
            read_only,
            [
                "list_accounts",
                "gmail_search",
                "gmail_get_thread",
                "gmail_get_message",
                "gmail_list_labels",
                "gmail_list_send_as",
                "gmail_attachment_link",
                "gmail_attachment_text",
                "gmail_view_image",
            ]
        );
        assert!(!read_only.contains(&"gmail_create_draft"));
        assert!(!read_only.contains(&"gmail_modify_labels"));

        // Every token sees list_accounts and nothing else for free.
        let none = tools_for(&scopes(&["delegate", "calendar:read"]));
        assert!(none.contains(&"list_accounts"));
        assert!(none.contains(&"calendar_get_event"));
        assert!(!none.contains(&"calendar_create_event"));

        assert_eq!(tools_for(&scopes(&["gmail:read"])).len(), 9);
        let everything = tools_for(&parse_scopes(&valid_scopes()).unwrap());
        assert_eq!(everything.len(), TOOLS.len());
        assert_eq!(TOOLS.len(), 53);
    }

    #[test]
    fn tools_map_back_to_their_scope() {
        assert_eq!(
            scope_for_tool("sheets_append_rows"),
            Some(Scope::Service(Service::Sheets, Level::Write))
        );
        assert_eq!(scope_for_tool("list_accounts"), None);
        assert_eq!(scope_for_tool("gmail_send"), None);
        assert!(is_tool("list_accounts"));
        assert!(!is_tool("gmail_send"));
        // Nothing that sends, trashes or deletes a message is in the table.
        // `send_as` is Google's own word for the addresses a draft may be
        // written as, and reading them sends nothing; every other `send` in a
        // tool name would be a tool this server must never have.
        for (name, _) in TOOLS {
            assert!(!name.replace("send_as", "").contains("send"), "{name}");
            assert!(!name.contains("trash"), "{name}");
        }
        assert!(TOOLS.iter().filter(|(_, s)| s.is_none()).count() == 1);
        assert_eq!(
            tools_of_scope(Scope::Service(Service::Docs, Level::Read)),
            [
                "docs_read",
                "docs_list_paragraphs",
                "docs_read_formatting",
                "docs_list_images",
                "docs_view_image",
                "docs_image_link"
            ]
        );
        assert_eq!(
            tools_of_scope(Scope::Service(Service::Docs, Level::Write)),
            [
                "docs_create",
                "docs_append",
                "docs_replace_text",
                "docs_insert_text",
                "docs_edit_paragraph",
                "docs_style_paragraph",
                "docs_insert_code",
                "docs_upload_link",
                "docs_insert_image",
            ]
        );
    }

    #[test]
    fn google_scope_bundles() {
        assert_eq!(
            google_scopes_for(&[Service::Gmail]),
            [
                "openid",
                "https://www.googleapis.com/auth/userinfo.email",
                "https://www.googleapis.com/auth/gmail.modify",
            ]
        );
        // Docs and Sheets read and export through Drive, so Drive comes along.
        let docs = google_scopes_for(&[Service::Docs]);
        assert!(docs.contains(&"https://www.googleapis.com/auth/drive.readonly"));
        assert!(docs.contains(&"https://www.googleapis.com/auth/drive.file"));
        assert!(docs.contains(&"https://www.googleapis.com/auth/documents"));
        assert_eq!(
            services_implied(&[Service::Sheets]),
            [Service::Drive, Service::Sheets]
        );
        assert_eq!(services_implied(&[Service::Gmail]), [Service::Gmail]);
        // Asked for twice, requested once, in registry order.
        let both = google_scopes_for(&[Service::Sheets, Service::Drive, Service::Docs]);
        assert_eq!(both.iter().filter(|s| s.ends_with("drive.file")).count(), 1);
        assert_eq!(both.len(), 6);
        assert_eq!(
            google_scopes_for(&[Service::Calendar]).last().unwrap(),
            &"https://www.googleapis.com/auth/calendar.events"
        );
    }

    #[test]
    fn scopes_serialise_as_the_strings_the_database_stores() {
        let list = scopes(&["gmail:read", "gmail:draft", "delegate"]);
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(json, r#"["gmail:read","gmail:draft","delegate"]"#);
        assert_eq!(serde_json::from_str::<Vec<Scope>>(&json).unwrap(), list);
        assert!(serde_json::from_str::<Vec<Scope>>(r#"["gmail:send"]"#).is_err());
        assert_eq!(
            serde_json::from_str::<Service>("\"sheets\"").unwrap(),
            Service::Sheets
        );
    }
}
