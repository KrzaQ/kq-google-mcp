//! The Drive tools: finding files, describing them, getting at their contents
//! either as text a model can read or as a short-lived link a person can
//! click, storing a file the caller uploaded, and making a folder.
//!
//! drive_upload and drive_create_folder only ever add: neither changes, moves
//! or replaces what is there, and they refuse rather than put anything
//! anywhere but the folder they were asked for. drive_update_file is the one
//! write that replaces, and it replaces only content it has first made safe:
//! the version it replaces is marked keep forever before anything else is
//! sent. All three take `confirmed` like every other write. The two tools
//! that create a Doc or a Sheet live in `docs` and `sheets`.

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
    /// "application/vnd.google-apps.spreadsheet"; folders are
    /// "application/vnd.google-apps.folder"
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
    /// it. Left out, along with folder_path, the file goes to the root of My
    /// Drive
    pub folder: Option<String>,
    /// The folder to put the file in, as a path from the root of My Drive,
    /// e.g. "topologia/notatki". Each folder on it that is missing is
    /// created, as `mkdir -p` does. Give this or `folder`, never both
    pub folder_path: Option<String>,
    /// What to call the file in Drive. Left out, it keeps the name it was
    /// uploaded under
    pub name: Option<String>,
    /// Must be true to write. Call with false first and show the person the
    /// name, the size and the folder.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveCreateFolderParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// What to call the new folder, e.g. "topologia"
    pub name: String,
    /// The id of the Drive folder to create it in, as drive_search reports
    /// it. Left out, the folder goes to the root of My Drive
    pub parent: Option<String>,
    /// Must be true to write. Call with false first and show the person the
    /// name and where the folder goes.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveUpdateFileParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The id of the Drive file whose content is replaced, as drive_search
    /// reports it
    pub file_id: String,
    /// The upload_id you read back from POSTing the new content to a
    /// drive_upload_link URL
    pub upload_id: String,
    /// A new name for the file. Left out, the file keeps its name
    pub name: Option<String>,
    /// Must be true to write. Call with false first and show the person the
    /// file, its folder, both sizes and where the old version stays.
    pub confirmed: bool,
}

