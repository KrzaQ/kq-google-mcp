//! The Drive tools: finding files, describing them, getting at their contents
//! either as text a model can read or as a short-lived link a person can
//! click, and storing a file the caller uploaded.
//!
//! drive_upload is the one write here, and it only ever adds a file: it never
//! changes, moves or replaces one that is there. It takes `confirmed` like
//! every other write, and it refuses rather than put a file anywhere but the
//! folder it was asked for. The two tools that create a Doc or a Sheet live in
//! `docs` and `sheets`.

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::gmail::link_out;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text, capped, refuse};
use crate::db::Connection;
use crate::domain::scope::Service;
use crate::google::drive::{self, ExportFormat};
use crate::google::{Error as GoogleFailure, text};
use crate::http::links::{self, NewDownload, Target};
use crate::http::uploads;

/// What the root of My Drive is called where a folder's name would go.
const MY_DRIVE: &str = "My Drive";

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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveUploadLinkParam {
    /// What the file is called, e.g. "Faktura 04-2026.pdf". drive_upload
    /// stores it under this name unless it is given another
    pub filename: String,
    /// What the file is, e.g. application/pdf. Left out, the filename decides
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveUploadParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The upload_id you read back from POSTing the file to a
    /// drive_upload_link URL
    pub upload_id: String,
    /// The id of the Drive folder to put the file in, as drive_search reports
    /// it. Left out, the file goes to the root of My Drive
    pub folder: Option<String>,
    /// What to call the file in Drive. Left out, it keeps the name it was
    /// uploaded under
    pub name: Option<String>,
    /// Must be true to write. Call with false first and show the person the
    /// name, the size and the folder.
    pub confirmed: bool,
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

    #[tool(
        description = "A URL to upload one file to, so it can be stored in Google Drive. Putting \
                       a file in Drive takes three steps and you do the middle one yourself: call \
                       this, then POST the bytes to the `url` it answers \
                       (`curl --data-binary @report.pdf URL`), then pass the `upload_id` you read \
                       back to drive_upload. This server cannot read a file on your machine, so \
                       uploading it is the only way. A file may be at most 25 MB. There is no \
                       `account` here because a staged file belongs to you and not to a Drive: \
                       drive_upload decides which account and which folder it lands in. The URL \
                       takes one upload and lives 15 minutes; the file itself waits an hour to be \
                       used and is forgotten once it is. This is the Drive twin of \
                       gmail_upload_link and docs_upload_link — same staging, its own name."
    )]
    async fn drive_upload_link(
        &self,
        Parameters(p): Parameters<DriveUploadLinkParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::UploadLinkOut>, ErrorData> {
        let filename = p.filename.trim();
        if filename.is_empty() {
            return Err(bad("filename is empty; name the file you are uploading"));
        }
        let minted = uploads::mint(
            &self.state,
            uploads::NewUpload {
                user_id: call.principal.user().id,
                token_id: self.token_id(&call)?,
                filename: filename.to_string(),
                mime_type: p.content_type.clone(),
            },
        );
        Ok(Json(dto::UploadLinkOut {
            url: minted.url,
            filename: minted.filename,
            expires_at: dto::at_zone(minted.expires_at, call.tz),
            note: "POST the file to this URL as the whole request body — \
                   `curl --data-binary @/path/to/file URL` — and pass the upload_id it answers \
                   to drive_upload. The URL works once and for 15 minutes."
                .into(),
        }))
    }

    #[tool(
        description = "Store a file in Google Drive as it is, with no conversion: a PDF stays a \
                       PDF and a spreadsheet stays the file it was. Upload it first with \
                       drive_upload_link and pass the upload_id here. `folder` is the id of a \
                       Drive folder, as drive_search reports it; left out, the file goes to the \
                       root of My Drive. `name` is what the file is called in Drive, and left \
                       out it keeps the name it was uploaded under. An upload never replaces an \
                       existing file: if the folder already holds one of the same name, Drive \
                       holds both side by side. Shared drives are out of reach. A folder the \
                       person made in Drive may not take a file from this server; when it does \
                       not, nothing is uploaded anywhere, the upload stays staged, and leaving \
                       `folder` out puts the file in the root of My Drive instead. Needs \
                       confirmed=true: call once with confirmed=false, show the person the name, \
                       the size and the folder, and write only after they say yes. The upload \
                       is spent once Drive has the file, so upload it again to store it twice."
    )]
    async fn drive_upload(
        &self,
        Parameters(p): Parameters<DriveUploadParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DriveUploadOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let client = &self.google()?.client;
        let upload_id = p.upload_id.trim().to_string();
        let user_id = call.principal.user().id;
        // Read without being spent: a preview and every refusal below leave
        // the upload_id good for the next attempt.
        let staged = self
            .state
            .staging
            .peek(user_id, &upload_id)
            .map_err(|e| bad(e.to_string()))?;
        if staged.mime_type.starts_with(drive::GOOGLE_APPS_PREFIX) {
            return Err(bad(format!(
                "{} was uploaded as {}, which is a type Drive converts a file into; drive_upload \
                 stores a file as it is. Upload it again with the type it really has",
                staged.filename, staged.mime_type
            )));
        }
        let name = p
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&staged.filename)
            .to_string();
        let folder = match p.folder.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            None => None,
            Some(id) => Some(self.destination(&connection, id).await?),
        };
        let size = staged.bytes.len();
        let into = match &folder {
            Some(f) => format!("the folder {:?} ({})", f.name, f.id),
            None => format!("the root of {MY_DRIVE}"),
        };
        if !p.confirmed {
            let mut details = vec![
                format!("the file is called {name:?} in Drive"),
                format!(
                    "it is {} ({size} bytes) of {}, stored as it is with no conversion",
                    uploads::megabytes(size),
                    staged.mime_type
                ),
                format!("it goes into {into}"),
            ];
            if name != staged.filename {
                details.push(format!("it was uploaded as {:?}", staged.filename));
            }
            details.push(
                "an upload never replaces a file: if one of the same name is already there, \
                 Drive holds both"
                    .to_string(),
            );
            if folder.is_some() {
                details.push(
                    "if Drive refuses this folder to this server, nothing is uploaded anywhere \
                     and the upload stays staged"
                        .to_string(),
                );
            }
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "upload {name:?} to Drive in `{}`, into {into}",
                    connection.label
                ),
                details,
            ))));
        }
        // One attempt, into the folder that was asked for and nowhere else. A
        // refusal is answered as one; it is never retried into the root.
        let stored = match drive::upload(
            client,
            connection.id,
            &name,
            &staged.mime_type,
            &staged.bytes,
            folder.as_ref().map(|f| f.id.as_str()),
        )
        .await
        {
            Ok(stored) => stored,
            Err(e) => {
                return Err(match &folder {
                    Some(f) if folder_refused(&e) => {
                        refuse(folder_refusal(f, &upload_id, &e.to_string()))
                    }
                    _ => self.google_err_for(&connection, e),
                });
            }
        };
        // Drive has the file, so the upload is spent now and not before.
        if let Err(e) = self
            .state
            .staging
            .take(user_id, std::slice::from_ref(&upload_id))
        {
            tracing::warn!("spending the upload {upload_id} after Drive stored it: {e}");
        }
        let folder_name = folder
            .map(|f| f.name)
            .unwrap_or_else(|| MY_DRIVE.to_string());
        let size = stored.size.unwrap_or(size as u64);
        Ok(Json(Confirmable::Done(dto::DriveUploadOut {
            account: connection.label,
            url: stored
                .web_view_link
                .clone()
                .unwrap_or_else(|| format!("https://drive.google.com/file/d/{}/view", stored.id)),
            written: format!(
                "stored {:?} ({}) in {folder_name:?}",
                stored.name,
                uploads::megabytes(size as usize)
            ),
            folder_id: stored.parents.first().cloned(),
            file_id: stored.id,
            name: stored.name,
            mime_type: stored.mime_type,
            size,
            folder: folder_name,
        })))
    }
}

