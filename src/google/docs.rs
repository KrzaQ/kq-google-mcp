//! Docs: a document read as numbered paragraphs, and the writes that name one
//! of those numbers. The prose read goes through Drive's markdown export;
//! `documents.get` is what says where each paragraph starts and ends, which is
//! what an edit needs, and it carries the pictures and the tab list too.
//!
//! Two things shape every write here. Docs counts positions in UTF-16 code
//! units, so every index is computed in [`index`] and nowhere else. And a
//! paragraph number goes stale the moment anything changes, including the
//! caller's own last write, so every write carries the revision it was planned
//! against and is refused rather than applied once the document has moved on.
//!
//! Structural edits are still out: nothing here moves a paragraph or removes
//! one. A model that can insert, replace inside one paragraph and set a named
//! style cannot rearrange someone's article by accident. The one structure it
//! may add is a table, and [`apply_table`] is the single write in this module
//! that sends two batches; the comment on it says why.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::client::{Client, Download, Error, Result, urlencode};
use crate::domain::limits::{TABLE_MAX_COLUMNS, TABLE_MAX_ROWS};

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
            write_control: None,
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
            write_control: None,
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

    /// The UTF-16 offset of a byte offset into `text`. A byte offset that is
    /// not a character boundary, or is past the end, counts the whole text —
    /// there is no such position to point at.
    pub fn from_bytes(text: &str, bytes: usize) -> i64 {
        match text.get(..bytes) {
            Some(head) => len(head),
            None => len(text),
        }
    }

    /// The UTF-16 offset of a character offset into `text`, and `None` when
    /// the text has fewer characters than that. This is the conversion a
    /// code listing's spans need: the caller counts characters, Google counts
    /// code units.
    pub fn from_chars(text: &str, chars: usize) -> Option<i64> {
        let mut units = 0;
        for (seen, c) in text.chars().enumerate() {
            if seen == chars {
                return Some(units);
            }
            units += c.len_utf16() as i64;
        }
        (chars == text.chars().count()).then_some(units)
    }

    /// How many characters sit before a UTF-16 offset: [`from_chars`] the
    /// other way round. `None` when the offset is past the end of the text or
    /// halfway through a surrogate pair, which is no character at all.
    ///
    /// A write is told indexes and never asked for them, so this is the read
    /// direction: `docs_read_formatting` reports each run in the characters
    /// its caller counts. The tests walk both directions over the same Polish
    /// and emoji text, because an index that is wrong one way is wrong the
    /// other.
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

/// The named paragraph styles Docs has, in the order a tool lists them.
/// Alignment, spacing and indentation are deliberately not here: these tools
/// set a named style and nothing else, because a magazine article needs
/// headings and not a word processor.
pub const NAMED_STYLES: [&str; 9] = [
    "NORMAL_TEXT",
    "TITLE",
    "SUBTITLE",
    "HEADING_1",
    "HEADING_2",
    "HEADING_3",
    "HEADING_4",
    "HEADING_5",
    "HEADING_6",
];

/// The font a code listing is set in when the caller names none.
pub const CODE_FONT: &str = "Courier New";

/// What every stale write is told, in one sentence and one place.
const STALE: &str = "the document changed since it was read, so nothing was written; \
                     call docs_list_paragraphs again — one write moves the paragraph numbers \
                     and the revision id both";

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
    style: RunStyle,
}

/// What one run of text is set to, and only that. Docs answers with the
/// properties somebody chose on the run and leaves out the ones it inherits
/// from the paragraph's named style, so an absent value here means the
/// document says nothing about it and not that it is off.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunStyle {
    /// The foreground colour as `#rrggbb`.
    pub colour: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub font: Option<String>,
    /// The size in points, as Docs measures it.
    pub size: Option<f64>,
}

/// One run of a paragraph, with the characters it covers.
///
/// `start` and `end` count characters from the start of the paragraph, which
/// is what [`Span`] takes: a listing read back this way can be written back
/// with the same numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledRun {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub style: RunStyle,
}

impl Paragraph {
    /// How many characters the paragraph holds, as a person counts them.
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }

    /// The runs this paragraph is made of, each with the characters it covers
    /// and only the properties the document sets on it.
    ///
    /// Docs merges neighbouring characters that share a style into one run,
    /// so a listing written as twenty coloured spans reads back as fewer runs
    /// than that. The colour at a given offset is the thing to compare; the
    /// number of runs is Docs' own business.
    ///
    /// The offsets are characters and Docs' indexes are UTF-16 code units, so
    /// the walk counts units and converts through [`index`], the one place
    /// this module does that.
    pub fn formatting(&self) -> Vec<StyledRun> {
        let mut out = Vec::with_capacity(self.runs.len());
        let mut units = 0;
        for (at, run) in self.runs.iter().enumerate() {
            // Only the last run of a paragraph can carry the newline that
            // ends it, and `text` does not hold that newline.
            let text = match at + 1 == self.runs.len() {
                true => run.text.strip_suffix('\n').unwrap_or(&run.text),
                false => run.text.as_str(),
            };
            if text.is_empty() {
                continue;
            }
            let start = index::to_chars(&self.text, units);
            units += index::len(text);
            let (Some(start), Some(end)) = (start, index::to_chars(&self.text, units)) else {
                // The runs and the text they were joined from disagree, which
                // they cannot; reporting nothing is better than reporting an
                // offset a write would act on.
                return Vec::new();
            };
            out.push(StyledRun {
                start,
                end,
                text: text.to_string(),
                style: run.style.clone(),
            });
        }
        out
    }

    /// The document index of a byte offset into [`Paragraph::text`], or
    /// `None` when the offset is past the end of the runs.
    fn index_at(&self, byte: usize) -> Option<i64> {
        let mut consumed = 0;
        for run in &self.runs {
            if byte <= consumed + run.text.len() {
                return Some(run.start_index + index::from_bytes(&run.text, byte - consumed));
            }
            consumed += run.text.len();
        }
        None
    }

    /// The second lock on a write. The caller says what it believes this
    /// paragraph starts with, and a paragraph that says otherwise is not
    /// written to. The revision guard catches a document that changed; this
    /// catches a caller that counted wrong.
    pub fn check_expect(&self, expect: &str) -> Result<()> {
        let expect = expect.trim();
        if expect.is_empty() {
            return Err(Error::Unsupported(
                "`expect` is empty; pass the words the paragraph starts with, as \
                 docs_list_paragraphs reports them"
                    .into(),
            ));
        }
        if self.text.trim_start().starts_with(expect) {
            return Ok(());
        }
        Err(Error::Unsupported(format!(
            "paragraph {} does not start with {expect:?}; it starts with {:?}. Nothing was \
             written — read the document again and count once more",
            self.ordinal,
            head(&self.text, 80)
        )))
    }
}

