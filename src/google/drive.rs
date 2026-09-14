//! Drive: search, metadata, download, export and the two file creations that
//! make a Doc out of markdown and a Sheet out of CSV.
//!
//! Shared drives are deliberately out of this release: every call goes against
//! the person's own corpus, so `supportsAllDrives` is never set and a file
//! someone else owns in a team drive simply is not found.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::client::{Client, Download, Error, Result, urlencode};
use super::multipart;

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

/// The fields a comment thread is asked for. Two of them are the reason this
/// list exists at all: the default projection carries neither the replies nor
/// `quotedFileContent`, which is the text the comment is anchored to. Drive
/// also refuses `comments.list` outright when no `fields` is given.
const COMMENT_FIELDS: &str = "id,createdTime,modifiedTime,resolved,\
     author(displayName,emailAddress),content,quotedFileContent(value),\
     replies(id,createdTime,author(displayName,emailAddress),content)";

/// How many comment threads one page carries. Drive's own maximum, asked for
/// whatever the caller wants, because the resolved threads are dropped after
/// the page arrives and a small page would hide the open ones behind them.
const COMMENT_PAGE_SIZE: u32 = 100;

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
    let request = client.get("drive/v3/files")?.query(&[
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
        .get(&format!("drive/v3/files/{}", urlencode(file_id)))?
        .query(&[("fields", FILE_FIELDS)]);
    let wire: WireFile = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// One comment thread in the margin of a file, with the replies under it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comment {
    pub id: String,
    /// Who wrote it, as an address where Google gives one and as the display
    /// name otherwise, which is how a file's owners are reported too.
    pub author: Option<String>,
    pub created_time: Option<DateTime<Utc>>,
    pub modified_time: Option<DateTime<Utc>>,
    pub text: String,
    /// The text of the file the comment is anchored to. Absent on a comment
    /// somebody left on the file as a whole.
    pub quoted_text: Option<String>,
    /// True when one of the replies resolved the thread.
    pub resolved: bool,
    pub replies: Vec<Reply>,
}

/// One reply under a comment, oldest first as Drive answers them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reply {
    pub id: String,
    pub author: Option<String>,
    pub created_time: Option<DateTime<Utc>>,
    pub text: String,
}

/// One page of comment threads, and whether Drive had more to give.
#[derive(Debug, Clone, PartialEq)]
pub struct Comments {
    pub threads: Vec<Comment>,
    /// True when the file has more threads than the one page this read. The
    /// tool says so rather than letting a model believe it has the whole
    /// margin.
    pub more: bool,
}

/// `comments.list`: the margin of a file, read and never written. Deleted
/// comments are left out, because Google answers them with their content
/// stripped and a thread with no text in it is nothing to read.
pub async fn comments(client: &Client, connection_id: i64, file_id: &str) -> Result<Comments> {
    let request = client
        .get(&format!("drive/v3/files/{}/comments", urlencode(file_id)))?
        .query(&[
            (
                "fields",
                format!("comments({COMMENT_FIELDS}),nextPageToken"),
            ),
            ("pageSize", COMMENT_PAGE_SIZE.to_string()),
        ]);
    let wire: WireCommentList = client.json(connection_id, request).await?;
    Ok(Comments {
        more: wire.next_page_token.is_some(),
        threads: wire.comments.into_iter().map(Into::into).collect(),
    })
}

/// `files.get?alt=media`, the bytes of a file that has bytes. The response is
/// handed back unread so the download route can stream it.
pub async fn download(client: &Client, connection_id: i64, file_id: &str) -> Result<Download> {
    let request = client
        .get(&format!("drive/v3/files/{}", urlencode(file_id)))?
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
        .get(&format!("drive/v3/files/{}/export", urlencode(file_id)))?
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
    let boundary = multipart::boundary();
    let body = multipart::related(
        &boundary,
        &metadata.to_string(),
        &format!("{source_mime}; charset=UTF-8"),
        content,
    );
    let request = client
        .post("upload/drive/v3/files")?
        .query(&[("uploadType", "multipart"), ("fields", FILE_FIELDS)])
        .header(reqwest::header::CONTENT_TYPE, multipart::header(&boundary))
        .body(body);
    let wire: WireFile = client.json(connection_id, request).await?;
    Ok(wire.into())
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

impl WireOwner {
    /// The address if Google gave one, and the display name otherwise.
    fn name(self) -> Option<String> {
        self.email_address.or(self.display_name)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireCommentList {
    comments: Vec<WireComment>,
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireComment {
    id: String,
    created_time: Option<DateTime<Utc>>,
    modified_time: Option<DateTime<Utc>>,
    resolved: bool,
    author: Option<WireOwner>,
    content: String,
    quoted_file_content: Option<WireQuoted>,
    replies: Vec<WireReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireQuoted {
    value: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReply {
    id: String,
    created_time: Option<DateTime<Utc>>,
    author: Option<WireOwner>,
    content: String,
}

impl From<WireComment> for Comment {
    fn from(w: WireComment) -> Self {
        Comment {
            id: w.id,
            author: w.author.and_then(WireOwner::name),
            created_time: w.created_time,
            modified_time: w.modified_time,
            text: w.content,
            quoted_text: w.quoted_file_content.map(|q| q.value),
            resolved: w.resolved,
            replies: w.replies.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<WireReply> for Reply {
    fn from(w: WireReply) -> Self {
        Reply {
            id: w.id,
            author: w.author.and_then(WireOwner::name),
            created_time: w.created_time,
            text: w.content,
        }
    }
}

impl From<WireFile> for FileMeta {
    fn from(w: WireFile) -> Self {
        FileMeta {
            id: w.id,
            name: w.name,
            mime_type: w.mime_type,
            modified_time: w.modified_time,
            size: w.size.and_then(|s| s.parse().ok()),
            owners: w.owners.into_iter().filter_map(WireOwner::name).collect(),
            web_view_link: w.web_view_link,
            parents: w.parents,
        }
    }
}