impl Gmcp {
    /// The folder a file is about to go into, read so the person approving
    /// the upload sees its name and not only its id. An id that names no
    /// folder this account can see, names something that is not a folder, or
    /// names a folder in the bin is refused here, before anything is written.
    async fn destination(
        &self,
        connection: &Connection,
        id: &str,
    ) -> Result<drive::Folder, ErrorData> {
        let found = drive::folder(&self.google()?.client, connection.id, id).await;
        let folder = match found {
            Ok(folder) => folder,
            Err(GoogleFailure::Google(g)) if g.status == 404 => {
                return Err(refuse(format!(
                    "there is no folder `{id}` that `{}` can see. A folder in a shared drive is \
                     out of reach of these tools. Leave `folder` out to put the file in the \
                     root of {MY_DRIVE}",
                    connection.label
                )));
            }
            Err(e) => return Err(self.google_err_for(connection, e)),
        };
        if folder.mime_type != drive::FOLDER_MIME {
            return Err(refuse(format!(
                "`{id}` is {:?}, a {}, and not a folder; pass the id of a folder, or leave \
                 `folder` out to put the file in the root of {MY_DRIVE}",
                folder.name, folder.mime_type
            )));
        }
        if folder.trashed {
            return Err(refuse(format!(
                "the folder {:?} is in the bin, and a file put there would be in the bin too; \
                 pick another folder, or leave `folder` out to put the file in the root of \
                 {MY_DRIVE}",
                folder.name
            )));
        }
        Ok(folder)
    }
}

/// True when Drive refused the parent folder rather than the file. These are
/// the three answers the Drive reference gives for a file the app may not
/// write or cannot see: `notFound` (404), and `insufficientFilePermissions`
/// and `appNotAuthorizedToFile` (403). Every other 403 — a full quota, a rate
/// limit — is about the account and not the folder, and is passed through.
fn folder_refused(e: &GoogleFailure) -> bool {
    match e {
        GoogleFailure::Google(g) => {
            g.status == 404
                || (g.status == 403
                    && matches!(
                        g.reason.as_deref(),
                        Some("insufficientFilePermissions" | "appNotAuthorizedToFile")
                    ))
        }
        _ => false,
    }
}

/// What a caller is told when Drive refused the folder. The file was put
/// nowhere else, and the caller's next step is in the message.
fn folder_refusal(folder: &drive::Folder, upload_id: &str, google: &str) -> String {
    format!(
        "Drive refused to put the file into the folder {:?} ({}): that folder could not be \
         written to with the access this server holds. Nothing was uploaded, and the file was \
         not put anywhere else. The upload `{upload_id}` is still staged: call drive_upload \
         again with the same upload_id and leave `folder` out to put the file in the root of \
         {MY_DRIVE}, where the person can move it. ({google})",
        folder.name, folder.id
    )
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

pub(super) fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
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