/// The beginning of a paragraph, for a message that has to quote it.
fn head(text: &str, keep: usize) -> String {
    let start: String = text.chars().take(keep).collect();
    if text.chars().count() > keep {
        format!("{start}…")
    } else {
        start
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
    /// The tables the body holds, in the order it meets them. This is how a
    /// table that has just been made is found again: what Google answered
    /// says where each of its cells is, so nothing has to compute that.
    tables: Vec<Grid>,
}

/// One table of the document, as Google described it.
#[derive(Debug, Clone, PartialEq)]
struct Grid {
    /// Where the table itself starts, in Docs' own index.
    index: i64,
    rows: Vec<Vec<Cell>>,
}

/// One cell, named by its first paragraph. A cell always holds at least one
/// paragraph, and that paragraph is where text written into the cell goes.
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    /// Where that paragraph starts, as Docs counts.
    index: i64,
    /// The number `docs_list_paragraphs` gives it.
    ordinal: usize,
    /// True when the cell holds no text at all.
    empty: bool,
}

impl Grid {
    fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.rows.iter().flatten()
    }

    /// The paragraph number of the first cell and of the last.
    fn paragraphs(&self) -> (usize, usize) {
        let first = self.cells().next().map_or(0, |c| c.ordinal);
        let last = self.cells().last().map_or(0, |c| c.ordinal);
        (first, last)
    }
}

impl Outline {
    pub fn url(&self) -> String {
        format!(
            "https://docs.google.com/document/d/{}/edit",
            self.document_id
        )
    }

    /// The paragraph a caller numbered, or the refusal that says how many
    /// there are.
    pub fn paragraph(&self, ordinal: usize) -> Result<&Paragraph> {
        if ordinal == 0 {
            return Err(Error::Unsupported(
                "paragraphs are numbered from 1, so there is no paragraph 0".into(),
            ));
        }
        self.paragraphs.get(ordinal - 1).ok_or_else(|| {
            Error::Unsupported(format!(
                "this document has {} paragraph{}, so there is no paragraph {ordinal}",
                self.paragraphs.len(),
                if self.paragraphs.len() == 1 { "" } else { "s" }
            ))
        })
    }

    /// The revision the caller planned against, against the one the document
    /// is at now. This is the guard that fires before anything is sent; the
    /// same guard travels with the batch for the gap in between.
    fn check_revision(&self, revision_id: &str) -> Result<()> {
        let asked = revision_id.trim();
        if asked.is_empty() {
            return Err(Error::Unsupported(
                "this write needs the revision_id that docs_list_paragraphs answered with; \
                 it is what says the paragraph numbers are still the document's"
                    .into(),
            ));
        }
        if asked == self.revision_id {
            return Ok(());
        }
        Err(Error::Unsupported(format!(
            "{STALE}. The write named revision {asked:?} and the document is at {:?}",
            self.revision_id
        )))
    }

    /// Where an insert goes and what is written there. A paragraph is text
    /// that ends in a newline, so an insert carries its own break: before a
    /// paragraph the text is followed by one, and after the document's last
    /// paragraph it is preceded by one instead, because the index after the
    /// body's final newline is the one place Docs refuses to write at.
    ///
    /// The third value is where the caller's own text begins, which is what a
    /// style or a colour is measured from.
    fn insertion(&self, at: At, paragraph: &Paragraph, body: &str) -> (i64, String, i64) {
        match at {
            At::Before(_) => (
                paragraph.start_index,
                format!("{body}\n"),
                paragraph.start_index,
            ),
            At::After(_) if paragraph.end_index >= self.end_index => {
                let index = (self.end_index - 1).max(1);
                (index, format!("\n{body}"), index + 1)
            }
            At::After(_) => (
                paragraph.end_index,
                format!("{body}\n"),
                paragraph.end_index,
            ),
        }
    }

    /// The range one paragraph covers, as a style request may name it. The
    /// body's final newline is Docs' own, so the last paragraph's range stops
    /// one short of it; the paragraph still overlaps the range, which is all
    /// `updateParagraphStyle` asks for.
    fn style_range(&self, paragraph: &Paragraph) -> Range {
        let end = if paragraph.end_index >= self.end_index {
            (self.end_index - 1).max(paragraph.start_index + 1)
        } else {
            paragraph.end_index
        };
        Range {
            start_index: paragraph.start_index,
            end_index: end,
        }
    }

