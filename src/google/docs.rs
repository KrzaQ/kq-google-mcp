//! Docs: a document read as numbered paragraphs, plus the two edits this
//! release makes, text appended at the end and a find-and-replace across the
//! document. The prose read goes through Drive's markdown export;
//! `documents.get` is what says where each paragraph starts and ends, and it
//! carries the pictures and the tab list too.
//!
//! Docs counts positions in UTF-16 code units, which Rust counts in neither,
//! so every index is computed in [`index`] and nowhere else.
//!
//! Structural edits are deliberately out: a model that can only append and
//! replace cannot rearrange someone's document by accident.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::client::{Client, Download, Error, Result, urlencode};

/// Docs is served from its own host, never from `www.googleapis.com`.
const DOCS: &str = "docs";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Document {
    pub document_id: String,
    pub title: String,
    /// Where the body ends. An insert goes one before it, because the last
    /// index is the newline Docs keeps at the end of the body and nothing may
    /// be written after it.
    pub end_index: i64,
    pub tabs: Vec<Tab>,
}

impl Document {
    /// The index `insertText` may write at.
    pub fn append_index(&self) -> i64 {
        (self.end_index - 1).max(1)
    }

    pub fn url(&self) -> String {
        format!(
            "https://docs.google.com/document/d/{}/edit",
            self.document_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tab {
    pub id: String,
    pub title: String,
}

/// A picture the document holds, in the order the body meets it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InlineImage {
    /// `image1`, `image2`, and so on. These are the labels Drive's markdown
    /// export writes, so they are the labels a model has already read in the
    /// text of the document, and it can name a picture without looking one up.
    pub label: String,
    /// Docs' own id for the object. A download link stores this, because a
    /// label moves when somebody adds a picture above it.
    pub object_id: String,
    pub alt_title: Option<String>,
    pub alt_text: Option<String>,
    /// How large the picture is *in the document*, in points. Docs says
    /// nothing about how many bytes it is; only fetching it answers that.
    pub width_pt: Option<i64>,
    pub height_pt: Option<i64>,
    /// Where the bytes are. Google's own URL, good for about half an hour,
    /// and absent for a drawing or a chart, which has no picture to fetch.
    pub content_uri: Option<String>,
}

impl InlineImage {
    /// What the picture is called once it has left the document. Docs gives a
    /// picture no name, so the document's title and the label are the only two
    /// things that tell the person who downloads it what they have.
    pub fn filename(&self, title: &str, mime_type: Option<&str>) -> String {
        let stem = match title.trim() {
            "" => self.label.clone(),
            title => format!("{title} {}", self.label),
        };
        format!("{stem}.{}", extension(mime_type))
    }
}

/// The file extension for what Google said the bytes are.
fn extension(mime_type: Option<&str>) -> &'static str {
    match mime_type.unwrap_or_default() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// What a document holds in the way of pictures.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocumentImages {
    pub document_id: String,
    pub title: String,
    pub images: Vec<InlineImage>,
}

impl DocumentImages {
    /// The picture a caller named, by label or by object id. The refusal lists
    /// what the document does hold, so the next call can be right; the tools
    /// and the download route both answer in these words.
    pub fn find(&self, wanted: &str) -> Result<&InlineImage> {
        let wanted = wanted.trim();
        self.images
            .iter()
            .find(|i| i.label.eq_ignore_ascii_case(wanted) || i.object_id == wanted)
            .ok_or_else(|| {
                let known: Vec<&str> = self.images.iter().map(|i| i.label.as_str()).collect();
                Error::Unsupported(format!(
                    "the document {:?} has no picture {wanted:?}; it has {}",
                    self.title,
                    if known.is_empty() {
                        "none at all".to_string()
                    } else {
                        known.join(", ")
                    }
                ))
            })
    }
}

/// The pictures a document holds, in the order they appear in it.
pub async fn images(
    client: &Client,
    connection_id: i64,
    document_id: &str,
) -> Result<DocumentImages> {
    let wire = fetch(client, connection_id, document_id).await?;
    Ok(DocumentImages {
        images: inline_images(&wire),
        document_id: wire.document_id,
        title: wire.title,
    })
}

/// The bytes of one picture, from the `contentUri` Google just answered with.
/// Streamed, so the download route never holds a picture in memory.
pub async fn open_image(client: &Client, image: &InlineImage) -> Result<Download> {
    let uri = image
        .content_uri
        .as_deref()
        .filter(|uri| !uri.is_empty())
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "{} is a drawing or a chart rather than a picture, so there are no image bytes \
                 to fetch; open the document to see it",
                image.label
            ))
        })?;
    client.follow_content_uri(uri).await
}