#[tool_router(router = drive_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "Find files in Drive by name, type, age or a raw Drive query, newest \
                       change first. Returns id, name, MIME type, modified time, size, owners and \
                       the web link. Files in the bin are never listed. A folder is a file too: \
                       pass mime_type \"application/vnd.google-apps.folder\" to list folders, \
                       and pass a folder's id as `folder` to drive_upload or as `parent` to \
                       drive_create_folder."
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
                       back to drive_upload, or to drive_update_file to replace the content of a \
                       file already in Drive. This server cannot read a file on your machine, so \
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
                   to drive_upload or drive_update_file. The URL works once and for 15 minutes."
                .into(),
        }))
    }

    #[tool(
        description = "Store a file in Google Drive as it is, with no conversion: a PDF stays a \
                       PDF and a spreadsheet stays the file it was. Upload it first with \
                       drive_upload_link and pass the upload_id here. `folder` is the id of a \
                       Drive folder, as drive_search reports it. `folder_path` names the folder \
                       by its path from the root of My Drive instead, such as \
                       topologia/notatki, and creates each folder on it that is missing; when \
                       two folders on the way share a name, the call is refused with both ids \
                       so you can pass the right one as `folder`. Give one of the two, or \
                       neither to put the file in the root of My Drive. A folder this call \
                       creates stays in Drive even when a later step fails, and the answer \
                       names it. `name` is what the file is called in Drive, and left \
                       out it keeps the name it was uploaded under. An upload never replaces an \
                       existing file: if the folder already holds one of the same name, Drive \
                       holds both side by side. Shared drives are out of reach. A folder the \
                       person made in Drive may not take a file from this server; when it does \
                       not, nothing is uploaded anywhere, the upload stays staged, and leaving \
                       the folder out puts the file in the root of My Drive instead. Needs \
                       confirmed=true: call once with confirmed=false, show the person the name, \
                       the size and the folder, and for a path which folders are new and which \
                       are reused. Write only after they say yes. The upload \
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
        let folder_id = p.folder.as_deref().map(str::trim).filter(|f| !f.is_empty());
        let folder_path = p
            .folder_path
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty());
        if folder_id.is_some() && folder_path.is_some() {
            return Err(bad(
                "pass `folder` or `folder_path`, not both: one destination, named one way. \
                 `folder` is the id of a folder and `folder_path` its path from the root of My \
                 Drive",
            ));
        }
        let segments = folder_path.map(path_segments).transpose()?;
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
        let destination = match (folder_id, segments) {
            (Some(id), _) => {
                Destination::Folder(self.destination(&connection, id, Putting::File).await?)
            }
            (None, Some(segments)) => {
                Destination::Path(self.plan_path(&connection, segments, &upload_id).await?)
            }
            (None, None) => Destination::Root,
        };
        let size = staged.bytes.len();
        let into = destination.describe();
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
            if let Destination::Path(plan) = &destination {
                details.extend(plan.preview());
            }
            if name != staged.filename {
                details.push(format!("it was uploaded as {:?}", staged.filename));
            }
            details.push(
                "an upload never replaces a file: if one of the same name is already there, \
                 Drive holds both"
                    .to_string(),
            );
            if let Destination::Folder(_) = &destination {
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
        let arg = match &destination {
            Destination::Path(_) => "folder_path",
            _ => "folder",
        };
        // A path's missing folders first, one at a time, each inside the one
        // before. Whatever this creates stays, even when the upload fails.
        let (place, created) = match destination {
            Destination::Root => (None, Vec::new()),
            Destination::Folder(f) => (
                Some(Place {
                    id: f.id,
                    name: f.name,
                }),
                Vec::new(),
            ),
            Destination::Path(plan) => {
                let (place, created) = self.make_path(&connection, plan, &upload_id).await?;
                (Some(place), created)
            }
        };
        // One attempt, into the folder that was asked for and nowhere else. A
        // refusal is answered as one; it is never retried into the root.
        let stored = match drive::upload(
            client,
            connection.id,
            &name,
            &staged.mime_type,
            &staged.bytes,
            place.as_ref().map(|f| f.id.as_str()),
        )
        .await
        {
            Ok(stored) => stored,
            Err(e) => {
                let refused = place.is_some() && access_refused(&e);
                let error = match &place {
                    Some(f) if refused => {
                        refuse(folder_refusal(f, arg, &upload_id, &e.to_string()))
                    }
                    _ => self.google_err_for(&connection, e),
                };
                if created.is_empty() {
                    return Err(error);
                }
                let staged_note = if refused {
                    String::new()
                } else {
                    format!("The upload `{upload_id}` is still staged. ")
                };
                return Err(with_note(
                    error,
                    &format!("{staged_note}{}", created_note(&created)),
                ));
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
        let folder_name = place
            .map(|f| f.name)
            .unwrap_or_else(|| MY_DRIVE.to_string());
        let size = stored.size.unwrap_or(size as u64);
        let mut written = format!(
            "stored {:?} ({}) in {folder_name:?}",
            stored.name,
            uploads::megabytes(size as usize)
        );
        if !created.is_empty() {
            written.push_str(&format!(
                "; this call created {} {} on the way",
                plural(created.len(), "the folder", "the folders"),
                names(&created)
            ));
        }
        Ok(Json(Confirmable::Done(dto::DriveUploadOut {
            account: connection.label,
            url: stored
                .web_view_link
                .clone()
                .unwrap_or_else(|| format!("https://drive.google.com/file/d/{}/view", stored.id)),
            written,
            folder_id: stored.parents.first().cloned(),
            file_id: stored.id,
            name: stored.name,
            mime_type: stored.mime_type,
            size,
            folder: folder_name,
            created_folders: created
                .into_iter()
                .map(|m| dto::CreatedFolderOut {
                    folder_id: m.id,
                    name: m.name,
                })
                .collect(),
        })))
    }

    #[tool(
        description = "Create one folder in Google Drive: in `parent`, the id of a Drive folder \
                       as drive_search reports it, or in the root of My Drive when `parent` is \
                       left out. Answers the new folder's id, name, parent and Drive URL. Pass \
                       that id as `folder` to drive_upload, or as `parent` here to go one level \
                       deeper. This server's access to Drive covers the files it created itself, \
                       so a folder made here is one it can always put files into, while a folder \
                       the person made by hand may refuse them. Drive holds two folders of one \
                       name side by side, so this makes a new folder even when one of that name \
                       is already there; the preview says when one is. Shared drives are out of \
                       reach. Needs confirmed=true: call once with confirmed=false, show the \
                       person the name and where the folder goes, and create it only after they \
                       say yes."
    )]
    async fn drive_create_folder(
        &self,
        Parameters(p): Parameters<DriveCreateFolderParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DriveFolderOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let client = &self.google()?.client;
        let name = p.name.trim().to_string();
        if name.is_empty() {
            return Err(bad("name is empty; name the folder to create"));
        }
        let parent = match p.parent.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            None => None,
            Some(id) => Some(self.destination(&connection, id, Putting::Folder).await?),
        };
        let into = match &parent {
            Some(f) => format!("the folder {:?} ({})", f.name, f.id),
            None => format!("the root of {MY_DRIVE}"),
        };
        if !p.confirmed {
            let there = drive::folders_named(
                client,
                connection.id,
                parent.as_ref().map(|f| f.id.as_str()),
                &name,
            )
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
            let mut details = vec![
                format!("the new folder is called {name:?}"),
                format!("it goes into {into}"),
            ];
            details.push(if there.folders.is_empty() && !there.more {
                format!("no folder called {name:?} is there yet")
            } else {
                format!(
                    "a folder called {name:?} is already there ({}); Drive holds folders of one \
                     name side by side, so this makes another one. To use the one already \
                     there, pass its id as `folder` to drive_upload instead",
                    ids(&there)
                )
            });
            if parent.is_some() {
                details.push(
                    "if Drive refuses this folder to this server, nothing is created anywhere"
                        .to_string(),
                );
            }
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "create the folder {name:?} in Drive in `{}`, in {into}",
                    connection.label
                ),
                details,
            ))));
        }
        // One attempt, into the parent that was asked for and nowhere else.
        let made = match drive::create_folder(
            client,
            connection.id,
            &name,
            parent.as_ref().map(|f| f.id.as_str()),
        )
        .await
        {
            Ok(made) => made,
            Err(e) => {
                return Err(match &parent {
                    Some(f) if access_refused(&e) => {
                        refuse(parent_refusal(f, &name, &e.to_string()))
                    }
                    _ => self.google_err_for(&connection, e),
                });
            }
        };
        let parent_name = parent
            .map(|f| f.name)
            .unwrap_or_else(|| MY_DRIVE.to_string());
        Ok(Json(Confirmable::Done(dto::DriveFolderOut {
            account: connection.label,
            url: folder_url(&made),
            written: format!("created the folder {:?} in {parent_name:?}", made.name),
            parent_id: made.parents.first().cloned(),
            folder_id: made.id,
            name: made.name,
            parent: parent_name,
        })))
    }

    #[tool(
        description = "Replace the content of a file already in Google Drive with a file you \
                       uploaded, in place: the file keeps its id, its link and its folder. \
                       Upload the new content first with drive_upload_link and pass the \
                       upload_id here. The upload must have the file's own type, so a PDF is \
                       replaced only by a PDF; nothing is converted. A Google Doc, Sheet or \
                       other Google file has no file content to replace, and the docs_* and \
                       sheets_* tools write to it. Before anything is replaced, this server \
                       marks the current version keep forever in the file's version history, \
                       where the person finds it under Manage versions in Drive; it does not \
                       appear as a second file in the folder. If Drive refuses that mark, \
                       nothing is replaced. Drive keeps at most 200 versions of one file \
                       forever, and a file that has reached that limit is refused. `name` \
                       renames the file in the same call; left out, the name stays. A file the \
                       person put in Drive by hand may not take a change from this server; \
                       then nothing is changed and the upload stays staged, and drive_upload \
                       can store it as a new file instead. Needs confirmed=true: call once with \
                       confirmed=false, show the person the file, its folder, both sizes and \
                       where the old version stays, and write only after they say yes. The \
                       upload is spent once Drive has the new content."
    )]
    async fn drive_update_file(
        &self,
        Parameters(p): Parameters<DriveUpdateFileParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DriveUpdateOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Drive).await?;
        let client = &self.google()?.client;
        let upload_id = p.upload_id.trim().to_string();
        let user_id = call.principal.user().id;
        let file_id = p.file_id.trim();
        if file_id.is_empty() {
            return Err(bad(
                "file_id is empty; pass the id of the Drive file to replace, as drive_search \
                 reports it",
            ));
        }
        // Read without being spent: a preview and every refusal below leave
        // the upload_id good for the next attempt.
        let staged = self
            .state
            .staging
            .peek(user_id, &upload_id)
            .map_err(|e| bad(e.to_string()))?;
        let read = drive::revisable(client, connection.id, file_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let file = read.file;
        let unchanged =
            format!("Nothing was changed, and the upload `{upload_id}` is still staged");
        if file.is_google_native() {
            return Err(refuse(format!(
                "{:?} is a {}, a Google file with no file content of its own to replace. A \
                 Google Doc is changed with the docs_* tools and a Sheet with the sheets_* \
                 tools. {unchanged}",
                file.name, file.mime_type
            )));
        }
        if !same_type(&staged.mime_type, &file.mime_type) {
            return Err(refuse(format!(
                "{:?} is {}, and the upload {:?} is {}. New content must have the type the \
                 file already has, because this tool converts nothing. {unchanged}. To keep \
                 both, store the upload as a new file with drive_upload",
                file.name, file.mime_type, staged.filename, staged.mime_type
            )));
        }
        let Some(head) = read.head_revision_id else {
            return Err(refuse(format!(
                "Drive reports no current version of {:?}, so this server cannot mark it keep \
                 forever, and it replaces no content it cannot keep. {unchanged}",
                file.name
            )));
        };
        // The history is read before the preview too, so a file at its limit
        // is refused before the person approves anything.
        let kept = match drive::kept_forever(client, connection.id, &file.id).await {
            Ok(kept) => kept,
            Err(e) if access_refused(&e) => {
                return Err(refuse(file_refusal(&file, &upload_id, &e.to_string())));
            }
            Err(e) => return Err(self.google_err_for(&connection, e)),
        };
        let already_kept = kept.ids.contains(&head);
        if !already_kept && kept.ids.len() >= drive::KEEP_FOREVER_MAX {
            return Err(refuse(limit_refusal(&file, kept.ids.len(), &upload_id)));
        }
        let new_name = p
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != file.name)
            .map(str::to_string);
        let size = staged.bytes.len();
        if !p.confirmed {
            let folder = self.folder_of(&connection, &file).await;
            let mut details = vec![
                format!("the file is {:?} ({}), in {folder}", file.name, file.id),
                format!(
                    "it is now {} of {}, last changed {}",
                    match file.size {
                        Some(n) => format!(
                            "{} ({n} bytes)",
                            uploads::megabytes(usize::try_from(n).unwrap_or(usize::MAX))
                        ),
                        None => "of a size Drive does not report".to_string(),
                    },
                    file.mime_type,
                    dto::instant(file.modified_time, call.tz)
                        .unwrap_or_else(|| "at a time Drive does not report".to_string())
                ),
                format!(
                    "the new content is {} ({size} bytes), uploaded as {:?}",
                    uploads::megabytes(size),
                    staged.filename
                ),
            ];
            if let Some(name) = &new_name {
                details.push(format!("the file is renamed to {name:?}"));
            }
            details.push(
                "the file keeps its id, its link and its folder; only its content changes"
                    .to_string(),
            );
            details.push(
                "the current version is kept forever in the file's version history in Drive, \
                 where the person finds it under Manage versions; it does not appear as a \
                 second file in the folder"
                    .to_string(),
            );
            details.push(
                "if Drive refuses to keep the current version, nothing is replaced and the \
                 upload stays staged"
                    .to_string(),
            );
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "replace the content of {:?} in Drive in `{}`, in {folder}",
                    file.name, connection.label
                ),
                details,
            ))));
        }
        // The old content is made safe first. Without the mark, Drive deletes
        // a version 30 days after newer content arrives, so new content over
        // an unmarked version would be a delete with a delay.
        match drive::keep_forever(client, connection.id, &file.id, &head).await {
            Ok(revision) if revision.keep_forever => {}
            Ok(_) => {
                return Err(refuse(format!(
                    "Drive answered the request to keep the current version of {:?} forever \
                     without marking it so, and this server replaces no content it cannot \
                     keep. {unchanged}",
                    file.name
                )));
            }
            Err(e) if access_refused(&e) => {
                return Err(refuse(file_refusal(&file, &upload_id, &e.to_string())));
            }
            Err(e) => {
                let mut note = format!(
                    "Drive did not mark the current version of {:?} keep forever, so nothing \
                     was replaced: new content over an unmarked version would leave the old \
                     content to be deleted after 30 days. The upload `{upload_id}` is still \
                     staged.",
                    file.name
                );
                if kept.more {
                    note.push_str(&format!(
                        " The file has more versions than one read lists, so it may have \
                         reached the limit of {} versions kept forever; the person can unpin \
                         or delete an old version under Manage versions in Drive.",
                        drive::KEEP_FOREVER_MAX
                    ));
                }
                return Err(with_note(self.google_err_for(&connection, e), &note));
            }
        }
        // The file's own type for the content part: the two were checked to
        // be the same, and Drive's spelling of it changes nothing.
        let replaced = match drive::replace(
            client,
            connection.id,
            &file.id,
            new_name.as_deref(),
            &file.mime_type,
            &staged.bytes,
        )
        .await
        {
            Ok(replaced) => replaced,
            Err(e) => {
                return Err(with_note(
                    self.google_err_for(&connection, e),
                    &format!(
                        "The current version {head} of {:?} is now marked keep forever, and \
                         nothing else changed: the file still has its old content and its old \
                         name. The mark is harmless and stays. The upload `{upload_id}` is \
                         still staged.",
                        file.name
                    ),
                ));
            }
        };
        // Drive has the new content, so the upload is spent now and not before.
        if let Err(e) = self
            .state
            .staging
            .take(user_id, std::slice::from_ref(&upload_id))
        {
            tracing::warn!("spending the upload {upload_id} after Drive took it: {e}");
        }
        let size = replaced.size.unwrap_or(size as u64);
        let mut written = format!(
            "replaced the content of {:?} with {}",
            file.name,
            uploads::megabytes(usize::try_from(size).unwrap_or(usize::MAX))
        );
        if new_name.is_some() {
            written.push_str(&format!(" and renamed it to {:?}", replaced.name));
        }
        written.push_str(&format!(
            "; the previous version {head} is kept forever in its version history, under \
             Manage versions in Drive"
        ));
        Ok(Json(Confirmable::Done(dto::DriveUpdateOut {
            account: connection.label,
            url: replaced
                .web_view_link
                .clone()
                .unwrap_or_else(|| format!("https://drive.google.com/file/d/{}/view", replaced.id)),
            written,
            file_id: replaced.id,
            name: replaced.name,
            mime_type: replaced.mime_type,
            size,
            modified_time: dto::instant(replaced.modified_time, call.tz),
            pinned_revision_id: head,
        })))
    }
}