    /// The table a write has just made: the first one at or after the index
    /// it was inserted at that has the shape that was asked for and holds
    /// nothing at all.
    ///
    /// The position alone would be enough in a document nobody else is
    /// editing. The shape and the emptiness are checked as well because this
    /// lookup decides where text is about to be written, and a table that is
    /// not the new one is the one failure this whole two-batch write exists
    /// to avoid.
    fn new_table(&self, index: i64, rows: usize, columns: usize) -> Option<&Grid> {
        self.tables.iter().find(|grid| {
            grid.index >= index
                && grid.rows.len() == rows
                && grid.rows.iter().all(|row| row.len() == columns)
                && grid.cells().all(|cell| cell.empty && cell.ordinal > 0)
        })
    }
}

/// The document as numbered paragraphs. One `documents.get`, which is the
/// same read every write starts with.
pub async fn outline(client: &Client, connection_id: i64, document_id: &str) -> Result<Outline> {
    let wire = fetch(client, connection_id, document_id).await?;
    let mut paragraphs = Vec::new();
    let mut tables = Vec::new();
    collect_paragraphs(&wire.body.content, false, &mut paragraphs, &mut tables);
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
        tables,
    })
}

/// Every paragraph under this content, in the order the body meets it. A
/// table carries content of its own, so the walk goes through its cells and
/// the paragraphs in them are numbered where they are met — the same walk the
/// pictures take, for the same reason.
fn collect_paragraphs(
    content: &[WireElement],
    in_table: bool,
    out: &mut Vec<Paragraph>,
    tables: &mut Vec<Grid>,
) {
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
                        style: RunStyle::from(&run.text_style),
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
        // A table is walked cell by cell, and each cell is noted as the
        // paragraph its text would go into. The numbering is the walk's own,
        // so a cell's paragraph number here is the number the listing gives
        // it and the two can never drift apart.
        if let Some(table) = &element.table {
            let mut rows = Vec::with_capacity(table.table_rows.len());
            for row in &table.table_rows {
                let mut cells = Vec::with_capacity(row.table_cells.len());
                for cell in &row.table_cells {
                    let at = out.len();
                    collect_paragraphs(&cell.content, true, out, tables);
                    cells.push(Cell {
                        index: out.get(at).map_or(0, |p| p.start_index),
                        ordinal: out.get(at).map_or(0, |p| p.ordinal),
                        empty: out[at..].iter().all(|p| p.text.is_empty()),
                    });
                }
                rows.push(cells);
            }
            tables.push(Grid {
                index: element.start_index.unwrap_or_default(),
                rows,
            });
        }
    }
}

// ----- planning a write ------------------------------------------------------

/// Where an insert goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    After(usize),
    Before(usize),
}

impl At {
    fn ordinal(self) -> usize {
        match self {
            At::After(n) | At::Before(n) => n,
        }
    }
}

/// One coloured run of a code listing, measured in characters from the start
/// of the code. The calling model works out where the tokens are: this server
/// does no syntax highlighting and knows no languages.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// `#rrggbb`, and nothing else.
    pub colour: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

/// One document write, worked out in full before anything is sent: which
/// paragraph it is about, how the text reads now and how it would read
/// afterwards, and the requests that do it.
///
/// A preview and a write build the same plan, so both are refused for the
/// same reasons in the same words, and a preview that comes back is a write
/// that would go through.
#[derive(Debug)]
pub struct Plan {
    /// The paragraph the write is about; for an insert, the one it goes next
    /// to.
    pub paragraph: usize,
    /// How that paragraph reads now.
    pub before: String,
    /// How the text this write leaves behind reads: the changed paragraph, or
    /// the inserted one.
    pub after: String,
    /// Where the write lands, in Docs' own index.
    pub index: i64,
    revision_id: String,
    requests: Vec<DocRequest>,
}

impl Plan {
    /// How many requests the one batch carries. A code listing reports it, so
    /// a person can see the text, the font and every colour go in together.
    pub fn requests(&self) -> usize {
        self.requests.len()
    }
}

/// A named style, as Docs spells it. `heading 2` and `Heading_2` are the same
/// style; anything that is not one of the nine is refused with the nine.
pub fn named_style(style: &str) -> Result<&'static str> {
    let wanted = style.trim().to_ascii_uppercase().replace([' ', '-'], "_");
    NAMED_STYLES
        .into_iter()
        .find(|s| *s == wanted)
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "{style:?} is not a paragraph style; Docs has {}",
                NAMED_STYLES.join(", ")
            ))
        })
}

/// Text as its own new paragraph, before or after the paragraph a caller
/// numbered, optionally under a named style. Plain text: Docs takes the
/// string as written, so `## Heading` would arrive as those characters, which
/// is what the style is for.
pub fn plan_insert(
    outline: &Outline,
    revision_id: &str,
    at: At,
    text: &str,
    style: Option<&str>,
    expect: Option<&str>,
) -> Result<Plan> {
    outline.check_revision(revision_id)?;
    let body = text.trim_end_matches('\n');
    if body.trim().is_empty() {
        return Err(Error::Unsupported(
            "there is nothing to insert: the text is empty".into(),
        ));
    }
    let style = style.map(named_style).transpose()?;
    let neighbour = outline.paragraph(at.ordinal())?;
    if let Some(expect) = expect {
        neighbour.check_expect(expect)?;
    }
    let (index, payload, text_at) = outline.insertion(at, neighbour, body);
    let mut requests = vec![insert_request(index, &payload)];
    if let Some(named_style_type) = style {
        requests.push(DocRequest {
            update_paragraph_style: Some(UpdateParagraphStyle {
                range: Range {
                    start_index: text_at,
                    end_index: text_at + index::len(body),
                },
                paragraph_style: ParagraphStyle { named_style_type },
                fields: "namedStyleType",
            }),
            ..DocRequest::default()
        });
    }
    Ok(Plan {
        paragraph: neighbour.ordinal,
        before: neighbour.text.clone(),
        after: body.to_string(),
        index,
        revision_id: revision_id.trim().to_string(),
        requests,
    })
}