/// The pictures, labelled in the order the body meets them.
///
/// `inlineObjects` is a JSON object keyed by object id, and that order means
/// nothing at all. The labels must follow the body, because `image1` is what
/// the markdown export calls the first picture in the document, and that
/// export is the text a model has already read. So the body is walked instead,
/// tables included, and each reference is taken where it is met.
fn inline_images(wire: &WireDocument) -> Vec<InlineImage> {
    let mut referenced = Vec::new();
    walk(&wire.body.content, &mut referenced);
    let mut images: Vec<InlineImage> = Vec::new();
    for object_id in referenced {
        // A reference to an object the document does not describe is nothing
        // this server can show or fetch, so it is not given a label either.
        let Some(object) = wire.inline_objects.get(&object_id) else {
            continue;
        };
        let embedded = &object.inline_object_properties.embedded_object;
        images.push(InlineImage {
            label: format!("image{}", images.len() + 1),
            object_id,
            alt_title: some(&embedded.title),
            alt_text: some(&embedded.description),
            width_pt: embedded.size.width.magnitude.map(round),
            height_pt: embedded.size.height.magnitude.map(round),
            content_uri: some(&embedded.image_properties.content_uri),
        });
    }
    images
}

/// Every `inlineObjectElement` under this content, in reading order. A table
/// carries content of its own, so the walk goes through its cells.
fn walk(content: &[WireElement], out: &mut Vec<String>) {
    for element in content {
        for run in element.paragraph.iter().flat_map(|p| &p.elements) {
            if let Some(inline) = &run.inline_object_element {
                out.push(inline.inline_object_id.clone());
            }
        }
        for row in element.table.iter().flat_map(|t| &t.table_rows) {
            for cell in &row.table_cells {
                walk(&cell.content, out);
            }
        }
    }
}