impl Gmcp {
    /// The folder a file is in, by name, for a preview. A preview is not
    /// refused over a name it cannot read: it falls back to the id.
    async fn folder_of(&self, connection: &Connection, file: &drive::FileMeta) -> String {
        let Some(id) = file.parents.first() else {
            return "no folder this account can see".to_string();
        };
        let found = match self.google() {
            Ok(google) => drive::folder(&google.client, connection.id, id).await.ok(),
            Err(_) => None,
        };
        match found {
            Some(folder) => format!("the folder {:?}", folder.name),
            None => format!("the folder {id}"),
        }
    }
}

/// True when two MIME types name the same type: case and parameters such as
/// a charset do not count.
fn same_type(a: &str, b: &str) -> bool {
    let essence = |t: &str| {
        t.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    };
    essence(a) == essence(b)
}

/// What a caller is told when Drive refused the file to this server. Nothing
/// was changed, and the caller's next step is in the message.
fn file_refusal(file: &drive::FileMeta, upload_id: &str, google: &str) -> String {
    format!(
        "Drive refused to change the file {:?} ({}): it could not be written to with the access \
         this server holds. This server can change the files it put in Drive itself, and a file \
         the person put there by hand is usually not one of them. Nothing was replaced, and no \
         version was marked. The upload `{upload_id}` is still staged: drive_upload stores it as \
         a new file instead. ({google})",
        file.name, file.id
    )
}