/// One occurrence of one string inside one paragraph. This is what
/// `docs_replace_text` should be for careful work: it changes the words a
/// caller means and leaves every other copy of them alone.
pub fn plan_edit(
    outline: &Outline,
    revision_id: &str,
    ordinal: usize,
    find: &str,
    occurrence: usize,
    replace: &str,
    expect: &str,
) -> Result<Plan> {
    outline.check_revision(revision_id)?;
    let paragraph = outline.paragraph(ordinal)?;
    paragraph.check_expect(expect)?;
    if find.is_empty() {
        return Err(Error::Unsupported(
            "the text to find is empty; say which words in the paragraph to change".into(),
        ));
    }
    if occurrence == 0 {
        return Err(Error::Unsupported(
            "occurrences are counted from 1: occurrence 1 is the first match in the paragraph"
                .into(),
        ));
    }
    let found: Vec<usize> = paragraph
        .text
        .match_indices(find)
        .map(|(at, _)| at)
        .collect();
    let Some(&at) = found.get(occurrence - 1) else {
        return Err(Error::Unsupported(format!(
            "paragraph {ordinal} holds {} of {find:?}, so it has no occurrence {occurrence}; \
             nothing was written",
            found.len()
        )));
    };
    let (Some(start), Some(end)) = (paragraph.index_at(at), paragraph.index_at(at + find.len()))
    else {
        return Err(Error::Malformed(format!(
            "paragraph {ordinal} does not line up with the indexes Google gave it"
        )));
    };
    let after = format!(
        "{}{replace}{}",
        &paragraph.text[..at],
        &paragraph.text[at + find.len()..]
    );
    // The new text goes in first and the old text comes out after it, both in
    // the one batch: Docs applies the requests in order, so the deletion
    // names the old words where the insertion has just pushed them. Inserted
    // this way the text keeps the formatting of what it is put beside, which
    // a deletion first would lose.
    let mut requests = Vec::new();
    let shift = if replace.is_empty() {
        0
    } else {
        requests.push(insert_request(start, replace));
        index::len(replace)
    };
    requests.push(DocRequest {
        delete_content_range: Some(DeleteContentRange {
            range: Range {
                start_index: start + shift,
                end_index: end + shift,
            },
        }),
        ..DocRequest::default()
    });
    Ok(Plan {
        paragraph: ordinal,
        before: paragraph.text.clone(),
        after,
        index: start,
        revision_id: revision_id.trim().to_string(),
        requests,
    })
}

/// The named style of one paragraph, and nothing else about it.
pub fn plan_style(
    outline: &Outline,
    revision_id: &str,
    ordinal: usize,
    style: &str,
    expect: &str,
) -> Result<Plan> {
    outline.check_revision(revision_id)?;
    let paragraph = outline.paragraph(ordinal)?;
    paragraph.check_expect(expect)?;
    let named_style_type = named_style(style)?;
    Ok(Plan {
        paragraph: ordinal,
        before: paragraph.style.clone(),
        after: named_style_type.to_string(),
        index: paragraph.start_index,
        revision_id: revision_id.trim().to_string(),
        requests: vec![DocRequest {
            update_paragraph_style: Some(UpdateParagraphStyle {
                range: outline.style_range(paragraph),
                paragraph_style: ParagraphStyle { named_style_type },
                fields: "namedStyleType",
            }),
            ..DocRequest::default()
        }],
    })
}

/// A code listing: the text, the monospace font over the whole of it, and one
/// `updateTextStyle` per coloured span — all in the one batch, because a
/// listing that arrived and was not coloured is worse than one that was
/// refused.
pub fn plan_code(
    outline: &Outline,
    revision_id: &str,
    after_paragraph: usize,
    code: &str,
    font: Option<&str>,
    spans: &[Span],
    expect: Option<&str>,
) -> Result<Plan> {
    outline.check_revision(revision_id)?;
    let body = code.trim_end_matches('\n');
    if body.trim().is_empty() {
        return Err(Error::Unsupported(
            "there is nothing to insert: the code is empty".into(),
        ));
    }
    let font = font
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or(CODE_FONT);
    let neighbour = outline.paragraph(after_paragraph)?;
    if let Some(expect) = expect {
        neighbour.check_expect(expect)?;
    }
    // Every span is checked before the first request is built. A listing
    // half-coloured because the fourth span was nonsense would have to be
    // repaired by hand.
    let ranges = span_ranges(body, spans)?;
    let (index, payload, text_at) = outline.insertion(At::After(after_paragraph), neighbour, body);
    let mut requests = vec![
        insert_request(index, &payload),
        DocRequest {
            update_text_style: Some(UpdateTextStyle {
                range: Range {
                    start_index: text_at,
                    end_index: text_at + index::len(body),
                },
                text_style: TextStyle {
                    weighted_font_family: Some(WeightedFontFamily {
                        font_family: font.to_string(),
                    }),
                    ..TextStyle::default()
                },
                fields: "weightedFontFamily".to_string(),
            }),
            ..DocRequest::default()
        },
    ];
    for (span, (start, end)) in spans.iter().zip(ranges) {
        let mut fields: Vec<&str> = Vec::new();
        let mut text_style = TextStyle::default();
        if let Some(colour) = &span.colour {
            text_style.foreground_color = Some(OptionalColor {
                color: Color {
                    rgb_color: rgb(colour)?,
                },
            });
            fields.push("foregroundColor");
        }
        if let Some(bold) = span.bold {
            text_style.bold = Some(bold);
            fields.push("bold");
        }
        if let Some(italic) = span.italic {
            text_style.italic = Some(italic);
            fields.push("italic");
        }
        if fields.is_empty() {
            return Err(Error::Unsupported(format!(
                "the span {}..{} says nothing to change; give it a colour, bold or italic",
                span.start, span.end
            )));
        }
        requests.push(DocRequest {
            update_text_style: Some(UpdateTextStyle {
                range: Range {
                    start_index: text_at + start,
                    end_index: text_at + end,
                },
                text_style,
                fields: fields.join(","),
            }),
            ..DocRequest::default()
        });
    }
    Ok(Plan {
        paragraph: after_paragraph,
        before: neighbour.text.clone(),
        after: body.to_string(),
        index,
        revision_id: revision_id.trim().to_string(),
        requests,
    })
}

