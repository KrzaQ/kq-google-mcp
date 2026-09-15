//! The Drive tools: finding files, describing them, and getting at their
//! contents either as text a model can read or as a short-lived link a person
//! can click. Nothing here writes to Drive; the two tools that create files
//! live in `docs` and `sheets`, where they need confirmation.

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto;
use super::gmail::link_out;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text, capped, refuse};
use crate::domain::scope::Service;
use crate::google::drive::{self, ExportFormat};
use crate::google::text;
use crate::http::links::{self, NewDownload, Target};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveSearchParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// A raw Drive query clause, for a caller that knows the syntax, e.g.
    /// "'me' in owners and fullText contains 'invoice'"
    pub query: Option<String>,
    /// A fragment of the file name
    pub name_contains: Option<String>,
    /// An exact MIME type, e.g. "application/pdf" or
    /// "application/vnd.google-apps.spreadsheet"
    pub mime_type: Option<String>,
    /// Only files changed after this time. An RFC 3339 instant with an
    /// offset, or a plain 2026-09-08T14:00 or 2026-09-08 on the person's own
    /// clock
    pub modified_after: Option<String>,
    /// How many files to return; default 20, at most 100
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileParam {
    pub account: String,
    pub file_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportParam {
    pub account: String,
    pub file_id: String,
    /// "markdown", "pdf" or "docx" for a Google Doc; "xlsx" or "pdf" for a Sheet
    pub format: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentsParam {
    pub account: String,
    pub file_id: String,
    /// Also return the threads somebody has already resolved; default false
    pub include_resolved: Option<bool>,
    /// How many threads to return; default 50, at most 100
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadTextParam {
    pub account: String,
    pub file_id: String,
    /// Stop after this many characters, with a notice saying what was cut
    pub max_chars: Option<u32>,
}

#[tool_router(router = drive_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "Find files in Drive by name, type, age or a raw Drive query, newest \
                       change first. Returns id, name, MIME type, modified time, size, owners and \
                       the web link. Files in the bin are never listed."
    )]
    async fn drive_search(
        &self,
        Parameters(p): Parameters<DriveSearchParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::FilesOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let after = p
            .modified_after
            .as_deref()
            .map(|a| instant(a, call.tz))
            .transpose()?;
        let search = drive::Search {
            query: p.query,
            name_contains: p.name_contains,
            mime_type: p.mime_type,
            modified_after: after.as_ref().map(|m| m.at),
            max: Some(capped(p.max, 20, 100)),
        };
        let files = drive::list(&self.google()?.client, connection.id, &search)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::FilesOut {
            account: connection.label,
            count: files.len(),
            files: files
                .into_iter()
                .map(|f| dto::FileOut::new(f, call.tz))
                .collect(),
            note: after.and_then(|m| m.note),
        }))
    }

    #[tool(description = "One file's metadata: name, MIME type, size, owners, modified time.")]
    async fn drive_get_file(
        &self,
        Parameters(p): Parameters<FileParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::FileOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let file = drive::get(&self.google()?.client, connection.id, p.file_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::FileOut::new(file, call.tz)))
    }

    #[tool(
        description = "A download URL for a file that has bytes of its own: a PDF, a picture, an \
                       upload. Google Docs and Sheets have no bytes — use drive_export_link for \
                       those. The link lives 15 minutes and may be fetched a few times."
    )]
    async fn drive_download_link(
        &self,
        Parameters(p): Parameters<FileParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::LinkOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let file = drive::get(&self.google()?.client, connection.id, p.file_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        if file.is_google_native() {
            return Err(refuse(format!(
                "{} is a Google {} and has no file to download; use drive_export_link",
                file.name,
                file.mime_type
                    .trim_start_matches("application/vnd.google-apps.")
            )));
        }
        let minted = links::mint(
            &self.state,
            call.principal.user().id,
            NewDownload {
                connection_id: connection.id,
                token_id: self.token_id(&call)?,
                target: Target::DriveDownload {
                    file_id: file.id.clone(),
                },
                filename: file.name.clone(),
                mime_type: file.mime_type.clone(),
                size: file.size.and_then(|s| i64::try_from(s).ok()),
            },
        )
        .await
        .map_err(api_err)?;
        Ok(Json(link_out(minted, call.tz)))
    }

    #[tool(
        description = "A download URL for a Google Doc or Sheet converted to a real file: \
                       markdown, pdf or docx for a Doc, xlsx or pdf for a Sheet. The link lives \
                       15 minutes. To read a Doc yourself, use docs_read or drive_read_text \
                       instead of exporting it."
    )]
    async fn drive_export_link(
        &self,
        Parameters(p): Parameters<ExportParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::LinkOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let format: ExportFormat = p
            .format
            .parse()
            .map_err(|e: drive::ExportFormatError| bad(e.to_string()))?;
        let file = drive::get(&self.google()?.client, connection.id, p.file_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        drive::check_export_format(&file, format)
            .map_err(|e| self.google_err_for(&connection, e))?;
        let minted = links::mint(
            &self.state,
            call.principal.user().id,
            NewDownload {
                connection_id: connection.id,
                token_id: self.token_id(&call)?,
                target: Target::DriveExport {
                    file_id: file.id.clone(),
                    format,
                },
                filename: format!("{}.{}", file.name, format.extension()),
                mime_type: format.mime_type().to_string(),
                // An export has no size until it is made.
                size: None,
            },
        )
        .await
        .map_err(api_err)?;
        Ok(Json(link_out(minted, call.tz)))
    }

    #[tool(
        description = "The text of a Drive file: a Google Doc as markdown, a Google Sheet as CSV \
                       per tab, a PDF through poppler, a DOCX from its document part, and plain \
                       text and CSV as they are. Long text is cut with a notice saying how much \
                       was left out."
    )]
    async fn drive_read_text(
        &self,
        Parameters(p): Parameters<ReadTextParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::TextOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let client = &self.google()?.client;
        let file = drive::get(client, connection.id, p.file_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let extraction = text::drive_file(client, connection.id, &self.extractor, &file)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let (body, truncated) = cap_text(extraction.text, extraction.truncated_chars, p.max_chars);
        Ok(Json(dto::TextOut {
            account: connection.label,
            source: extraction.source.to_string(),
            filename: Some(file.name),
            chars: body.chars().count(),
            truncated_chars: truncated,
            text: body,
        }))
    }

    #[tool(
        description = "A picture from Drive, downscaled and returned as an image you can look \
                       at. It is visible only in the turn it is fetched; call again to look \
                       later. Formats this server cannot decode (HEIC, SVG) are link-only."
    )]
    async fn drive_view_image(
        &self,
        Parameters(p): Parameters<FileParam>,
        Extension(call): Extension<Call>,
    ) -> Result<CallToolResult, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let client = &self.google()?.client;
        let file = drive::get(client, connection.id, p.file_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        if !file.mime_type.starts_with("image/") {
            return Err(refuse(format!(
                "{} is a {}, not a picture; use drive_read_text or drive_download_link",
                file.name, file.mime_type
            )));
        }
        let bytes = drive::download(client, connection.id, &file.id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?
            .collect()
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        images::content(
            call.principal.client_profile(),
            Source {
                connection_id: connection.id,
                kind: Kind::DriveFile,
                ids: &[&file.id],
                filename: &file.name,
                mime_type: &file.mime_type,
            },
            &bytes,
        )
    }

    #[tool(
        description = "The comments on a Google Doc, Sheet or any other Drive file: for each \
                       thread who wrote it and when, the text it is anchored to, and its replies \
                       oldest first. Read the margin before you edit a document somebody else is \
                       also writing. Threads somebody has resolved are left out unless \
                       include_resolved is true. This reads only: no tool here writes a comment, \
                       a reply or a suggestion, so answer a comment by telling the person what it \
                       says and what you changed."
    )]
    async fn drive_list_comments(
        &self,
        Parameters(p): Parameters<CommentsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::CommentsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let file_id = p.file_id.trim();
        let include_resolved = p.include_resolved.unwrap_or(false);
        let max = capped(p.max, 50, 100) as usize;
        let read = drive::comments(&self.google()?.client, connection.id, file_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;

        let all = read.threads.len();
        let mut threads: Vec<drive::Comment> = read
            .threads
            .into_iter()
            .filter(|c| include_resolved || !c.resolved)
            .collect();
        let resolved = all - threads.len();
        let over = threads.len().saturating_sub(max);
        threads.truncate(max);
        Ok(Json(dto::CommentsOut {
            account: connection.label,
            file_id: file_id.to_string(),
            count: threads.len(),
            comments: threads
                .into_iter()
                .map(|c| dto::CommentOut::new(c, call.tz))
                .collect(),
            note: comments_note(resolved, over, read.more),
        }))
    }
}