/// What a caller is told when the file already has as many versions kept
/// forever as Drive allows.
fn limit_refusal(file: &drive::FileMeta, kept: usize, upload_id: &str) -> String {
    format!(
        "Drive keeps at most {} versions of one file forever, and {:?} already has {kept}. This \
         server replaces a file's content only after it has marked the current version keep \
         forever, so nothing was replaced, and the upload `{upload_id}` is still staged. To make \
         room, the person opens Manage versions for this file in Drive and unpins or deletes an \
         old version kept forever; this server does neither. To keep both, store the upload as \
         a new file with drive_upload",
        drive::KEEP_FOREVER_MAX,
        file.name
    )
}

impl Gmcp {
    /// The folder something is about to go into, read so the person
    /// approving the write sees its name and not only its id. An id that
    /// names no folder this account can see, names something that is not a
    /// folder, or names a folder in the bin is refused here, before anything
    /// is written.
    async fn destination(
        &self,
        connection: &Connection,
        id: &str,
        what: Putting,
    ) -> Result<drive::Folder, ErrorData> {
        let (arg, thing) = what.words();
        let found = drive::folder(&self.google()?.client, connection.id, id).await;
        let folder = match found {
            Ok(folder) => folder,
            Err(GoogleFailure::Google(g)) if g.status == 404 => {
                return Err(refuse(format!(
                    "there is no folder `{id}` that `{}` can see. A folder in a shared drive is \
                     out of reach of these tools. Leave `{arg}` out to put the {thing} in the \
                     root of {MY_DRIVE}",
                    connection.label
                )));
            }
            Err(e) => return Err(self.google_err_for(connection, e)),
        };
        if folder.mime_type != drive::FOLDER_MIME {
            return Err(refuse(format!(
                "`{id}` is {:?}, a {}, and not a folder; pass the id of a folder, or leave \
                 `{arg}` out to put the {thing} in the root of {MY_DRIVE}",
                folder.name, folder.mime_type
            )));
        }
        if folder.trashed {
            return Err(refuse(format!(
                "the folder {:?} is in the bin, and a {thing} put there would be in the bin \
                 too; pick another folder, or leave `{arg}` out to put the {thing} in the root \
                 of {MY_DRIVE}",
                folder.name
            )));
        }
        Ok(folder)
    }
}