fn some(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn round(magnitude: f64) -> i64 {
    magnitude.round() as i64
}

/// `documents.get`. The body content is asked for because the end index is
/// the last element's and the pictures are found by walking it, and the tab
/// list because the read tool reports it.
async fn fetch(client: &Client, connection_id: i64, document_id: &str) -> Result<WireDocument> {
    let request = client
        .service(DOCS)
        .get(&format!("v1/documents/{}", urlencode(document_id)))?
        .query(&[("includeTabsContent", "false")]);
    client.json(connection_id, request).await
}

/// The document's title, end index and tabs.
pub async fn get(client: &Client, connection_id: i64, document_id: &str) -> Result<Document> {
    let wire = fetch(client, connection_id, document_id).await?;
    let end_index = wire
        .body
        .content
        .iter()
        .filter_map(|e| e.end_index)
        .max()
        .unwrap_or(1);
    Ok(Document {
        document_id: wire.document_id,
        title: wire.title,
        end_index,
        tabs: flatten_tabs(&wire.tabs),
    })
}

/// Tabs nest; the list a person needs is flat and in reading order.
fn flatten_tabs(tabs: &[WireTab]) -> Vec<Tab> {
    let mut out = Vec::new();
    for tab in tabs {
        out.push(Tab {
            id: tab.tab_properties.tab_id.clone(),
            title: tab.tab_properties.title.clone(),
        });
        out.extend(flatten_tabs(&tab.child_tabs));
    }
    out
}

/// `documents.batchUpdate` with one `insertText` at the end. Plain text: Docs
/// takes the string as written, so markdown arrives as markdown characters
/// and the tool description says so.
pub async fn append_text(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    text: &str,
) -> Result<i64> {
    if text.is_empty() {
        return Err(Error::Unsupported(
            "there is nothing to append: the text is empty".into(),
        ));
    }
    let document = get(client, connection_id, document_id).await?;
    let index = document.append_index();
    let request = client
        .service(DOCS)
        .post(&format!(
            "v1/documents/{}:batchUpdate",
            urlencode(document_id)
        ))?
        .json(&BatchUpdate {
            requests: vec![DocRequest {
                insert_text: Some(InsertText {
                    text: text.to_string(),
                    location: Location { index },
                }),
                ..DocRequest::default()
            }],
        });
    let _: WireBatchReply = client.json(connection_id, request).await?;
    Ok(index)
}

/// `documents.batchUpdate` with `replaceAllText`; the answer is how many
/// occurrences were replaced, which is what the tool reports back.
pub async fn replace_all_text(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    find: &str,
    replace: &str,
    match_case: bool,
) -> Result<i64> {
    if find.is_empty() {
        return Err(Error::Unsupported(
            "the text to find is empty; that would match everywhere".into(),
        ));
    }
    let request = client
        .service(DOCS)
        .post(&format!(
            "v1/documents/{}:batchUpdate",
            urlencode(document_id)
        ))?
        .json(&BatchUpdate {
            requests: vec![DocRequest {
                replace_all_text: Some(ReplaceAllText {
                    contains_text: SubstringMatch {
                        text: find.to_string(),
                        match_case,
                    },
                    replace_text: replace.to_string(),
                }),
                ..DocRequest::default()
            }],
        });
    let reply: WireBatchReply = client.json(connection_id, request).await?;
    Ok(reply
        .replies
        .iter()
        .filter_map(|r| r.replace_all_text.as_ref())
        .map(|r| r.occurrences_changed)
        .sum())
}

// ----- indexes ---------------------------------------------------------------

/// Docs counts every position in UTF-16 code units; Rust counts strings in
/// bytes and in characters. The three disagree the moment a document is not
/// plain English: `ą` is one UTF-16 unit and two UTF-8 bytes, `😀` is two
/// units and four bytes. Every index this module sends Google is computed
/// here and nowhere else, because an index that is off by one edits the
/// middle of a word and says it succeeded.
pub mod index {
    /// How many UTF-16 code units this text takes.
    pub fn len(text: &str) -> i64 {
        text.chars().map(|c| c.len_utf16() as i64).sum()
    }

    /// How many characters sit before a UTF-16 offset. `None` when the offset
    /// is past the end of the text or halfway through a surrogate pair, which
    /// is no character at all.
    ///
    /// Nothing in the server converts this way — Google is told indexes and
    /// never asked for them — but the tests walk both directions over the
    /// same Polish and emoji text, because an index that is wrong one way is
    /// wrong the other.
    #[allow(dead_code)]
    pub fn to_chars(text: &str, units: i64) -> Option<usize> {
        let mut left = units;
        for (seen, c) in text.chars().enumerate() {
            if left == 0 {
                return Some(seen);
            }
            left -= c.len_utf16() as i64;
            if left < 0 {
                return None;
            }
        }
        (left == 0).then_some(text.chars().count())
    }
}

// ----- paragraphs ------------------------------------------------------------

/// One paragraph of the body, numbered the way `docs_list_paragraphs` numbers
/// it: 1-based, in body order, through table cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Paragraph {
    pub ordinal: usize,
    /// Docs' own name for the style: `NORMAL_TEXT`, `HEADING_2`, `TITLE`.
    pub style: String,
    /// Where the paragraph starts, in UTF-16 code units from the start of the
    /// body, as Docs counts.
    pub start_index: i64,
    /// One past the newline that ends the paragraph.
    pub end_index: i64,
    /// The text, without the newline that ends it.
    pub text: String,
    /// True when the paragraph sits in a table cell. It is numbered like any
    /// other, because the body meets it like any other.
    pub in_table: bool,
    /// The text runs, each with the index Docs gave it. An offset into `text`
    /// becomes a document index only through these: an inline picture takes
    /// an index and carries no text, so the two do not run in step.
    runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq)]
struct Run {
    start_index: i64,
    text: String,
}

impl Paragraph {
    /// How many characters the paragraph holds, as a person counts them.
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// A document as a numbered list of paragraphs, with the revision every write
/// against those numbers has to name.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    pub document_id: String,
    pub title: String,
    /// What the document was at when it was read. A write carries it back as
    /// `writeControl.requiredRevisionId`, so a document that moved in between
    /// refuses the whole batch rather than editing the wrong words.
    pub revision_id: String,
    /// Where the body ends; the last index is Docs' own final newline.
    pub end_index: i64,
    pub paragraphs: Vec<Paragraph>,
}

impl Outline {
    pub fn url(&self) -> String {
        format!(
            "https://docs.google.com/document/d/{}/edit",
            self.document_id
        )
    }
}

/// The document as numbered paragraphs. One `documents.get`, which is the
/// same read every write starts with.
pub async fn outline(client: &Client, connection_id: i64, document_id: &str) -> Result<Outline> {
    let wire = fetch(client, connection_id, document_id).await?;
    let mut paragraphs = Vec::new();
    collect_paragraphs(&wire.body.content, false, &mut paragraphs);
    Ok(Outline {
        document_id: wire.document_id,
        title: wire.title,
        revision_id: wire.revision_id,
        end_index: wire
            .body
            .content
            .iter()
            .filter_map(|e| e.end_index)
            .max()
            .unwrap_or(1),
        paragraphs,
    })
}