/// A picture on its way into a document.
///
/// Docs has no request that takes image bytes and none that sets alt text, so
/// there are exactly three things to say about a picture: where Google may
/// fetch it and how large it should be on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct NewImage<'a> {
    /// A URL Google's own servers can reach while the batch is in flight.
    pub uri: &'a str,
    /// How large on the page, in points. Docs works the other side out from
    /// the picture itself when only one is given, and uses the picture's own
    /// size when neither is.
    pub width_pt: Option<f64>,
    pub height_pt: Option<f64>,
    /// What to call it in the plan. This never reaches Google.
    pub label: &'a str,
}

/// A picture as a new paragraph after the paragraph a caller numbered: the
/// break, then `insertInlineImage` inside it, in the one batch.
///
/// Google fetches `uri` itself while this call is in flight, so the URL has to
/// be reachable from outside for that moment. It is not checked here — this
/// server mints it — and a preview passes an empty one, because a preview
/// builds the same plan and sends none of it.
pub fn plan_image(
    outline: &Outline,
    revision_id: &str,
    after_paragraph: usize,
    image: &NewImage<'_>,
    expect: Option<&str>,
) -> Result<Plan> {
    outline.check_revision(revision_id)?;
    let neighbour = outline.paragraph(after_paragraph)?;
    if let Some(expect) = expect {
        neighbour.check_expect(expect)?;
    }
    let size = object_size(image)?;
    // The body is empty: what this insert carries is the paragraph break, and
    // the picture goes inside the paragraph the break makes.
    let (index, payload, image_at) = outline.insertion(At::After(after_paragraph), neighbour, "");
    Ok(Plan {
        paragraph: after_paragraph,
        before: neighbour.text.clone(),
        after: image.label.to_string(),
        index: image_at,
        revision_id: revision_id.trim().to_string(),
        requests: vec![
            insert_request(index, &payload),
            DocRequest {
                insert_inline_image: Some(InsertInlineImage {
                    uri: image.uri.to_string(),
                    location: Location { index: image_at },
                    object_size: size,
                }),
                ..DocRequest::default()
            },
        ],
    })
}

/// How large the picture is told to be, or nothing at all when the caller
/// said nothing. A measurement Docs would refuse is refused here instead.
fn object_size(image: &NewImage<'_>) -> Result<Option<ObjectSize>> {
    for (magnitude, side) in [(image.width_pt, "width_pt"), (image.height_pt, "height_pt")] {
        if let Some(magnitude) = magnitude
            && !(magnitude.is_finite() && magnitude > 0.0)
        {
            return Err(Error::Unsupported(format!(
                "{side} is {magnitude}; a picture is measured in points and has to be more than \
                 zero. Leave both out to keep the picture's own size"
            )));
        }
    }
    let dimension = |magnitude: Option<f64>| {
        magnitude.map(|magnitude| Dimension {
            magnitude,
            unit: "PT",
        })
    };
    match (dimension(image.width_pt), dimension(image.height_pt)) {
        (None, None) => Ok(None),
        (width, height) => Ok(Some(ObjectSize { width, height })),
    }
}

/// Each span as a pair of UTF-16 offsets into the code, or the first reason
/// the set of them cannot be written: a span that is empty, one that runs
/// past the end of the code, or two that overlap. Nothing is built until they
/// all pass.
fn span_ranges(code: &str, spans: &[Span]) -> Result<Vec<(i64, i64)>> {
    let characters = code.chars().count();
    let mut ranges = Vec::with_capacity(spans.len());
    for span in spans {
        if span.end <= span.start {
            return Err(Error::Unsupported(format!(
                "the span {}..{} is empty; a span must cover at least one character",
                span.start, span.end
            )));
        }
        if span.end > characters {
            return Err(Error::Unsupported(format!(
                "the span {}..{} runs past the end of the code, which is {characters} characters",
                span.start, span.end
            )));
        }
        let (Some(start), Some(end)) = (
            index::from_chars(code, span.start),
            index::from_chars(code, span.end),
        ) else {
            return Err(Error::Unsupported(format!(
                "the span {}..{} does not land on characters of the code",
                span.start, span.end
            )));
        };
        ranges.push((start, end));
    }
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|i| spans[*i].start);
    for pair in order.windows(2) {
        let (first, next) = (&spans[pair[0]], &spans[pair[1]]);
        if next.start < first.end {
            return Err(Error::Unsupported(format!(
                "the spans {}..{} and {}..{} overlap; each character of a listing takes its \
                 colour from one span",
                first.start, first.end, next.start, next.end
            )));
        }
    }
    Ok(ranges)
}

