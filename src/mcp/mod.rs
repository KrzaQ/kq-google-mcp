//! The MCP endpoint at `/mcp`: the curated Gmail, Drive, Docs, Sheets and
//! Calendar tools, over the streamable HTTP transport, bearer tokens only.
//!
//! Three rules shape this module.
//!
//! **The token decides what exists.** `tools/list` is filtered through
//! [`crate::domain::scope::tools_for`], so a model with a `gmail:read` token
//! never sees `sheets_append_rows` and never tries it; `tools/call` checks the
//! same table again, because a client that saw a tool once may call it after
//! the token changed. One server instance serves every token: the principal
//! travels in the request extensions, put there by the bearer middleware and
//! forwarded by rmcp into each call's context.
//!
//! **Nothing is ever sent.** No tool here reaches a send endpoint; a draft is
//! the deliverable and the person presses send in Gmail. Nothing is trashed,
//! `TRASH` and `SPAM` are refused, and every calendar mutation carries
//! `sendUpdates=none` and refuses an event that has attendees.
//!
//! **Every call is logged.** `call_tool` writes one `tool_call` row around the
//! whole call — the tool, its arguments with the bodies summarised, the
//! connection once one was resolved, how it ended and how long it took.

pub mod calendar;
pub mod docs;
pub mod drive;
pub mod dto;
pub mod gmail;
pub mod images;
pub mod sheets;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use chrono_tz::Tz;
use rmcp::ServerHandler;
use rmcp::handler::server::tool::{Extension, ToolRouter};
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo,
};
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::{RoleServer, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::db::{AuditKind, AuditOutcome, Connection, ConnectionStatus, DbError, NewAuditEntry};
use crate::domain::limits::TEXT_MAX_CHARS;
use crate::domain::scope::{self, Service};
use crate::google::text::{Extractor, truncation_notice};
use crate::http::auth::{self, Principal};
use crate::http::error::ApiError;
use crate::http::{AppState, Google, audit};

/// The house rules, in the one place every client reads before its first call.
fn instructions(portal: &str) -> String {
    format!(
        "Curated Google tools for one or more connected accounts. \
         THIS SERVER NEVER SENDS MAIL. There is no send tool and there never will be: a draft is \
         the deliverable, and the person opens it in Gmail and presses send themselves. Say so \
         when someone asks you to send something. Nothing is trashed either.\n\
         Every tool takes `account`, the label of a connected Google account. Call list_accounts \
         first: it says which labels exist, which of gmail, drive, docs, sheets and calendar each \
         one was connected with, and whether any of them needs attention. An account whose status \
         is needs_reauth cannot be fixed by retrying — the person reconnects it at {portal}, and \
         until they do, every call against it is refused.\n\
         Writes to Docs, Sheets and Calendar take `confirmed`. Call them first with \
         confirmed=false: nothing is written and you get back exactly what would change. Show that \
         to the person, and pass confirmed=true only after they agree. Gmail drafts need no \
         confirmation, because the draft is itself the thing being confirmed.\n\
         A Google Doc is changed paragraph by paragraph: call docs_list_paragraphs first, pass \
         the numbers and the revision_id it answers with to docs_insert_text, \
         docs_edit_paragraph, docs_style_paragraph, docs_insert_code, docs_insert_table, \
         docs_delete_paragraphs or docs_delete_table, and call it again after every write, \
         because one write moves both. The two delete tools are the only ones here that destroy \
         anything, and nothing on this side undoes them. docs_read answers the \
         words and no formatting at all, so read a colour, a font or a weight back with \
         docs_read_formatting. A picture goes into a document the same way a file goes into a \
         draft: mint a URL with docs_upload_link, POST the picture to it yourself, and pass the \
         upload_id to docs_insert_image.\n\
         A spreadsheet cell that displays a number may hold a formula, and writing that number \
         back over it replaces the formula with a frozen value while the sheet goes on looking \
         correct, so read with sheets_read_range render=\"formula\" before you copy or rewrite \
         cells; a write carries values and never formatting, which is what sheets_copy_format is \
         for.\n\
         A file is attached to a draft by uploading it first: gmail_upload_link answers a URL, \
         you POST the bytes to it yourself, and you pass the upload_id you read back to a draft \
         tool as one of `attachments` — this server cannot read a file on your machine. Use \
         gmail_attach_to_draft to put a file on a draft that already exists: it keeps everything \
         that draft has, where gmail_update_draft replaces the whole message. A draft \
         whose body says something is attached while it carries no file is a mistake: the result \
         says so in attachment_warning, and you must tell the person rather than reporting the \
         draft as done.\n\
         A Gmail draft is written as the account's default address unless you pass `from`, which \
         must be one of the account's verified send-as addresses; gmail_list_send_as reports \
         them, and a reply comes from the address the original was delivered to.\n\
         Calendar events here never notify anyone and never carry attendees; an event that \
         already has attendees can be read but not changed or deleted through these tools.\n\
         Times are the person's own, not UTC: every instant comes back on their clock with the \
         offset in force that day, and a time you pass without an offset is read on that same \
         clock. list_accounts says which zone that is and what time it is there. Pass an \
         explicit offset when you have one and it is honoured as written; a bare 2026-09-11T15:00 \
         or 2026-09-11 means that wall-clock time where the person is.\n\
         Files leave through short-lived download links: the *_link tools mint a URL that lives \
         15 minutes and may be fetched a few times. Give it to the person, or curl it. For text \
         there is no need for a link at all: gmail_attachment_text and drive_read_text extract it \
         server-side.\n\
         gmail_view_image, drive_view_image and docs_view_image return the picture itself, \
         downscaled. It is only \
         visible in the turn it was fetched; to look again later, call the tool again. In Claude \
         Code the picture counts against MAX_MCP_OUTPUT_TOKENS (25000 by default) — raise that \
         environment variable if images come back truncated."
    )
}

/// What one tool call knows about who is calling. It travels in the call's
/// extensions, so a tool takes it as an argument and the surrounding
/// [`Gmcp::call_tool`] reads back which connection was resolved for the log.
///
/// The zone is resolved once here, from the acting person's row with the house
/// zone behind it, and every tool takes it from the call. Reaching for a
/// global instead would give two people on one server the same clock.
#[derive(Clone)]
pub struct Call {
    pub principal: Principal,
    /// The acting person's clock: what every instant is rendered in and what
    /// a time without an offset is read on.
    pub tz: Tz,
    connection: Arc<Mutex<Option<i64>>>,
}

impl Call {
    fn new(principal: Principal, house: Tz) -> Self {
        Self {
            tz: principal.user().zone(house),
            principal,
            connection: Arc::new(Mutex::new(None)),
        }
    }

    fn resolved(&self) -> Option<i64> {
        *self.connection.lock().expect("the call's connection lock")
    }

    fn resolve(&self, id: i64) {
        *self.connection.lock().expect("the call's connection lock") = Some(id);
    }
}

#[derive(Clone)]
pub struct Gmcp {
    pub state: AppState,
    /// The `pdftotext` lookup, done once rather than per extraction.
    pub extractor: Arc<Extractor>,
    #[allow(dead_code)] // read by the tool_handler macro through self.tool_router
    tool_router: ToolRouter<Self>,
}

// ----- refusals --------------------------------------------------------------

/// A refusal by policy: a hidden tool, an account this token cannot reach, a
/// service the connection never had, something this server does not do. These
/// are logged as `forbidden`.
pub fn refuse(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_request(message.into(), None)
}

/// A refusal by argument: a malformed instant, a range that makes no sense.
pub fn bad(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

pub fn db_err(e: DbError) -> ErrorData {
    match e {
        DbError::NotFound => bad("not found"),
        DbError::Conflict(m) => bad(m),
        DbError::Sqlx(e) => ErrorData::internal_error(e.to_string(), None),
    }
}

/// Google's own failures, passed through as the plan says: the status and
/// Google's message. The two a person can act on are answered by
/// [`Gmcp::google_err_for`] instead, which knows which account they are about.
fn google_err(e: crate::google::Error) -> ErrorData {
    use crate::google::Error as G;
    match e {
        G::NeedsReauth { .. } | G::NotConfigured(_) => refuse(e.to_string()),
        G::Unsupported(m) => refuse(m),
        G::Google(g) => bad(format!("google returned {}: {}", g.status, g.message)),
        G::TooLarge | G::PdftotextMissing | G::PdftotextTimeout(_) => bad(e.to_string()),
        G::Path(_) | G::Untrusted(_) => refuse(e.to_string()),
        G::Transport(_) | G::Malformed(_) | G::Connection(_) => {
            ErrorData::internal_error(e.to_string(), None)
        }
    }
}

// ----- shared plumbing -------------------------------------------------------

#[tool_router(router = base_router, vis = "pub(crate)")]
impl Gmcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            extractor: Arc::new(Extractor::new()),
            tool_router: Self::base_router()
                + Self::gmail_router()
                + Self::drive_router()
                + Self::docs_router()
                + Self::sheets_router()
                + Self::calendar_router(),
        }
    }

    /// The Google client, or the one error that says this deployment has no
    /// credentials.
    pub fn google(&self) -> Result<&Google, ErrorData> {
        self.state
            .google()
            .ok_or_else(|| refuse("this server has no Google credentials configured"))
    }

    /// The sentence a dead grant gets, in the one place both callers read it
    /// from: [`Gmcp::account`] says it when the row is already marked, and
    /// [`Gmcp::google_err_for`] says the same thing when Google refuses the
    /// refresh token in the middle of a call. A model that saw a connection id
    /// and no URL would have nothing to tell the person.
    pub fn reconnect_message(&self, connection: &Connection) -> String {
        format!(
            "the Google account behind `{}` ({}) must be connected again; \
             retrying will not help. Ask the person to reconnect it at {}",
            connection.label,
            connection.google_email,
            self.portal()
        )
    }

    /// A Google failure with the resolved connection in hand. A refused
    /// refresh token and a refresh token that will not open are the same thing
    /// to the person — the account has to be connected again — so both answer
    /// with the label and the portal rather than with an id or a 500.
    pub fn google_err_for(&self, connection: &Connection, e: crate::google::Error) -> ErrorData {
        use crate::google::Error as G;
        match e {
            G::NeedsReauth { .. } | G::Connection(_) => refuse(self.reconnect_message(connection)),
            other => google_err(other),
        }
    }

    pub fn portal(&self) -> String {
        match self.state.config.public_url.join("/connections") {
            Ok(url) => url.to_string(),
            Err(_) => "/connections".to_string(),
        }
    }

    /// The connection an `account` argument names, checked four ways: the
    /// principal can see it, it is healthy, it has the service the tool needs,
    /// and its use is recorded. Everything else in this module starts here.
    pub async fn account(
        &self,
        call: &Call,
        label: &str,
        service: Service,
    ) -> Result<Connection, ErrorData> {
        let reach = call.principal.reach();
        let label = label.trim();
        let found = self
            .state
            .db
            .find_visible_connection(reach, label)
            .await
            .map_err(db_err)?;
        let Some(connection) = found else {
            let known = self
                .state
                .db
                .visible_connections(reach)
                .await
                .map_err(db_err)?;
            return Err(refuse(unknown_account(label, &known)));
        };
        call.resolve(connection.id);
        if connection.status == ConnectionStatus::NeedsReauth {
            return Err(refuse(self.reconnect_message(&connection)));
        }
        if !connection.services.iter().any(|s| s == service.as_str()) {
            return Err(refuse(format!(
                "connection `{}` has no `{}` service; it was connected with {}. \
                 The person can add it by reconnecting the account at {}",
                connection.label,
                service,
                list_or_none(&connection.services),
                self.portal()
            )));
        }
        if let Err(e) = self.state.db.touch_connection_used(connection.id).await {
            tracing::warn!("connection {}: {e}", connection.id);
        }
        Ok(connection)
    }

    /// The token behind this call, which owns any link it mints. A browser
    /// session cannot reach `/mcp`, so this is always present in practice;
    /// the error keeps that from being an assumption.
    pub fn token_id(&self, call: &Call) -> Result<i64, ErrorData> {
        call.principal
            .token()
            .map(|t| t.id)
            .ok_or_else(|| refuse("download links are minted for bearer tokens only"))
    }

    #[tool(
        description = "The Google accounts this token can reach: the label every other tool's \
                       `account` argument takes, the Google address behind it, which services it \
                       was connected with, and whether it is healthy. Call this first. It also \
                       reports the person's own time zone and what time it is there, which is the \
                       clock every other tool shows times on and reads times against — use it \
                       rather than guessing what \"today\" means. An account marked needs_reauth \
                       is refused by every other tool until the person reconnects it in the \
                       portal — retrying does not help. Images come back downscaled and, in \
                       Claude Code, count against MAX_MCP_OUTPUT_TOKENS."
    )]
    async fn list_accounts(
        &self,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::AccountsOut>, ErrorData> {
        let connections = self
            .state
            .db
            .visible_connections(call.principal.reach())
            .await
            .map_err(db_err)?;
        let accounts: Vec<dto::AccountOut> = connections
            .iter()
            .map(|c| dto::AccountOut::new(c, call.tz))
            .collect();
        let note = if accounts.is_empty() {
            format!(
                "This token can reach no Google account. The person connects one at {}",
                self.portal()
            )
        } else if accounts.iter().any(|a| a.needs_reauth) {
            format!(
                "One or more accounts need to be connected again at {}; \
                 tools against them are refused until that is done",
                self.portal()
            )
        } else {
            "Pass one of these labels as `account` to every other tool".into()
        };
        Ok(Json(dto::AccountsOut {
            accounts,
            timezone: call.tz.name().to_string(),
            now: dto::at_zone(Utc::now(), call.tz),
            note,
        }))
    }
}

