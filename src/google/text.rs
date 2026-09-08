//! Turning an attachment or a Drive file into text a chat model can read,
//! without a download link and without a vision model.
//!
//! Five sources, per the plan: plain text and CSV as they are, PDF through
//! poppler's `pdftotext`, DOCX by reading `word/document.xml` out of the zip,
//! a Google Doc through Drive's markdown export, and a Google Sheet through
//! the Sheets API as CSV per tab. Everything else is refused with a message
//! that names the type, because a link is the honest answer for it.
//!
//! `pdftotext` is looked for once, at startup, so `/api/health` can report it
//! and a tool call does not pay for the search. Its absence is a typed error
//! and never a silent empty result.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Serialize;

use super::client::{Client, Error, Result};
use super::{drive, sheets};
use crate::domain::limits::TEXT_MAX_CHARS;

/// The poppler binary this module shells out to.
pub const PDFTOTEXT: &str = "pdftotext";
const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PDF_MIME: &str = "application/pdf";
/// Where the paragraph text lives inside a .docx.
const DOCX_DOCUMENT: &str = "word/document.xml";

/// Extracted text, with what was left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extraction {
    pub text: String,
    /// How many characters the cap removed; zero when nothing was cut.
    pub truncated_chars: usize,
    /// How the text was obtained, for the tool to say so.
    pub source: &'static str,
}

impl Extraction {
    /// Cut to the cap, with a last line saying how much went missing. The
    /// notice is part of the text because that is the only place a model
    /// reliably reads it.
    pub fn new(text: String, source: &'static str) -> Self {
        Self::capped(text, source, TEXT_MAX_CHARS)
    }

    fn capped(text: String, source: &'static str, max_chars: usize) -> Self {
        let total = text.chars().count();
        if total <= max_chars {
            return Extraction {
                text,
                truncated_chars: 0,
                source,
            };
        }
        let kept: String = text.chars().take(max_chars).collect();
        let cut = total - max_chars;
        Extraction {
            text: format!("{kept}\n\n[… {cut} more characters were cut off here]"),
            truncated_chars: cut,
            source,
        }
    }
}

/// Where `pdftotext` was found, looked for once per process.
static PDFTOTEXT_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Prime the lookup. `serve` calls this at startup so the first tool call and
/// `/api/health` both answer from the same, already-made decision.
pub fn init() {
    let _ = pdftotext_path();
}

fn pdftotext_path() -> Option<&'static Path> {
    PDFTOTEXT_PATH.get_or_init(|| which(PDFTOTEXT)).as_deref()
}

/// What `/api/health` reports. False means PDF extraction answers with an
/// error that says poppler is missing, and everything else still works.
pub fn pdftotext_available() -> bool {
    pdftotext_path().is_some()
}

/// The extractor a request uses. It holds the resolved binary so a test can
/// hand it a name that is not installed and see the error, rather than the
/// suite depending on what the machine happens to have.
#[derive(Debug, Clone)]
pub struct Extractor {
    pdftotext: Option<PathBuf>,
}

impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor {
    /// The process-wide lookup, done once.
    pub fn new() -> Self {
        Self {
            pdftotext: pdftotext_path().map(Path::to_path_buf),
        }
    }

    /// An extractor that looks for a named program instead of the real one.
    /// This is how the absence of poppler is tested: point it at a name that
    /// is not on `PATH` and nothing on the machine has to change.
    pub fn with_program(program: &str) -> Self {
        Self {
            pdftotext: which(program),
        }
    }

    pub fn pdftotext_available(&self) -> bool {
        self.pdftotext.is_some()
    }

    /// Bytes with a declared type, as text. The filename is only used when
    /// the type is missing or useless, which is common for Gmail attachments
    /// sent by older clients.
    pub async fn bytes(&self, mime_type: &str, filename: &str, bytes: &[u8]) -> Result<Extraction> {
        let mime = effective_mime(mime_type, filename);
        if mime == PDF_MIME {
            return Ok(Extraction::new(self.pdf(bytes).await?, "pdftotext"));
        }
        if mime == DOCX_MIME {
            return Ok(Extraction::new(docx(bytes)?, "docx"));
        }
        if mime.starts_with("text/")
            || mime == "application/json"
            || mime == "application/xml"
            || mime == "application/csv"
        {
            return Ok(Extraction::new(lossy(bytes), "text"));
        }
        Err(Error::Unsupported(format!(
            "{mime} is not a type this server extracts text from; use the download link instead"
        )))
    }

    /// PDF through `pdftotext -layout`, reading the file on standard input
    /// and writing to standard output, so nothing touches the disk.
    async fn pdf(&self, bytes: &[u8]) -> Result<String> {
        use tokio::io::AsyncWriteExt;

        let program = self.pdftotext.as_ref().ok_or(Error::PdftotextMissing)?;
        let mut child = tokio::process::Command::new(program)
            .args(["-layout", "-enc", "UTF-8", "-", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Malformed(format!("could not run {}: {e}", program.display())))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Malformed("pdftotext took no input".into()))?;
        stdin
            .write_all(bytes)
            .await
            .map_err(|e| Error::Malformed(format!("writing the PDF to pdftotext: {e}")))?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| Error::Malformed(format!("waiting for pdftotext: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Malformed(format!(
                "pdftotext could not read this PDF: {}",
                stderr.trim().chars().take(200).collect::<String>()
            )));
        }
        Ok(lossy(&output.stdout))
    }
}

/// A Gmail attachment, fetched and extracted.
pub async fn gmail_attachment(
    client: &Client,
    connection_id: i64,
    extractor: &Extractor,
    message_id: &str,
    attachment: &super::gmail::Attachment,
) -> Result<Extraction> {
    let bytes =
        super::gmail::get_attachment(client, connection_id, message_id, &attachment.id).await?;
    extractor
        .bytes(&attachment.mime_type, &attachment.filename, &bytes)
        .await
}

/// A Drive file, by whichever of the five routes its type calls for.
pub async fn drive_file(
    client: &Client,
    connection_id: i64,
    extractor: &Extractor,
    file: &drive::FileMeta,
) -> Result<Extraction> {
    if file.is_google_doc() {
        return Ok(Extraction::new(
            google_doc(client, connection_id, &file.id).await?,
            "google-doc",
        ));
    }
    if file.is_google_sheet() {
        return Ok(Extraction::new(
            google_sheet(client, connection_id, &file.id).await?,
            "google-sheet",
        ));
    }
    if file.is_google_native() {
        return Err(Error::Unsupported(format!(
            "{} is a {} and this server does not read those; open it in Drive",
            file.name, file.mime_type
        )));
    }
    let bytes = drive::download(client, connection_id, &file.id)
        .await?
        .collect()
        .await?;
    extractor.bytes(&file.mime_type, &file.name, &bytes).await
}

/// A Google Doc, as markdown, through Drive's export. Docs' own API has no
/// text output, and markdown keeps headings and lists a model can use.
pub async fn google_doc(client: &Client, connection_id: i64, file_id: &str) -> Result<String> {
    let bytes = drive::export(
        client,
        connection_id,
        file_id,
        drive::ExportFormat::Markdown,
    )
    .await?
    .collect()
    .await?;
    Ok(lossy(&bytes))
}

/// A Google Sheet, as CSV per tab, through the Sheets API. Drive's XLSX
/// export would be a binary a model cannot read; the values endpoint gives
/// the formatted cells directly.
pub async fn google_sheet(client: &Client, connection_id: i64, file_id: &str) -> Result<String> {
    let spreadsheet = sheets::get(client, connection_id, file_id).await?;
    let mut out = String::new();
    for tab in &spreadsheet.tabs {
        let rows = sheets::values_get(client, connection_id, file_id, &tab.title, None).await?;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("## {}\n\n", tab.title));
        out.push_str(&to_csv(&rows)?);
    }
    Ok(out)
}

