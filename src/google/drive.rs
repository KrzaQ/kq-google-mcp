//! Drive: search, metadata, download, export and the two file creations that
//! make a Doc out of markdown and a Sheet out of CSV.
//!
//! Shared drives are deliberately out of this release: every call goes against
//! the person's own corpus, so `supportsAllDrives` is never set and a file
//! someone else owns in a team drive simply is not found.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::client::{Client, Download, Error, Result};

/// What Google calls a Doc, a Sheet and a folder.
pub const DOCUMENT_MIME: &str = "application/vnd.google-apps.document";
pub const SPREADSHEET_MIME: &str = "application/vnd.google-apps.spreadsheet";
#[allow(dead_code)]
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// The fields a file is asked for. Drive returns almost nothing by default,
/// and asking for everything is both slower and noisier than this list.
const FILE_FIELDS: &str =
    "id,name,mimeType,modifiedTime,size,webViewLink,parents,owners(displayName,emailAddress)";

/// How many files a search returns when the caller does not say.
pub const DEFAULT_MAX_FILES: u32 = 25;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub modified_time: Option<DateTime<Utc>>,
    /// Google files (Docs, Sheets) have no size; uploads do.
    pub size: Option<u64>,
    pub owners: Vec<String>,
    pub web_view_link: Option<String>,
    pub parents: Vec<String>,
}

impl FileMeta {
    pub fn is_google_doc(&self) -> bool {
        self.mime_type == DOCUMENT_MIME
    }

    pub fn is_google_sheet(&self) -> bool {
        self.mime_type == SPREADSHEET_MIME
    }

    /// A Google file has no bytes of its own and must be exported rather than
    /// downloaded.
    pub fn is_google_native(&self) -> bool {
        self.mime_type.starts_with("application/vnd.google-apps.")
    }
}

/// The tool's search arguments, turned into one Drive `q` string.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Search {
    /// A raw Drive query, for a caller that knows the syntax.
    pub query: Option<String>,
    pub name_contains: Option<String>,
    pub mime_type: Option<String>,
    pub modified_after: Option<DateTime<Utc>>,
    pub max: Option<u32>,
}

impl Search {
    /// The `q` parameter. Trashed files are never listed; a tool that cannot
    /// trash anything has no business finding what is in the bin.
    pub fn to_query(&self) -> String {
        let mut clauses = vec!["trashed = false".to_string()];
        if let Some(name) = &self.name_contains {
            clauses.push(format!("name contains '{}'", escape(name)));
        }
        if let Some(mime) = &self.mime_type {
            clauses.push(format!("mimeType = '{}'", escape(mime)));
        }
        if let Some(after) = &self.modified_after {
            clauses.push(format!(
                "modifiedTime > '{}'",
                after.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ));
        }
        if let Some(query) = self.query.as_deref().map(str::trim)
            && !query.is_empty()
        {
            clauses.push(format!("({query})"));
        }
        clauses.join(" and ")
    }
}

/// Drive's query language quotes with single quotes and escapes with
/// backslashes; a file called `Bob's` must not end the string.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// What a Doc or a Sheet can be turned into. Docs and Sheets export to
/// different things, and [`ExportFormat::allowed_for`] is what refuses the
/// wrong pairing before Google does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Pdf,
    Docx,
    Xlsx,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 4] = [
        ExportFormat::Markdown,
        ExportFormat::Pdf,
        ExportFormat::Docx,
        ExportFormat::Xlsx,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "markdown",
            ExportFormat::Pdf => "pdf",
            ExportFormat::Docx => "docx",
            ExportFormat::Xlsx => "xlsx",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "text/markdown",
            ExportFormat::Pdf => "application/pdf",
            ExportFormat::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            ExportFormat::Xlsx => {
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            }
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Pdf => "pdf",
            ExportFormat::Docx => "docx",
            ExportFormat::Xlsx => "xlsx",
        }
    }

    /// The formats a file of this type exports to, in the order the tool
    /// description lists them.
    pub fn allowed_for(mime_type: &str) -> &'static [ExportFormat] {
        match mime_type {
            DOCUMENT_MIME => &[
                ExportFormat::Markdown,
                ExportFormat::Pdf,
                ExportFormat::Docx,
            ],
            SPREADSHEET_MIME => &[ExportFormat::Xlsx, ExportFormat::Pdf],
            _ => &[],
        }
    }
}

impl std::fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown export format {0:?}; use markdown, pdf, docx or xlsx")]
pub struct ExportFormatError(pub String);

impl std::str::FromStr for ExportFormat {
    type Err = ExportFormatError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        ExportFormat::ALL
            .into_iter()
            .find(|f| f.as_str() == s.trim().to_ascii_lowercase())
            .ok_or_else(|| ExportFormatError(s.to_string()))
    }
}

/// `files.list`.
pub async fn list(client: &Client, connection_id: i64, search: &Search) -> Result<Vec<FileMeta>> {
    let request = client.get("drive/v3/files").query(&[
        ("q", search.to_query()),
        ("fields", format!("files({FILE_FIELDS}),nextPageToken")),
        (
            "pageSize",
            search.max.unwrap_or(DEFAULT_MAX_FILES).to_string(),
        ),
        ("orderBy", "modifiedTime desc".to_string()),
    ]);
    let wire: WireFileList = client.json(connection_id, request).await?;
    Ok(wire.files.into_iter().map(Into::into).collect())
}