/// What is about to go into a folder, so a refusal names the argument the
/// caller passed and the thing it was putting there.
#[derive(Debug, Clone, Copy)]
enum Putting {
    /// drive_upload's `folder`.
    File,
    /// drive_create_folder's `parent`.
    Folder,
}

impl Putting {
    fn words(self) -> (&'static str, &'static str) {
        match self {
            Putting::File => ("folder", "file"),
            Putting::Folder => ("parent", "folder"),
        }
    }
}

/// Where the person opens a folder: the link Drive gave, or the one every
/// folder has when Drive gave none.
fn folder_url(folder: &drive::FileMeta) -> String {
    folder
        .web_view_link
        .clone()
        .unwrap_or_else(|| format!("https://drive.google.com/drive/folders/{}", folder.id))
}

/// The ids of the folders a lookup found, oldest first, for a caller that has
/// to pick one.
fn ids(found: &drive::NamedFolders) -> String {
    let mut said: Vec<String> = found.folders.iter().map(|f| f.id.clone()).collect();
    if found.more {
        said.push("and more".to_string());
    }
    said.join(", ")
}

/// Where drive_upload puts a file, once the destination has been read and
/// before anything is written.
enum Destination {
    Root,
    /// A folder named by its id.
    Folder(drive::Folder),
    /// A folder named by its path from the root.
    Path(PathPlan),
}