/// The message an unknown `account` gets: what was asked for, and everything
/// that does exist, because the model's next call should be right.
fn unknown_account(label: &str, known: &[Connection]) -> String {
    if known.is_empty() {
        return format!(
            "there is no account called `{label}`; this token can reach no account at all. \
             Call list_accounts to see that for yourself"
        );
    }
    let labels: Vec<String> = known.iter().map(|c| format!("`{}`", c.label)).collect();
    format!(
        "there is no account called `{label}`; this token can reach {}",
        labels.join(", ")
    )
}

fn list_or_none(services: &[String]) -> String {
    if services.is_empty() {
        "nothing".to_string()
    } else {
        services
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `max`-style arguments, clamped rather than refused: a model that asks for
/// a thousand messages wants "as many as you have".
pub fn capped(value: Option<u32>, default: u32, max: u32) -> u32 {
    value.unwrap_or(default).clamp(1, max)
}

/// A tool's own `max_chars`, applied on top of the domain cap that
/// `google::text` has already enforced. The notice is part of the text
/// because that is the only place a model reliably reads it.
///
/// When both caps fire, the text arriving here already ends in the extractor's
/// own notice. That line is not content: counting its characters as cut would
/// report a number larger than the document, so it is taken off before the
/// second cap is measured and written afresh with the total.
pub fn cap_text(text: String, already_cut: usize, max_chars: Option<u32>) -> (String, usize) {
    let max = max_chars
        .map(|m| m.max(1) as usize)
        .unwrap_or(TEXT_MAX_CHARS)
        .min(TEXT_MAX_CHARS);
    let notice = (already_cut > 0).then(|| truncation_notice(already_cut));
    let body = match &notice {
        Some(n) => text.strip_suffix(n.as_str()).unwrap_or(&text),
        None => text.as_str(),
    };
    let total = body.chars().count();
    if total <= max {
        return (text, already_cut);
    }
    let kept: String = body.chars().take(max).collect();
    let cut = total - max + already_cut;
    let notice = truncation_notice(cut);
    (format!("{kept}{notice}"), cut)
}

/// A portal error on the way out of a tool. Only link minting raises one, and
/// its failures are the database's, so they are internal rather than the
/// model's to fix.
pub fn api_err(e: ApiError) -> ErrorData {
    match e.status.as_u16() {
        400..=499 => bad(e.message),
        _ => ErrorData::internal_error(e.message, None),
    }
}

/// A shared `account` argument, so the schema of every tool spells it the
/// same way.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
}

// ----- the handler -----------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Gmcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("gmcp", env!("CARGO_PKG_VERSION")).with_title("gmcp"),
            )
            .with_instructions(instructions(&self.portal()))
    }

    /// Per token, by hand: the router knows every tool, the registry says
    /// which of them this token's scopes unlock, and the client is told about
    /// nothing else. The result is deliberately not marked cacheable — two
    /// tokens against one server get two different lists.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let principal = principal_of(&context)?;
        let allowed = scope::tools_for(&principal.scopes());
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| allowed.contains(&t.name.as_ref()))
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    /// The scope check the listing already made, made again — a client may
    /// have cached a listing from before the token changed — and the audit
    /// row that every call leaves behind.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let principal = principal_of(&context)?.clone();
        let started = Utc::now();
        let name = request.name.to_string();
        let args = request
            .arguments
            .clone()
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Null);
        let call = Call::new(principal.clone(), self.state.config.timezone);
        let result = match self.authorise(&name, &principal) {
            Err(e) => Err(e),
            Ok(()) => {
                let mut context = context;
                context.extensions.insert(call.clone());
                let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
                self.tool_router.call(tcc).await
            }
        };
        let (outcome, detail) = match &result {
            Ok(_) => (AuditOutcome::Ok, None),
            Err(e) if e.code == ErrorCode::INVALID_REQUEST => {
                (AuditOutcome::Forbidden, Some(e.message.to_string()))
            }
            Err(e) => (AuditOutcome::Error, Some(e.message.to_string())),
        };
        audit::record(
            &self.state.db,
            NewAuditEntry {
                user_id: Some(principal.user().id),
                token_id: principal.token().map(|t| t.id),
                connection_id: call.resolved(),
                tool: Some(name),
                args: audit::args(args),
                detail,
                duration_ms: Some((Utc::now() - started).num_milliseconds()),
                ..NewAuditEntry::new(started, AuditKind::ToolCall, outcome)
            },
        )
        .await;
        result
    }
}