/// `files.get`, metadata only.
pub async fn get(client: &Client, connection_id: i64, file_id: &str) -> Result<FileMeta> {
    let request = client
        .get(&format!("drive/v3/files/{file_id}"))
        .query(&[("fields", FILE_FIELDS)]);
    let wire: WireFile = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `files.get?alt=media`, the bytes of a file that has bytes. The response is
/// handed back unread so the download route can stream it.
pub async fn download(client: &Client, connection_id: i64, file_id: &str) -> Result<Download> {
    let request = client
        .get(&format!("drive/v3/files/{file_id}"))
        .query(&[("alt", "media")]);
    client.download(connection_id, request).await
}

/// `files.export`, for the Google-native files that have no bytes of their
/// own. Also streamed.
pub async fn export(
    client: &Client,
    connection_id: i64,
    file_id: &str,
    format: ExportFormat,
) -> Result<Download> {
    let request = client
        .get(&format!("drive/v3/files/{file_id}/export"))
        .query(&[("mimeType", format.mime_type())]);
    client.download(connection_id, request).await
}

/// A markdown document, imported as a Google Doc. This is the only way to
/// make a Doc with content in one call: `files.create` converts the uploaded
/// `text/markdown` body into Docs' own format.
pub async fn create_doc_from_markdown(
    client: &Client,
    connection_id: i64,
    title: &str,
    markdown: &str,
    folder_id: Option<&str>,
) -> Result<FileMeta> {
    create(
        client,
        connection_id,
        title,
        DOCUMENT_MIME,
        "text/markdown",
        markdown.as_bytes(),
        folder_id,
    )
    .await
}

/// The same for a Sheet, from CSV.
pub async fn create_sheet_from_csv(
    client: &Client,
    connection_id: i64,
    title: &str,
    csv: &str,
    folder_id: Option<&str>,
) -> Result<FileMeta> {
    create(
        client,
        connection_id,
        title,
        SPREADSHEET_MIME,
        "text/csv",
        csv.as_bytes(),
        folder_id,
    )
    .await
}

/// `files.create` with `uploadType=multipart`: a JSON metadata part and a
/// content part in one `multipart/related` body, which is what makes Drive
/// convert the content into the target type.
async fn create(
    client: &Client,
    connection_id: i64,
    title: &str,
    target_mime: &str,
    source_mime: &str,
    content: &[u8],
    folder_id: Option<&str>,
) -> Result<FileMeta> {
    let metadata = serde_json::json!({
        "name": title,
        "mimeType": target_mime,
        "parents": folder_id.map(|id| vec![id]).unwrap_or_default(),
    });
    let boundary = boundary();
    let body = multipart_related(&boundary, &metadata.to_string(), source_mime, content);
    let request = client
        .post("upload/drive/v3/files")
        .query(&[("uploadType", "multipart"), ("fields", FILE_FIELDS)])
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/related; boundary={boundary}"),
        )
        .body(body);
    let wire: WireFile = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// The two parts, with CRLF line endings as the format requires.
fn multipart_related(
    boundary: &str,
    metadata: &str,
    content_mime: &str,
    content: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + metadata.len() + 256);
    let mut push = |s: &str| body.extend_from_slice(s.as_bytes());
    push(&format!("--{boundary}\r\n"));
    push("Content-Type: application/json; charset=UTF-8\r\n\r\n");
    push(metadata);
    push(&format!("\r\n--{boundary}\r\n"));
    push(&format!(
        "Content-Type: {content_mime}; charset=UTF-8\r\n\r\n"
    ));
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// A boundary that cannot occur in the content. Random rather than fixed
/// because the content is whatever a model wrote.
fn boundary() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("os randomness");
    format!("gmcp{}", hex::encode(bytes))
}

/// Refuse a format the file cannot produce, with the ones it can.
pub fn check_export_format(file: &FileMeta, format: ExportFormat) -> Result<()> {
    let allowed = ExportFormat::allowed_for(&file.mime_type);
    if allowed.contains(&format) {
        return Ok(());
    }
    if allowed.is_empty() {
        return Err(Error::Unsupported(format!(
            "{} is not a Google Doc or Sheet, so it is downloaded rather than exported",
            file.name
        )));
    }
    Err(Error::Unsupported(format!(
        "{} cannot be exported as {format}; it exports as {}",
        file.name,
        allowed
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireFileList {
    files: Vec<WireFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireFile {
    id: String,
    name: String,
    mime_type: String,
    modified_time: Option<DateTime<Utc>>,
    /// Drive sends sizes as strings, because they can exceed 2^53.
    size: Option<String>,
    web_view_link: Option<String>,
    parents: Vec<String>,
    owners: Vec<WireOwner>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireOwner {
    display_name: Option<String>,
    email_address: Option<String>,
}

impl From<WireFile> for FileMeta {
    fn from(w: WireFile) -> Self {
        FileMeta {
            id: w.id,
            name: w.name,
            mime_type: w.mime_type,
            modified_time: w.modified_time,
            size: w.size.and_then(|s| s.parse().ok()),
            owners: w
                .owners
                .into_iter()
                .filter_map(|o| o.email_address.or(o.display_name))
                .collect(),
            web_view_link: w.web_view_link,
            parents: w.parents,
        }
    }
}