/// Every paragraph under this content, in the order the body meets it. A
/// table carries content of its own, so the walk goes through its cells and
/// the paragraphs in them are numbered where they are met — the same walk the
/// pictures take, for the same reason.
fn collect_paragraphs(content: &[WireElement], in_table: bool, out: &mut Vec<Paragraph>) {
    for element in content {
        if let Some(wire) = &element.paragraph {
            let start_index = element.start_index.unwrap_or_default();
            let mut cursor = start_index;
            let mut runs: Vec<Run> = Vec::new();
            for part in &wire.elements {
                let at = part.start_index.unwrap_or(cursor);
                if let Some(run) = &part.text_run {
                    runs.push(Run {
                        start_index: at,
                        text: run.content.clone(),
                    });
                    cursor = at + index::len(&run.content);
                } else {
                    // A picture takes one index and carries no text.
                    cursor = at + 1;
                }
            }
            let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
            out.push(Paragraph {
                ordinal: out.len() + 1,
                style: some(&wire.paragraph_style.named_style_type)
                    .unwrap_or_else(|| "NORMAL_TEXT".to_string()),
                start_index,
                end_index: element.end_index.unwrap_or(start_index),
                text: joined.strip_suffix('\n').unwrap_or(&joined).to_string(),
                in_table,
                runs,
            });
        }
        for row in element.table.iter().flat_map(|t| &t.table_rows) {
            for cell in &row.table_cells {
                collect_paragraphs(&cell.content, true, out);
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireDocument {
    document_id: String,
    title: String,
    /// What the document is at right now. Every write names it back, which is
    /// how a paragraph number that has gone stale is refused rather than
    /// followed.
    revision_id: String,
    body: WireBody,
    tabs: Vec<WireTab>,
    /// Keyed by object id, in whatever order the JSON happens to carry. The
    /// body says where each one belongs; this says what it is.
    inline_objects: HashMap<String, WireInlineObject>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBody {
    content: Vec<WireElement>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireElement {
    start_index: Option<i64>,
    end_index: Option<i64>,
    /// Absent on a section break and on a table, which is why both of these
    /// are optional: the body holds three kinds of element and only the
    /// paragraphs are numbered.
    paragraph: Option<WireParagraph>,
    table: Option<WireTable>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireParagraph {
    elements: Vec<WireParagraphElement>,
    paragraph_style: WireParagraphStyle,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireParagraphStyle {
    named_style_type: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireParagraphElement {
    start_index: Option<i64>,
    inline_object_element: Option<WireInlineObjectElement>,
    text_run: Option<WireTextRun>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTextRun {
    content: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireInlineObjectElement {
    inline_object_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTable {
    table_rows: Vec<WireTableRow>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTableRow {
    table_cells: Vec<WireTableCell>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTableCell {
    content: Vec<WireElement>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireInlineObject {
    inline_object_properties: WireInlineObjectProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireInlineObjectProperties {
    embedded_object: WireEmbeddedObject,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireEmbeddedObject {
    /// The alt text a person typed: Docs calls the two halves title and
    /// description.
    title: String,
    description: String,
    image_properties: WireImageProperties,
    size: WireSize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireImageProperties {
    content_uri: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSize {
    width: WireDimension,
    height: WireDimension,
}

/// Docs measures in points and has no other unit, so the unit it sends with
/// each magnitude says nothing this code has to read.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireDimension {
    magnitude: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTab {
    tab_properties: WireTabProperties,
    child_tabs: Vec<WireTab>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTabProperties {
    tab_id: String,
    title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBatchReply {
    replies: Vec<WireReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReply {
    replace_all_text: Option<WireReplaceReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReplaceReply {
    occurrences_changed: i64,
}

#[derive(Debug, Serialize)]
struct BatchUpdate {
    requests: Vec<DocRequest>,
}

/// One entry of a `batchUpdate`. Google's shape is an object with exactly
/// one of these keys set, which a struct with skipped `None`s expresses
/// directly; an enum would nest the payload one level too deep.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_text: Option<InsertText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replace_all_text: Option<ReplaceAllText>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertText {
    text: String,
    location: Location,
}

#[derive(Debug, Serialize)]
struct Location {
    index: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceAllText {
    contains_text: SubstringMatch,
    replace_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubstringMatch {
    text: String,
    match_case: bool,
}