/// `#rrggbb` and nothing else. A colour Docs would read as black is a listing
/// that looks broken, so the refusal comes before the write.
fn rgb(colour: &str) -> Result<RgbColor> {
    let digits = colour.trim().strip_prefix('#').unwrap_or_default();
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Unsupported(format!(
            "{colour:?} is not a colour; write it as #rrggbb, for example #1a7f37"
        )));
    }
    let channel =
        |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).unwrap_or_default() as f32 / 255.0;
    Ok(RgbColor {
        red: channel(0),
        green: channel(2),
        blue: channel(4),
    })
}

fn insert_request(index: i64, text: &str) -> DocRequest {
    DocRequest {
        insert_text: Some(InsertText {
            text: text.to_string(),
            location: Location { index },
        }),
        ..DocRequest::default()
    }
}

/// The one `batchUpdate` a write tool sends. Everything a plan holds goes in
/// this single batch: Docs applies a batch in order and as one write, and a
/// second batch would be a half-applied edit every time the first succeeded
/// and the second did not.
pub async fn apply(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    plan: Plan,
) -> Result<()> {
    batch(
        client,
        connection_id,
        document_id,
        plan.requests,
        plan.revision_id,
    )
    .await
    .map_err(stale)?;
    Ok(())
}

/// One `documents.batchUpdate`, against the revision it was planned for.
async fn batch(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    requests: Vec<DocRequest>,
    revision_id: String,
) -> Result<()> {
    let request = client
        .service(DOCS)
        .post(&format!(
            "v1/documents/{}:batchUpdate",
            urlencode(document_id)
        ))?
        .json(&BatchUpdate {
            requests,
            write_control: Some(WriteControl {
                required_revision_id: revision_id,
            }),
        });
    let _: WireBatchReply = client.json(connection_id, request).await?;
    Ok(())
}

/// Google's refusal of a batch whose revision has moved on, said the way the
/// model has to act on it: read the document again. A retry of the same write
/// would be a write against paragraph numbers that have already moved, so
/// there is none.
fn stale(e: Error) -> Error {
    match &e {
        Error::Google(g)
            if matches!(g.status, 400 | 409 | 412)
                && g.message.to_lowercase().contains("revision") =>
        {
            Error::Unsupported(format!("{STALE}. Google refused the write: {}", g.message))
        }
        _ => e,
    }
}

// ----- a table ---------------------------------------------------------------

/// A table on its way into a document: the empty grid, planned against the
/// caller's revision, and the text that goes in its cells afterwards.
///
/// The cells are not in the plan's requests, because where they go is not
/// known yet: the indexes inside a table are Google's answer to the write
/// that makes it. [`apply_table`] is where that happens.
#[derive(Debug)]
pub struct TablePlan {
    /// The paragraph the table goes after.
    pub paragraph: usize,
    /// How that paragraph reads now.
    pub before: String,
    pub rows: usize,
    pub columns: usize,
    /// Whether the first row is set bold, which is the only formatting here.
    pub header: bool,
    /// Where the table goes, in Docs' own index.
    pub index: i64,
    grid: Vec<Vec<String>>,
    revision_id: String,
    requests: Vec<DocRequest>,
}

impl TablePlan {
    /// The grid as lines, one row to a line, for a person to read before it
    /// is written.
    pub fn lines(&self) -> Vec<String> {
        self.grid.iter().map(|row| row.join(" | ")).collect()
    }
}

/// Where a table landed, once both batches have gone through.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertedTable {
    pub rows: usize,
    pub columns: usize,
    /// The paragraph numbers of the first and the last cell, as
    /// `docs_list_paragraphs` numbers them. Filling a cell adds no paragraph,
    /// so these are the numbers of the finished table too.
    pub first_paragraph: usize,
    pub last_paragraph: usize,
    /// Where the table starts, in Docs' own index.
    pub index: i64,
}

/// A grid of plain text after the paragraph a caller numbered. The cells are
/// text and nothing else: no markdown, no colours, no widths, no borders and
/// no merged cells, because a magazine table needs none of those and every
/// one of them is a thing that can go wrong in somebody's document.
pub fn plan_table(
    outline: &Outline,
    revision_id: &str,
    after_paragraph: usize,
    rows: &[Vec<String>],
    header: bool,
    expect: Option<&str>,
) -> Result<TablePlan> {
    outline.check_revision(revision_id)?;
    // The whole grid is checked before anything is sent, so a table Google
    // would have made and this server could not have filled is refused while
    // the document is still untouched.
    let (height, width) = grid_size(rows)?;
    let neighbour = outline.paragraph(after_paragraph)?;
    if let Some(expect) = expect {
        neighbour.check_expect(expect)?;
    }
    // Where a new paragraph would go is where the table goes. Docs writes a
    // newline of its own before a table, so this insert carries none.
    let (index, _, _) = outline.insertion(At::After(after_paragraph), neighbour, "");
    Ok(TablePlan {
        paragraph: neighbour.ordinal,
        before: neighbour.text.clone(),
        rows: height,
        columns: width,
        header,
        index,
        grid: rows.to_vec(),
        revision_id: revision_id.trim().to_string(),
        requests: vec![DocRequest {
            insert_table: Some(InsertTable {
                rows: height as i64,
                columns: width as i64,
                location: Location { index },
            }),
            ..DocRequest::default()
        }],
    })
}

