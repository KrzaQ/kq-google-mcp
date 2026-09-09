//! The Drive tools: finding files, describing them, and getting at their
//! contents either as text a model can read or as a short-lived link a person
//! can click. Nothing here writes to Drive; the two tools that create files
//! live in `docs` and `sheets`, where they need confirmation.

use chrono::{DateTime, Utc};
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
    /// Only files changed after this instant, RFC 3339
    pub modified_after: Option<String>,
    /// How many files to return; default 20, at most 100
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileParam {
    pub account: String,
    pub file_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExportParam {
    pub account: String,
    pub file_id: String,
    /// "markdown", "pdf" or "docx" for a Google Doc; "xlsx" or "pdf" for a Sheet
    pub format: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
        let search = drive::Search {
            query: p.query,
            name_contains: p.name_contains,
            mime_type: p.mime_type,
            modified_after: p.modified_after.as_deref().map(instant).transpose()?,
            max: Some(capped(p.max, 20, 100)),
        };
        let files = drive::list(&self.google()?.client, connection.id, &search)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::FilesOut {
            account: connection.label,
            count: files.len(),
            files: files.into_iter().map(Into::into).collect(),
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
        Ok(Json(file.into()))
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
        Ok(Json(link_out(minted)))
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
        Ok(Json(link_out(minted)))
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
}

/// An RFC 3339 instant from an argument, refused by name rather than by
/// whatever chrono says.
pub(super) fn instant(value: &str) -> Result<DateTime<Utc>, ErrorData> {
    DateTime::parse_from_rfc3339(value.trim())
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| {
            bad(format!(
                "{value:?} is not an RFC 3339 instant; write it as 2026-09-08T14:00:00Z"
            ))
        })
}