/// Rows as CSV. The `csv` crate does the quoting, so a cell containing a
/// comma, a quote or a newline survives the trip.
pub fn to_csv(rows: &[Vec<String>]) -> Result<String> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    for row in rows {
        writer
            .write_record(row)
            .map_err(|e| Error::Malformed(format!("writing CSV: {e}")))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| Error::Malformed(format!("writing CSV: {e}")))?;
    String::from_utf8(bytes).map_err(|e| Error::Malformed(format!("writing CSV: {e}")))
}

/// DOCX: the paragraphs of `word/document.xml`, in order. A .docx is a zip,
/// and its main part is XML whose `<w:t>` runs carry the text and whose
/// `<w:p>` elements are the paragraphs.
pub fn docx(bytes: &[u8]) -> Result<String> {
    use quick_xml::events::Event as XmlEvent;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| Error::Malformed(format!("this is not a readable .docx: {e}")))?;
    let mut document = archive.by_name(DOCX_DOCUMENT).map_err(|_| {
        Error::Malformed(format!(
            "the .docx has no {DOCX_DOCUMENT}; it may be a .doc"
        ))
    })?;
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut document, &mut xml)
        .map_err(|e| Error::Malformed(format!("reading {DOCX_DOCUMENT}: {e}")))?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut paragraph = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) => match e.name().local_name().as_ref() {
                "t" => in_text = true,
                "tab" => paragraph.push('\t'),
                _ => {}
            },
            Ok(XmlEvent::Empty(e)) => match e.name().local_name().as_ref() {
                "br" | "cr" => paragraph.push('\n'),
                "tab" => paragraph.push('\t'),
                // An empty paragraph is a blank line in the document.
                "p" => {
                    out.push_str(paragraph.trim_end());
                    out.push('\n');
                    paragraph.clear();
                }
                _ => {}
            },
            Ok(XmlEvent::Text(e)) if in_text => {
                paragraph.push_str(&e.xml10_content());
            }
            // An entity reference is its own event; `&amp;` in a run arrives
            // here as the name `amp`, not as part of the text around it.
            Ok(XmlEvent::GeneralRef(e)) if in_text => {
                let entity = format!("&{};", e.xml10_content());
                match quick_xml::escape::unescape(&entity) {
                    Ok(text) => paragraph.push_str(&text),
                    Err(_) => paragraph.push_str(&entity),
                }
            }
            Ok(XmlEvent::End(e)) => match e.name().local_name().as_ref() {
                "t" => in_text = false,
                "p" => {
                    out.push_str(paragraph.trim_end());
                    out.push('\n');
                    paragraph.clear();
                }
                _ => {}
            },
            Ok(XmlEvent::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(Error::Malformed(format!("{DOCX_DOCUMENT} is not XML: {e}")));
            }
        }
    }
    if !paragraph.trim().is_empty() {
        out.push_str(paragraph.trim_end());
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

/// UTF-8 where it is UTF-8, and the lossy decode where it is not. An
/// attachment written by anything is still worth reading.
fn lossy(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// What to treat the bytes as. Mail clients send `application/octet-stream`
/// for everything, so the extension gets a say when the type says nothing.
fn effective_mime(mime_type: &str, filename: &str) -> String {
    let mime = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let useless =
        mime.is_empty() || mime == "application/octet-stream" || mime == "binary/octet-stream";
    if !useless {
        return mime;
    }
    mime_guess::from_path(filename)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_ascii_lowercase()
}

/// `which`, without a crate for it: an absolute or relative path is taken as
/// given, a bare name is looked for on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return executable(candidate).then(|| candidate.to_path_buf());
    }
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|candidate| executable(candidate))
        })
        .unwrap_or_default()
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