/// What the answer leaves out, in the one line a model reads before it decides
/// it has the whole margin.
fn comments_note(resolved: usize, over: usize, more: bool) -> Option<String> {
    let mut said: Vec<String> = Vec::new();
    if resolved > 0 {
        said.push(format!(
            "{resolved} resolved {} not shown; pass include_resolved=true to read {}",
            plural(resolved, "thread is", "threads are"),
            plural(resolved, "it", "them")
        ));
    }
    if over > 0 {
        said.push(format!(
            "{over} more {} left out by `max`; raise it to see {}",
            plural(over, "thread was", "threads were"),
            plural(over, "it", "them")
        ));
    }
    if more {
        said.push(
            "this file has more comments than one call reads, and only the first hundred \
             threads were looked at"
                .to_string(),
        );
    }
    (!said.is_empty()).then(|| said.join(". "))
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

/// A time argument, once it has been read, and the one thing the tool's reply
/// has to say about how it was read.
#[derive(Debug)]
pub(super) struct Moment {
    pub at: DateTime<Utc>,
    /// Set only when the wall-clock time given happens twice, because the
    /// clock went back that night. The earlier of the two was taken, and the
    /// tool says so rather than leaving the person to wonder which hour it
    /// booked.
    pub note: Option<String>,
}

/// A time from an argument, read on the acting person's clock. Three shapes,
/// tried in this order:
///
/// * RFC 3339 with an explicit offset (`2026-09-11T15:00:00+02:00`, `...Z`),
///   which is honoured exactly as written;
/// * a wall-clock time with no offset (`2026-09-11T15:00:00`,
///   `2026-09-11T15:00`, a space in place of the `T`), read on `tz`;
/// * a bare date (`2026-09-11`), which is the start of that day on `tz`.
///
/// The two clock changes are decided here, once, for every tool. A time that
/// the spring-forward skipped never happened, so it is refused and the gap is
/// named: booking an hour that does not exist would silently become a
/// different hour. A time the autumn fold repeats happened twice, so the
/// earlier of the two is taken and [`Moment::note`] says so.
pub(super) fn instant(value: &str, tz: Tz) -> Result<Moment, ErrorData> {
    let value = value.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(value) {
        return Ok(Moment {
            at: d.with_timezone(&Utc),
            note: None,
        });
    }
    let wall = wall_clock(value).ok_or_else(|| {
        bad(format!(
            "{value:?} is not a time. Write it on the person's own clock as \
             2026-09-08T14:00, 2026-09-08 14:00 or 2026-09-08 (which is the start of that day in \
             {tz}), or with an explicit offset — 2026-09-08T14:00:00+02:00 — which is used exactly \
             as written"
        ))
    })?;
    match tz.from_local_datetime(&wall) {
        LocalResult::Single(at) => Ok(Moment {
            at: at.with_timezone(&Utc),
            note: None,
        }),
        LocalResult::Ambiguous(earlier, _) => Ok(Moment {
            at: earlier.with_timezone(&Utc),
            note: Some(format!(
                "{value} happens twice in {tz} that night, because the clock goes back an hour; \
                 the earlier of the two, {}, was used",
                earlier.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
            )),
        }),
        LocalResult::None => Err(bad(match gap(wall, tz) {
            Some((from, to)) => format!(
                "{value:?} never happens in {tz}: the clock jumps from {} to {} on {}, so that \
                 hour does not exist. Give a time outside the gap, or write it with an explicit \
                 offset",
                from.format("%H:%M"),
                to.format("%H:%M"),
                to.format("%Y-%m-%d"),
            ),
            None => format!("{value:?} never happens in {tz}: the clock skips it"),
        })),
    }
}

/// A wall-clock time with no offset, in the shapes a model writes it.
fn wall_clock(value: &str) -> Option<NaiveDateTime> {
    for shape in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(value, shape) {
            return Some(d);
        }
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
}

/// The local times a spring-forward skipped, so the refusal can name them.
/// The transition is somewhere within a day of a time that does not exist, and
/// the offset before it differs from the offset after: a bisection on that
/// difference finds the second it happens.
fn gap(wall: NaiveDateTime, tz: Tz) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let offset = |at: DateTime<Utc>| tz.offset_from_utc_datetime(&at.naive_utc()).fix();
    let mut before = Utc.from_utc_datetime(&(wall - Duration::days(1)));
    let mut after = Utc.from_utc_datetime(&(wall + Duration::days(1)));
    if offset(before) == offset(after) {
        return None;
    }
    while after - before > Duration::seconds(1) {
        let middle = before + (after - before) / 2;
        if offset(middle) == offset(before) {
            before = middle;
        } else {
            after = middle;
        }
    }
    // `before` is the last second of the old offset, so the gap starts one
    // second after it *on the old clock*; converting it first would show the
    // new offset and name the same time twice.
    Some((
        before.with_timezone(&tz).naive_local() + Duration::seconds(1),
        after.with_timezone(&tz).naive_local(),
    ))
}