impl Destination {
    fn describe(&self) -> String {
        match self {
            Destination::Root => format!("the root of {MY_DRIVE}"),
            Destination::Folder(f) => format!("the folder {:?} ({})", f.name, f.id),
            Destination::Path(plan) => format!(
                "the folder {:?} at {}, from the root of {MY_DRIVE}",
                plan.last(),
                plan.path
            ),
        }
    }
}

/// A `folder_path` resolved against Drive, before anything is created.
struct PathPlan {
    /// The path, its names joined with `/`.
    path: String,
    /// The folders already there, outermost first, each inside the one
    /// before and the first in the root.
    reused: Vec<drive::Folder>,
    /// The names of the folders still missing, outermost first. Each goes
    /// inside the one before, and the first inside the last reused folder.
    /// Once one name is missing, every name below it is too.
    missing: Vec<String>,
}

impl PathPlan {
    /// The name of the folder the file goes into.
    fn last(&self) -> &str {
        self.missing
            .last()
            .map(String::as_str)
            .or_else(|| self.reused.last().map(|f| f.name.as_str()))
            .unwrap_or(MY_DRIVE)
    }

    /// What the preview says about the path, one line per folder on it.
    fn preview(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let mut inside = format!("the root of {MY_DRIVE}");
        for folder in &self.reused {
            lines.push(format!(
                "{:?} (reused): the folder already in {inside}, {}",
                folder.name, folder.id
            ));
            inside = format!("the folder {:?}", folder.name);
        }
        for name in &self.missing {
            lines.push(format!(
                "{name:?} (new): this server creates it in {inside}"
            ));
            inside = format!("the folder {name:?}");
        }
        if let Some(last) = self.reused.last() {
            let write = if self.missing.is_empty() {
                "put the file into it"
            } else {
                "create a folder inside it"
            };
            lines.push(format!(
                "{:?} is reused, so this call did not create it. If the person made it by hand, \
                 Drive may not let this server {write}; then nothing more is created or \
                 uploaded, and the upload stays staged",
                last.name
            ));
        }
        if !self.missing.is_empty() {
            lines.push(
                "the new folders are created one at a time. If a later step fails, the folders \
                 already created stay in Drive, because this server deletes nothing there, and \
                 the answer names each of them with its id"
                    .to_string(),
            );
        }
        lines
    }
}