impl Gmcp {
    /// A tool this token may not call is answered the way an unknown tool is,
    /// with the reason: a model that sees "no such tool" retries, and one that
    /// is told the scope is missing tells the person what to ask for.
    fn authorise(&self, name: &str, principal: &Principal) -> Result<(), ErrorData> {
        if !scope::is_tool(name) {
            return Err(bad(format!("there is no tool called `{name}`")));
        }
        let scopes = principal.scopes();
        match scope::scope_for_tool(name) {
            Some(required) if !scopes.contains(&required) => Err(refuse(format!(
                "`{name}` needs the `{required}` scope and this token does not have it; \
                 it carries {}. The person can mint a token with more in the portal",
                scopes
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            _ => Ok(()),
        }
    }
}

/// The principal the bearer middleware resolved, reached through the HTTP
/// request parts that the transport puts into every request's extensions.
fn principal_of(context: &RequestContext<RoleServer>) -> Result<&Principal, ErrorData> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>())
        .ok_or_else(|| refuse("no bearer token"))
}

// ----- mounting --------------------------------------------------------------

/// Bearer only: no cookies, no dev-mode fallback, because `/mcp` is what a
/// token is for. The principal goes into the request extensions, from where
/// rmcp forwards it into every tool call and into the listing.
async fn require_bearer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match auth::bearer(&state, req.headers()).await {
        Ok(Some(p)) => {
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        Ok(None) => ApiError::unauthorized().into_response(),
        Err(e) => e.into_response(),
    }
}

pub fn router(state: AppState) -> Router<AppState> {
    // The transport refuses a Host it does not know, which is what keeps a
    // DNS-rebinding page in a browser from talking to a local server.
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Some(h) = state.config.public_url.host_str() {
        hosts.push(h.to_string());
        if let Some(p) = state.config.public_url.port() {
            hosts.push(format!("{h}:{p}"));
        }
    }
    if let Some(port) = state.config.public_url.port() {
        for host in ["localhost", "127.0.0.1"] {
            hosts.push(format!("{host}:{port}"));
        }
    }
    let config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_allowed_hosts(hosts);
    let factory_state = state.clone();
    let service: StreamableHttpService<Gmcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(Gmcp::new(factory_state.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(state, require_bearer))
}