/// How many rows and columns the grid has, or the first reason it is not a
/// table: no rows at all, a row that holds a different number of cells than
/// the first one, a cell that is more than one line, or more rows or columns
/// than a tool here writes.
fn grid_size(rows: &[Vec<String>]) -> Result<(usize, usize)> {
    let Some(first) = rows.first() else {
        return Err(Error::Unsupported(
            "there is nothing to insert: the table has no rows".into(),
        ));
    };
    let columns = first.len();
    if columns == 0 {
        return Err(Error::Unsupported(
            "there is nothing to insert: row 1 holds no cells, so the table has no columns".into(),
        ));
    }
    for (at, row) in rows.iter().enumerate() {
        if row.len() != columns {
            return Err(Error::Unsupported(format!(
                "row {} holds {} cell{} and row 1 holds {columns}; every row of a table holds \
                 the same number of cells. Nothing was written",
                at + 1,
                row.len(),
                if row.len() == 1 { "" } else { "s" }
            )));
        }
        for (cell, text) in row.iter().enumerate() {
            if text.contains('\n') || text.contains('\r') {
                return Err(Error::Unsupported(format!(
                    "the cell in row {}, column {} holds a line break; a cell here is one line \
                     of plain text. Nothing was written",
                    at + 1,
                    cell + 1
                )));
            }
        }
    }
    if rows.len() > TABLE_MAX_ROWS {
        return Err(Error::Unsupported(format!(
            "this table has {} rows and a table written here holds at most {TABLE_MAX_ROWS}; \
             count the data again, and put a longer table in a spreadsheet. Nothing was written",
            rows.len()
        )));
    }
    if columns > TABLE_MAX_COLUMNS {
        return Err(Error::Unsupported(format!(
            "this table has {columns} columns and a table written here holds at most \
             {TABLE_MAX_COLUMNS}; count the data again. Nothing was written"
        )));
    }
    Ok((rows.len(), columns))
}

/// The one write in this module that sends two batches, deliberately.
///
/// `insertTable` makes an empty grid, and filling it means writing at indexes
/// inside cells that do not exist until the table does. Those indexes could
/// be worked out from a formula for the new layout and everything sent in one
/// batch. They are not. A formula would be proved only against a fixture this
/// repository wrote itself, so a wrong formula and a wrong fixture would agree
/// with each other while the text landed in the wrong cells of a real
/// document.
///
/// So: the empty grid, then `documents.get` again, then the cells filled at
/// the indexes Google itself answered with — in reverse document order, so
/// that each insert leaves the indexes of the ones still to come exactly
/// where they were. A header's bold follows its own cell's insert straight
/// away, while that text is still where it was just put. Both batches carry a
/// revision: the caller's, and then the one from this server's own re-read,
/// so anything that changed in between refuses the second batch.
///
/// When the second batch does fail, the table is there and empty. That is
/// what the error says, with the paragraph numbers it occupies. An empty
/// table a person can see and fix is an acceptable failure; text in the wrong
/// cells is not.
pub async fn apply_table(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    plan: TablePlan,
) -> Result<InsertedTable> {
    let TablePlan {
        rows,
        columns,
        header,
        index,
        grid,
        revision_id,
        requests,
        ..
    } = plan;
    batch(client, connection_id, document_id, requests, revision_id)
        .await
        .map_err(stale)?;
    // From here a table exists in somebody's document, so every refusal below
    // says that, rather than reading as though nothing had happened.
    let after = outline(client, connection_id, document_id)
        .await
        .map_err(|e| {
            Error::Unsupported(format!(
                "{}, and reading the document back to find its cells failed: {e}. Call \
                 docs_list_paragraphs to see where it is",
                made(rows, columns)
            ))
        })?;
    let Some(table) = after.new_table(index, rows, columns) else {
        return Err(Error::Unsupported(format!(
            "{}, and this server could not find it in the document it read back, so nothing \
             was written into its cells. Call docs_list_paragraphs to see where it is",
            made(rows, columns)
        )));
    };
    let (first_paragraph, last_paragraph) = table.paragraphs();
    let requests = fill_requests(table, &grid, header);
    if !requests.is_empty() {
        batch(
            client,
            connection_id,
            document_id,
            requests,
            after.revision_id.clone(),
        )
        .await
        .map_err(|e| empty_table(rows, columns, first_paragraph, last_paragraph, e))?;
    }
    Ok(InsertedTable {
        rows,
        columns,
        first_paragraph,
        last_paragraph,
        index: table.index,
    })
}

/// What the first batch did, in the words every refusal after it starts with.
fn made(rows: usize, columns: usize) -> String {
    format!("the {rows} by {columns} table was created and is empty")
}

/// The second batch failed, so the table stands there with nothing in it.
/// The answer says exactly that, says where it is in numbers the next call
/// can use, and says what to do about it.
fn empty_table(
    rows: usize,
    columns: usize,
    first_paragraph: usize,
    last_paragraph: usize,
    e: Error,
) -> Error {
    Error::Unsupported(format!(
        "{}: its cells are paragraphs {first_paragraph} to {last_paragraph} of the document as \
         it stands now, and none of the text was written into them. Fill a cell with \
         docs_insert_text before_paragraph=<that number>, change one that already holds text \
         with docs_edit_paragraph, or delete the table in Docs and start again. Read the \
         document with docs_list_paragraphs first, because the numbers here are from this \
         server's own read and the write that failed may have been refused because somebody \
         else was editing. Google refused the second write: {e}",
        made(rows, columns)
    ))
}