/// A folder a file is about to go into, by the two things a person and a
/// retry need: its name and its id.
struct Place {
    id: String,
    name: String,
}

/// A folder this call created on the way down a path, and where it put it.
struct Made {
    id: String,
    name: String,
    /// "the root of My Drive" or `the folder "topologia"`.
    inside: String,
}

/// The folder names in a `folder_path`, outermost first. One leading `/` is
/// allowed and changes nothing: a path always starts at the root of My Drive.
fn path_segments(raw: &str) -> Result<Vec<String>, ErrorData> {
    let path = raw.strip_prefix('/').unwrap_or(raw);
    if path.trim().is_empty() {
        return Err(bad(format!(
            "folder_path {raw:?} names no folder; leave it out to put the file in the root of \
             {MY_DRIVE}"
        )));
    }
    let mut segments = Vec::new();
    for segment in path.split('/').map(str::trim) {
        if segment.is_empty() {
            return Err(bad(format!(
                "folder_path {raw:?} has an empty folder name in it; write one name between \
                 each two `/`, as in topologia/notatki"
            )));
        }
        if segment == "." || segment == ".." {
            return Err(bad(format!(
                "folder_path {raw:?} has {segment:?} in it, and Drive takes that as the name of \
                 a folder, not as a step up; write the names of the folders from the root of \
                 {MY_DRIVE} down"
            )));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

impl Gmcp {
    /// Find which folders of a path are already there. Each name is looked
    /// up inside the folder before it, starting from the root of My Drive,
    /// and the lookups stop at the first name that is missing. Two folders
    /// of one name refuse the whole call before anything is created.
    async fn plan_path(
        &self,
        connection: &Connection,
        segments: Vec<String>,
        upload_id: &str,
    ) -> Result<PathPlan, ErrorData> {
        let client = &self.google()?.client;
        let path = segments.join("/");
        let mut reused: Vec<drive::Folder> = Vec::new();
        let mut names = segments.into_iter();
        while let Some(name) = names.next() {
            let parent = reused.last().map(|f| f.id.as_str());
            let found = drive::folders_named(client, connection.id, parent, &name)
                .await
                .map_err(|e| self.google_err_for(connection, e))?;
            if found.more || found.folders.len() > 1 {
                return Err(refuse(ambiguous(&name, reused.last(), &found, upload_id)));
            }
            match found.folders.into_iter().next() {
                Some(folder) => reused.push(folder),
                None => {
                    let mut missing = vec![name];
                    missing.extend(names);
                    return Ok(PathPlan {
                        path,
                        reused,
                        missing,
                    });
                }
            }
        }
        Ok(PathPlan {
            path,
            reused,
            missing: Vec::new(),
        })
    }

    /// Create the folders a path is missing, one at a time, each inside the
    /// one before. Answers the folder the file goes into and the folders this
    /// call created. A failure names every folder already created, with its
    /// id: they stay, because this server deletes nothing in Drive.
    async fn make_path(
        &self,
        connection: &Connection,
        plan: PathPlan,
        upload_id: &str,
    ) -> Result<(Place, Vec<Made>), ErrorData> {
        let client = &self.google()?.client;
        let mut parent = plan.reused.last().map(|f| Place {
            id: f.id.clone(),
            name: f.name.clone(),
        });
        let mut created: Vec<Made> = Vec::new();
        for name in plan.missing {
            let made = drive::create_folder(
                client,
                connection.id,
                &name,
                parent.as_ref().map(|f| f.id.as_str()),
            )
            .await;
            let made = match made {
                Ok(made) => made,
                Err(e) => {
                    let error = match &parent {
                        Some(f) if access_refused(&e) => {
                            refuse(path_refusal(f, &name, upload_id, &e.to_string()))
                        }
                        _ => with_note(
                            self.google_err_for(connection, e),
                            &format!(
                                "Nothing was uploaded, and the upload `{upload_id}` is still \
                                 staged."
                            ),
                        ),
                    };
                    return Err(with_note(error, &created_note(&created)));
                }
            };
            created.push(Made {
                id: made.id.clone(),
                name: made.name.clone(),
                inside: match &parent {
                    Some(f) => format!("the folder {:?}", f.name),
                    None => format!("the root of {MY_DRIVE}"),
                },
            });
            parent = Some(Place {
                id: made.id,
                name: made.name,
            });
        }
        let place = parent.ok_or_else(|| {
            ErrorData::internal_error("a folder_path resolved to no folder at all", None)
        })?;
        Ok((place, created))
    }
}

/// What a caller is told when two folders on a path share a name. Nothing
/// was created, and the ids let the caller name the one the person means.
fn ambiguous(
    name: &str,
    parent: Option<&drive::Folder>,
    found: &drive::NamedFolders,
    upload_id: &str,
) -> String {
    let count = if found.more {
        format!("more than {}", found.folders.len())
    } else {
        found.folders.len().to_string()
    };
    let place = match parent {
        Some(f) => format!("inside the folder {:?} ({})", f.name, f.id),
        None => format!("in the root of {MY_DRIVE}"),
    };
    format!(
        "there are {count} folders called {name:?} {place}: {}. Drive allows any number of \
         folders with one name in one place, and this server does not guess which one \
         folder_path means. Nothing was created and nothing was uploaded; the upload \
         `{upload_id}` is still staged. Ask the person which folder they mean, then pass its id \
         as `folder` and leave `folder_path` out. To go deeper below it, create the rest with \
         drive_create_folder first",
        ids(found)
    )
}

/// What a caller is told when Drive refused to create a folder of a path
/// inside a folder that was already there.
fn path_refusal(parent: &Place, name: &str, upload_id: &str, google: &str) -> String {
    format!(
        "Drive refused to create the folder {name:?} inside the folder {:?} ({}): that folder \
         could not be written to with the access this server holds. Nothing was uploaded, and \
         nothing was put anywhere else. The upload `{upload_id}` is still staged: make a folder \
         this server can write into with drive_create_folder and pass its id as `folder`, or \
         leave `folder_path` out to put the file in the root of {MY_DRIVE}. ({google})",
        parent.name, parent.id
    )
}

/// The sentence every failure on a path ends with: which folders this call
/// created before it stopped, so a retry does not create them twice.
fn created_note(created: &[Made]) -> String {
    if created.is_empty() {
        return "No folder was created.".to_string();
    }
    let list = created
        .iter()
        .map(|m| format!("{:?} ({}) in {}", m.name, m.id, m.inside))
        .collect::<Vec<_>>()
        .join(", ");
    let count = match created.len() {
        1 => "one folder".to_string(),
        n => format!("{n} folders"),
    };
    format!(
        "This call created {count} before it stopped, and {} in Drive, because this server \
         deletes nothing there: {list}. Call drive_upload again with the same folder_path and \
         it finds and reuses {}, or pass `folder` with the id of the last folder once the whole \
         path is there.",
        plural(created.len(), "it stays", "they stay"),
        plural(created.len(), "it", "them"),
    )
}

/// The names of the folders a call created, quoted and in order.
fn names(created: &[Made]) -> String {
    created
        .iter()
        .map(|m| format!("{:?}", m.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A refusal with one more sentence after it.
fn with_note(mut error: ErrorData, note: &str) -> ErrorData {
    let said = error.message.trim_end().trim_end_matches('.').to_string();
    error.message = format!("{said}. {note}").into();
    error
}

/// What a caller is told when Drive refused to create a folder inside the
/// parent it named. Nothing was created anywhere else.
fn parent_refusal(parent: &drive::Folder, name: &str, google: &str) -> String {
    format!(
        "Drive refused to create the folder {name:?} inside the folder {:?} ({}): that folder \
         could not be written to with the access this server holds. Nothing was created, and \
         no folder was made anywhere else. Leave `parent` out to create the folder in the root \
         of {MY_DRIVE}, where the person can move it. ({google})",
        parent.name, parent.id
    )
}

/// True when Drive refused this server the item it was writing to: the parent
/// folder of an upload, or the file whose content drive_update_file replaces.
/// These are the three answers the Drive reference gives for a file the app
/// may not write or cannot see: `notFound` (404), and
/// `insufficientFilePermissions` and `appNotAuthorizedToFile` (403). Every
/// other 403 — a full quota, a rate limit — is about the account and not the
/// item, and is passed through.
fn access_refused(e: &GoogleFailure) -> bool {
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
/// nowhere else, and the caller's next step is in the message. `arg` is the
/// argument that named the folder, `folder` or `folder_path`.
fn folder_refusal(folder: &Place, arg: &str, upload_id: &str, google: &str) -> String {
    format!(
        "Drive refused to put the file into the folder {:?} ({}): that folder could not be \
         written to with the access this server holds. Nothing was uploaded, and the file was \
         not put anywhere else. The upload `{upload_id}` is still staged: call drive_upload \
         again with the same upload_id and leave `{arg}` out to put the file in the root of \
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