/// The requests that fill the cells, in reverse document order.
///
/// An insert moves everything after it, so the last cell is written first and
/// every index still to be used is the one Google gave for a document that
/// has not moved yet. A header cell is set bold immediately after its own
/// insert, where the text covers exactly the units it was just written at.
/// An empty cell is skipped: Docs refuses an insert of no text.
fn fill_requests(table: &Grid, grid: &[Vec<String>], header: bool) -> Vec<DocRequest> {
    let mut requests = Vec::new();
    for (at, (row, texts)) in table.rows.iter().zip(grid).enumerate().rev() {
        for (cell, text) in row.iter().zip(texts).rev() {
            if text.is_empty() {
                continue;
            }
            requests.push(insert_request(cell.index, text));
            if header && at == 0 {
                requests.push(DocRequest {
                    update_text_style: Some(UpdateTextStyle {
                        range: Range {
                            start_index: cell.index,
                            end_index: cell.index + index::len(text),
                        },
                        text_style: TextStyle {
                            bold: Some(true),
                            ..TextStyle::default()
                        },
                        fields: "bold".to_string(),
                    }),
                    ..DocRequest::default()
                });
            }
        }
    }
    requests
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
    text_style: WireTextStyle,
}

/// What Docs says about one run. Every field is optional because Docs sends
/// only what the document sets: a run nobody styled comes back with no text
/// style at all.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTextStyle {
    bold: Option<bool>,
    italic: Option<bool>,
    foreground_color: Option<WireOptionalColor>,
    weighted_font_family: Option<WireWeightedFontFamily>,
    font_size: Option<WireDimension>,
}

/// Docs' `OptionalColor`: an object with no `color` at all means the run is
/// deliberately set to no colour, which is not the same as inheriting one.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireOptionalColor {
    color: Option<WireColor>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireColor {
    rgb_color: WireRgbColor,
}

/// Each channel is a fraction of one, and a channel that is zero is left out
/// of the JSON altogether, so an absent channel is none of that colour.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireRgbColor {
    red: f32,
    green: f32,
    blue: f32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireWeightedFontFamily {
    font_family: String,
}

impl From<&WireTextStyle> for RunStyle {
    fn from(wire: &WireTextStyle) -> Self {
        Self {
            colour: wire
                .foreground_color
                .as_ref()
                .and_then(|c| c.color.as_ref())
                .map(|c| hex(&c.rgb_color)),
            bold: wire.bold,
            italic: wire.italic,
            font: wire
                .weighted_font_family
                .as_ref()
                .and_then(|f| some(&f.font_family)),
            size: wire.font_size.as_ref().and_then(|d| d.magnitude),
        }
    }
}

/// A Docs colour written the way a caller passes one in: [`rgb`] the other
/// way round, so a colour that went in as `#1a7f37` comes back as `#1a7f37`.
fn hex(colour: &WireRgbColor) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(colour.red),
        channel(colour.green),
        channel(colour.blue)
    )
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
#[serde(rename_all = "camelCase")]
struct BatchUpdate {
    requests: Vec<DocRequest>,
    /// The revision the write was planned against. Google refuses the whole
    /// batch when the document has moved on, which is what keeps a stale
    /// paragraph number from editing the wrong words.
    #[serde(skip_serializing_if = "Option::is_none")]
    write_control: Option<WriteControl>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteControl {
    required_revision_id: String,
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
    delete_content_range: Option<DeleteContentRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_paragraph_style: Option<UpdateParagraphStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_text_style: Option<UpdateTextStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replace_all_text: Option<ReplaceAllText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_inline_image: Option<InsertInlineImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_table: Option<InsertTable>,
}

/// Docs writes a newline of its own before the table it inserts, so the
/// paragraph the caller named keeps its own ending and the table follows it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertTable {
    rows: i64,
    columns: i64,
    location: Location,
}

/// Docs fetches `uri` itself while the batch runs, so the picture has to be
/// reachable from Google for that moment. There is no request that sets alt
/// text on an inserted picture, which is why no tool here offers one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertInlineImage {
    uri: String,
    location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    object_size: Option<ObjectSize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ObjectSize {
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<Dimension>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<Dimension>,
}

/// Docs measures in points and takes the unit with every magnitude.
#[derive(Debug, Serialize)]
struct Dimension {
    magnitude: f64,
    unit: &'static str,
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

/// A half-open span of the document, counted the way Docs counts: in UTF-16
/// code units from the start of the body.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct Range {
    start_index: i64,
    end_index: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteContentRange {
    range: Range,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateParagraphStyle {
    range: Range,
    paragraph_style: ParagraphStyle,
    /// Only the named style is written. Alignment, spacing and indentation
    /// are left exactly as the person set them.
    fields: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ParagraphStyle {
    named_style_type: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateTextStyle {
    range: Range,
    text_style: TextStyle,
    /// The fields this request writes, comma-separated, which is how Google
    /// tells a value that was set from one that was left alone.
    fields: String,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct TextStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    weighted_font_family: Option<WeightedFontFamily>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground_color: Option<OptionalColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    italic: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WeightedFontFamily {
    font_family: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OptionalColor {
    color: Color,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Color {
    rgb_color: RgbColor,
}

/// Docs takes each channel as a fraction of one, not as a byte.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RgbColor {
    red: f32,
    green: f32,
    blue: f32,
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
