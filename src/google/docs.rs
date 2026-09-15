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
//! Structural edits are narrow. Nothing here moves a paragraph, so a model
//! that can insert, replace inside one paragraph and set a named style cannot
//! rearrange someone's article by accident. The one structure it may add is a
//! table, whole with [`apply_table`] or a row at a time with
//! [`apply_insert_row_or_column`]. Those two are the writes in this module
//! that send two batches, for the reason the comment on [`apply_table`] gives
//! at length.
//!
//! Three writes take content away, and they are the only ones here that
//! destroy anything: [`plan_delete`] removes whole paragraphs,
//! [`plan_delete_table`] removes a table and [`plan_delete_row_or_column`]
//! removes one row or one column of one. Docs refuses several deletions
//! outright — a segment's last newline, half a table, the break in front of
//! one, the last row of a table — and this module answers each of those in
//! its own words before a request is built.

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

    /// [`Paragraph::check_expect`] where the paragraph may hold no text at
    /// all. A blank line is a thing people delete, and every cell of a table
    /// that has just been made is empty; a caller cannot quote words that are
    /// not there, so an empty `expect` is how it says the paragraph is blank.
    ///
    /// This is still a lock and not a way past one. An empty `expect` against
    /// a paragraph that does hold text falls through to [`check_expect`],
    /// which refuses it.
    pub fn check_expect_blank(&self, expect: &str) -> Result<()> {
        if self.text.trim().is_empty() && expect.trim().is_empty() {
            return Ok(());
        }
        if self.text.trim().is_empty() {
            return Err(Error::Unsupported(format!(
                "paragraph {} holds no text, so it does not start with {expect:?}; pass an empty \
                 expect for a paragraph that is blank. Nothing was written",
                self.ordinal
            )));
        }
        self.check_expect(expect)
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
    /// Where each body element that is neither a paragraph nor a table
    /// starts: a section break, a table of contents. None of them is
    /// numbered, and Docs still refuses to delete the newline in front of
    /// one, which is the end of the paragraph before it.
    structural: Vec<i64>,
}

/// One table of the document, as Google described it.
#[derive(Debug, Clone, PartialEq)]
struct Grid {
    /// Where the table itself starts, in Docs' own index. The newline in
    /// front of a table belongs to the paragraph before it and not to the
    /// table, which is why deleting that paragraph is its own refusal.
    index: i64,
    /// One past the end of the table. Deleting a table means deleting exactly
    /// `index` to here: the Docs API has no request that removes a table.
    end: i64,
    rows: Vec<Vec<Cell>>,
}

/// One cell, named by its first paragraph. A cell always holds at least one
/// paragraph, and that paragraph is where text written into the cell goes.
#[derive(Debug, Clone, PartialEq)]
struct Cell {
    /// Where that paragraph starts, as Docs counts.
    index: i64,
    /// One past the end of the cell. The last index before it is the cell's
    /// own final newline, which Docs refuses to delete.
    end: i64,
    /// The number `docs_list_paragraphs` gives it.
    ordinal: usize,
    /// The number of the last paragraph of the cell, which is `ordinal` again
    /// for a cell that holds one paragraph.
    last: usize,
    /// True when the cell holds no text at all.
    empty: bool,
}

impl Cell {
    /// Whether this cell is where that paragraph sits.
    fn holds(&self, ordinal: usize) -> bool {
        self.ordinal > 0 && (self.ordinal..=self.last).contains(&ordinal)
    }
}

impl Grid {
    fn cells(&self) -> impl Iterator<Item = &Cell> {
        self.rows.iter().flatten()
    }

    fn columns(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }

    /// The first paragraph number the table covers and the last. Every
    /// paragraph between the two sits in one of its cells.
    fn paragraphs(&self) -> (usize, usize) {
        let first = self.cells().next().map_or(0, |c| c.ordinal);
        let last = self.cells().last().map_or(0, |c| c.last);
        (first, last)
    }

    /// The cells of one row or one column, in document order. The number is
    /// the one a person counts with, from 1.
    fn line(&self, axis: Axis, number: usize) -> Vec<&Cell> {
        let Some(at) = number.checked_sub(1) else {
            return Vec::new();
        };
        match axis {
            Axis::Row => self
                .rows
                .get(at)
                .map(|row| row.iter().collect())
                .unwrap_or_default(),
            Axis::Column => self.rows.iter().filter_map(|row| row.get(at)).collect(),
        }
    }

    /// Whether this table is where that paragraph sits.
    fn holds(&self, ordinal: usize) -> bool {
        let (first, last) = self.paragraphs();
        first > 0 && (first..=last).contains(&ordinal)
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

    /// The last index anything may be deleted at. The body ends with a
    /// newline of Docs' own and a document must keep one, so a delete that
    /// would run to or past it is refused.
    fn last_deletable(&self) -> i64 {
        (self.end_index - 1).max(1)
    }

    /// The innermost table cell a paragraph sits in, or `None` when it is
    /// body text. Innermost, because a table cell may hold a table.
    fn cell_of(&self, ordinal: usize) -> Option<&Cell> {
        self.tables
            .iter()
            .flat_map(Grid::cells)
            .filter(|cell| cell.holds(ordinal))
            .min_by_key(|cell| cell.last - cell.ordinal)
    }

    /// The innermost table a paragraph sits in.
    fn table_of(&self, ordinal: usize) -> Option<&Grid> {
        self.tables
            .iter()
            .filter(|grid| grid.holds(ordinal))
            .min_by_key(|grid| {
                let (first, last) = grid.paragraphs();
                last - first
            })
    }

    /// Everything Docs itself refuses to delete, refused here first.
    ///
    /// The rules are the Docs API's own: a segment keeps the newline that
    /// ends it, a table goes whole or not at all, and the newline in front of
    /// a table goes only when the table goes with it. Each one comes back
    /// from Google as a 400 that names an index and no paragraph, which is
    /// nothing a model can act on, so each one is a sentence here instead.
    fn check_deletable(&self, from: usize, to: usize, end: i64) -> Result<()> {
        // A range that opens in one cell and closes in another — or opens in
        // the body and closes inside a table — would take some rows of a
        // table and leave others, and there is no such document.
        let (opens, closes) = (self.cell_of(from), self.cell_of(to));
        if opens.map(|cell| cell.index) != closes.map(|cell| cell.index) {
            let (first, last) = self
                .table_of(from)
                .or_else(|| self.table_of(to))
                .map_or((0, 0), Grid::paragraphs);
            return Err(Error::Unsupported(format!(
                "paragraphs {from} to {to} would cut a table in half, which Docs refuses: the \
                 table covers paragraphs {first} to {last}. Widen the range past paragraph \
                 {last} to take the whole table with it, narrow it to paragraphs inside one \
                 cell, or delete the table on its own with docs_delete_table. Nothing was \
                 written"
            )));
        }
        if let Some(cell) = opens
            && end >= cell.end
        {
            return Err(Error::Unsupported(format!(
                "paragraph {to} is the last paragraph of its table cell, and a cell must keep \
                 one, so Docs refuses to delete it. Empty it with docs_edit_paragraph, or take \
                 the whole table with docs_delete_table. Nothing was written"
            )));
        }
        if opens.is_none() && end > self.last_deletable() {
            return Err(Error::Unsupported(format!(
                "paragraph {to} is the last paragraph of the document, and the break that ends \
                 it is the one Docs will not delete: a document must end with one. Leave it out \
                 of the range and empty it with docs_edit_paragraph instead. Nothing was written"
            )));
        }
        if let Some(grid) = self.tables.iter().find(|grid| grid.index == end) {
            let (first, last) = grid.paragraphs();
            return Err(Error::Unsupported(format!(
                "paragraph {to} is the paragraph in front of a table, and Docs refuses to delete \
                 the break in front of a table unless the table goes with it. Widen the range to \
                 paragraph {last} so the table goes too — its cells are paragraphs {first} to \
                 {last} — or leave paragraph {to} out of the range. Nothing was written"
            )));
        }
        if self.structural.contains(&end) {
            return Err(Error::Unsupported(format!(
                "paragraph {to} is the paragraph in front of a section break, and Docs refuses \
                 to delete the break in front of one. Leave paragraph {to} out of the range, or \
                 empty it with docs_edit_paragraph. Nothing was written"
            )));
        }
        Ok(())
    }

    /// Where an insert goes and what is written there. A paragraph is text
    /// that ends in a newline, so an insert carries its own break: before a
    /// paragraph the text is followed by one, and after the document's last
    /// paragraph it is preceded by one instead, because the index after the
    /// body's final newline is the one place Docs refuses to write at.
    ///
    /// The third value is where the caller's own text begins, which is what a
    /// style or a colour is measured from.
    /// Where a whole element goes when it follows a paragraph: the boundary
    /// after that paragraph's break.
    ///
    /// Text may not be written there — [`Self::insertion`] says why — but a
    /// table is not text. It is an element of the body in its own right, and
    /// writing it inside a paragraph would split that paragraph in two rather
    /// than put a table after it.
    fn boundary(&self, paragraph: &Paragraph) -> i64 {
        paragraph.end_index.max(1)
    }

    fn insertion(&self, at: At, paragraph: &Paragraph, body: &str) -> (i64, String, i64) {
        match at {
            At::Before(_) => (
                paragraph.start_index,
                format!("{body}\n"),
                paragraph.start_index,
            ),
            // Always inside the paragraph named, never at the index after
            // its break. That index belongs to no paragraph when the next
            // thing is a table or a section break, and Docs refuses to write
            // there — a caption above a table is the ordinary way to meet it.
            // Writing the break first and the text after it puts the same
            // characters in the same order, at an index that always exists.
            At::After(_) => {
                let index = (paragraph.end_index - 1).max(1);
                (index, format!("\n{body}"), index + 1)
            }
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
    /// The table a row or a column has just gone into: the one that still
    /// starts where it did, now with the shape the edit asked for.
    ///
    /// A structural edit leaves the table's own start index alone, so that
    /// index names it again in the document read back, and the shape says the
    /// edit is the one that landed.
    fn changed_table(&self, index: i64, rows: usize, columns: usize) -> Option<&Grid> {
        self.tables.iter().find(|grid| {
            grid.index == index
                && grid.rows.len() == rows
                && grid.rows.iter().all(|row| row.len() == columns)
        })
    }

    /// The paragraphs these cells hold, numbered as the listing numbers them.
    /// A cell may hold more than one paragraph, so this walks each cell's own
    /// range rather than taking one paragraph per cell.
    fn cell_paragraphs(&self, cells: &[&Cell]) -> Vec<(usize, String)> {
        cells
            .iter()
            .filter(|cell| cell.ordinal > 0)
            .flat_map(|cell| cell.ordinal..=cell.last)
            .filter_map(|ordinal| self.paragraphs.get(ordinal - 1))
            .map(|p| (p.ordinal, p.text.clone()))
            .collect()
    }

    /// Whether the break that ends this paragraph may still be deleted, now
    /// that the table is in. The plan asked the same question of the document
    /// the caller read; this asks it again of the document this server read
    /// back, and adds what only the second read can say: that the paragraph
    /// still ends where the plan measured, and that the newline Docs wrote of
    /// its own stands between that break and the table. Anything else and the
    /// break stays where it is.
    fn tidy_before(&self, ordinal: usize, index: i64, table: i64) -> bool {
        table == index + 1
            && self.paragraph(ordinal).is_ok_and(|p| p.end_index == index)
            && self.check_deletable(ordinal, ordinal, index).is_ok()
    }

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
    let mut structural = Vec::new();
    collect_paragraphs(
        &wire.body.content,
        false,
        &mut paragraphs,
        &mut tables,
        &mut structural,
    );
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
        structural,
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
    structural: &mut Vec<i64>,
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
                    collect_paragraphs(&cell.content, true, out, tables, structural);
                    cells.push(Cell {
                        index: out.get(at).map_or(0, |p| p.start_index),
                        end: cell.end_index.unwrap_or_default(),
                        ordinal: out.get(at).map_or(0, |p| p.ordinal),
                        last: out.last().map_or(0, |p| p.ordinal),
                        empty: out[at..].iter().all(|p| p.text.is_empty()),
                    });
                }
                rows.push(cells);
            }
            tables.push(Grid {
                index: element.start_index.unwrap_or_default(),
                end: element.end_index.unwrap_or_default(),
                rows,
            });
        }
        // A section break or a table of contents is neither, and is not
        // numbered; it is noted because a delete may not end in front of one.
        if element.paragraph.is_none() && element.table.is_none() {
            structural.push(element.start_index.unwrap_or_default());
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
    requests.push(delete_request(start + shift, end + shift));
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

/// A point size Docs will take. The API refuses nothing here and simply
/// renders what it is given, so an 800 pt listing would arrive as an 800 pt
/// listing; the bounds are this server's, and they are the ones a magazine
/// page can hold.
fn font_size(points: f64) -> Result<Dimension> {
    if !points.is_finite() || !(1.0..=400.0).contains(&points) {
        return Err(Error::Unsupported(format!(
            "{points} is not a font size this writes; give a size between 1 and 400 points"
        )));
    }
    Ok(Dimension {
        magnitude: points,
        unit: "PT",
    })
}

/// How a listing is set: the monospace font and the point size, both over
/// the whole of it. They travel together because they are written together,
/// in the one style request that covers the listing.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeStyle<'a> {
    pub font: Option<&'a str>,
    pub size_pt: Option<f64>,
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
    style: CodeStyle<'_>,
    spans: &[Span],
    expect: Option<&str>,
) -> Result<Plan> {
    let CodeStyle { font, size_pt } = style;
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
    let size = size_pt;
    let size_pt = size_pt.map(font_size).transpose()?;
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
                    font_size: size_pt,
                    ..TextStyle::default()
                },
                // The size travels with the font: one style over the whole
                // listing, so a listing is never half the size it asked for.
                fields: match size {
                    Some(_) => "weightedFontFamily,fontSize",
                    None => "weightedFontFamily",
                }
                .to_string(),
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

fn delete_request(start: i64, end: i64) -> DocRequest {
    DocRequest {
        delete_content_range: Some(DeleteContentRange {
            range: Range {
                start_index: start,
                end_index: end,
            },
        }),
        ..DocRequest::default()
    }
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
    /// Whether the table ends up directly under the paragraph it follows.
    ///
    /// Docs writes a newline of its own in front of a table, so an insert on
    /// its own leaves an empty paragraph between the text and the grid. That
    /// newline is the one Docs refuses to delete, so the second batch deletes
    /// the break that ends the named paragraph instead. Where even that break
    /// may not go, this is false: the table is still written and the empty
    /// paragraph stays.
    pub tidy: bool,
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
    /// Whether the empty paragraph Docs writes in front of a table was closed
    /// up, so the table sits directly under the paragraph it follows. When
    /// this is false an empty paragraph is left there, and the answer says so.
    pub tidy: bool,
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
    // newline of its own before a table, so this insert carries none — and
    // the table goes at the paragraph boundary rather than inside the
    // paragraph, which is the one place these two writes differ.
    let index = outline.boundary(neighbour);
    // That newline of Docs' own would stand between the text and the grid as
    // an empty paragraph, and it is the one newline Docs will not let anything
    // delete. The break that ends the named paragraph is a different
    // character, one index lower, and deleting it merges the two paragraphs:
    // the text ends up directly on top of the table and the newline in front
    // of the table is never touched. Whether that break may go is
    // `check_deletable`'s question and is asked of the document as the caller
    // read it. A paragraph that Docs already refuses to shorten there — the
    // last of a table cell, one in front of another table — keeps its break
    // and its empty paragraph: the shape a table takes next to one of those is
    // not a shape this server has measured, and it guesses at none of them.
    let tidy = index > 1
        && outline
            .check_deletable(neighbour.ordinal, neighbour.ordinal, index)
            .is_ok();
    Ok(TablePlan {
        paragraph: neighbour.ordinal,
        before: neighbour.text.clone(),
        rows: height,
        columns: width,
        header,
        tidy,
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
            check_one_line(
                text,
                &format!("the cell in row {}, column {}", at + 1, cell + 1),
            )?;
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

/// A cell is one line of plain text. A break in one would make a second
/// paragraph inside the cell and move every paragraph number the answer
/// reports, so it is refused before anything is sent.
fn check_one_line(text: &str, cell: &str) -> Result<()> {
    if text.contains('\n') || text.contains('\r') {
        return Err(Error::Unsupported(format!(
            "{cell} holds a line break; a cell here is one line of plain text. Nothing was \
             written"
        )));
    }
    Ok(())
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
/// The second batch ends with one delete: the break that ends the paragraph
/// the table follows. Docs writes a newline of its own in front of a table,
/// and that newline is the one it refuses to delete, so the empty paragraph it
/// makes is closed up from the other side — the two paragraphs merge and the
/// text ends up directly on top of the grid. It goes last because it is the
/// one request at a lower index than the cells. Where that break may not go
/// either, [`TablePlan::tidy`] is false, the delete is left out and the table
/// is written all the same.
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
        paragraph,
        rows,
        columns,
        header,
        tidy,
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
    let tidy = tidy && after.tidy_before(paragraph, index, table.index);
    let mut requests = fill_requests(table, &grid, header);
    if tidy {
        // Last of all, because it is the only request here at a lower index
        // than the cells: doing it last leaves every index the cells were
        // computed from exactly where Google put it. One character goes, the
        // break that ends the paragraph the table follows, and the paragraph
        // Docs wrote in front of the table closes the text off instead.
        requests.push(delete_request(index - 1, index));
    }
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
    // That delete merges two paragraphs into one, so everything after it is
    // numbered one lower than the read this server made, and the table itself
    // starts one character earlier.
    let closed = usize::from(tidy);
    Ok(InsertedTable {
        rows,
        columns,
        first_paragraph: first_paragraph.saturating_sub(closed),
        last_paragraph: last_paragraph.saturating_sub(closed),
        index: table.index - i64::from(tidy),
        tidy,
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

// ----- a row or a column of a table ------------------------------------------

/// Which way round a table is edited. A row runs across the table and holds
/// one cell for each column; a column runs down it and holds one for each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Row,
    Column,
}

impl Axis {
    /// The word for one of them, for a person to read.
    pub fn one(self) -> &'static str {
        match self {
            Axis::Row => "row",
            Axis::Column => "column",
        }
    }

    /// The word for several of them.
    pub fn many(self) -> &'static str {
        match self {
            Axis::Row => "rows",
            Axis::Column => "columns",
        }
    }

    /// "1 row", "2 rows".
    fn counted(self, n: usize) -> String {
        format!("{n} {}", if n == 1 { self.one() } else { self.many() })
    }

    /// The other way round. A row is as long as the table has columns, which
    /// is the one place this matters.
    fn across(self) -> Self {
        match self {
            Axis::Row => Axis::Column,
            Axis::Column => Axis::Row,
        }
    }

    /// How many of these a table of this shape has.
    fn count(self, rows: usize, columns: usize) -> usize {
        match self {
            Axis::Row => rows,
            Axis::Column => columns,
        }
    }

    /// The shape the table has once one more of these has gone in.
    fn grown(self, rows: usize, columns: usize) -> (usize, usize) {
        match self {
            Axis::Row => (rows + 1, columns),
            Axis::Column => (rows, columns + 1),
        }
    }

    /// The shape it has once one of them has come out.
    fn shrunk(self, rows: usize, columns: usize) -> (usize, usize) {
        match self {
            Axis::Row => (rows.saturating_sub(1), columns),
            Axis::Column => (rows, columns.saturating_sub(1)),
        }
    }
}

/// A row or a column on its way into a table that is already there, and the
/// text that goes in its cells afterwards.
///
/// The cells are not in the plan's requests, for the reason [`apply_table`]
/// gives: an insert makes cells that do not exist yet, and where they are is
/// Google's answer to the write that makes them.
#[derive(Debug)]
pub struct TableInsertPlan {
    /// The cell the caller named the table by.
    pub paragraph: usize,
    pub axis: Axis,
    /// Which row or column of the table the new one becomes, counting from 1.
    pub number: usize,
    /// The table's shape as it stands.
    pub rows: usize,
    pub columns: usize,
    /// Its shape once this has gone in.
    pub new_rows: usize,
    pub new_columns: usize,
    /// The text for the new cells, one to a cell, and empty for a row or a
    /// column that goes in blank.
    pub cells: Vec<String>,
    /// Where the table starts, in Docs' own index. A structural edit leaves
    /// that index where it is, so it is what finds the table again in the
    /// document this server reads back.
    index: i64,
    revision_id: String,
    requests: Vec<DocRequest>,
}

impl TableInsertPlan {
    /// The new cells as one line, for a person to read before it is written.
    pub fn line(&self) -> String {
        self.cells.join(" | ")
    }
}

/// A row or a column on its way out of a table, with everything in its cells.
#[derive(Debug)]
pub struct TableDeletePlan {
    /// The cell the caller named the table by.
    pub paragraph: usize,
    pub axis: Axis,
    /// Which row or column goes, counting from 1.
    pub number: usize,
    /// The table's shape as it stands.
    pub rows: usize,
    pub columns: usize,
    /// Its shape once this has come out.
    pub new_rows: usize,
    pub new_columns: usize,
    /// Every paragraph that goes: its number, and how it reads now. A cell
    /// may hold more than one paragraph, so this is not one line per cell.
    pub going: Vec<(usize, String)>,
    revision_id: String,
    requests: Vec<DocRequest>,
}

impl TableDeletePlan {
    /// How many cells go with it: a row holds one for each column.
    pub fn cells(&self) -> usize {
        self.axis.across().count(self.rows, self.columns)
    }
}

/// Where a new row or column landed, once both batches have gone through.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertedCells {
    /// The table's shape now.
    pub rows: usize,
    pub columns: usize,
    /// The first paragraph of each new cell, as `docs_list_paragraphs`
    /// numbers them, which is where text goes into that cell.
    pub paragraphs: Vec<usize>,
}

/// A row under the row a caller counted, or a column to the right of the
/// column they counted, in a table that is already there.
///
/// This is why the tool exists: a table edited this way keeps its column
/// widths, its borders and everything else somebody set by hand in Docs,
/// which deleting the table and writing it again throws away.
pub fn plan_insert_row_or_column(
    outline: &Outline,
    revision_id: &str,
    axis: Axis,
    paragraph: usize,
    after: usize,
    cells: &[String],
    expect: &str,
) -> Result<TableInsertPlan> {
    outline.check_revision(revision_id)?;
    outline.paragraph(paragraph)?.check_expect_blank(expect)?;
    let grid = table_at(outline, paragraph, axis)?;
    let (rows, columns) = (grid.rows.len(), grid.columns());
    check_number(axis, after, axis.count(rows, columns))?;
    check_cells(axis, cells, axis.across().count(rows, columns))?;
    let (new_rows, new_columns) = axis.grown(rows, columns);
    // Docs locates a structural edit by a cell of the table, and the new row
    // goes below that cell's row. Which cell of the row it is makes no
    // difference, so it is the first one.
    let request = match axis {
        Axis::Row => DocRequest {
            insert_table_row: Some(InsertTableRow {
                table_cell_location: TableCellLocation::at(grid.index, after, 1),
                insert_below: true,
            }),
            ..DocRequest::default()
        },
        Axis::Column => DocRequest {
            insert_table_column: Some(InsertTableColumn {
                table_cell_location: TableCellLocation::at(grid.index, 1, after),
                insert_right: true,
            }),
            ..DocRequest::default()
        },
    };
    Ok(TableInsertPlan {
        paragraph,
        axis,
        number: after + 1,
        rows,
        columns,
        new_rows,
        new_columns,
        cells: cells.to_vec(),
        index: grid.index,
        revision_id: revision_id.trim().to_string(),
        requests: vec![request],
    })
}

/// One row or one column out of a table, named by any cell of that table.
///
/// Docs deletes the whole table when its last row or its last column goes,
/// which is not what this was asked for, so that is refused here and
/// docs_delete_table is named instead.
pub fn plan_delete_row_or_column(
    outline: &Outline,
    revision_id: &str,
    axis: Axis,
    paragraph: usize,
    number: usize,
    expect: &str,
) -> Result<TableDeletePlan> {
    outline.check_revision(revision_id)?;
    outline.paragraph(paragraph)?.check_expect_blank(expect)?;
    let grid = table_at(outline, paragraph, axis)?;
    let (rows, columns) = (grid.rows.len(), grid.columns());
    let have = axis.count(rows, columns);
    if have <= 1 {
        return Err(Error::Unsupported(format!(
            "this table has one {} left, and Docs takes the whole table away when the last one \
             goes. Delete the table itself with docs_delete_table, which says how many rows and \
             columns go before it does it. Nothing was written",
            axis.one()
        )));
    }
    check_number(axis, number, have)?;
    let (new_rows, new_columns) = axis.shrunk(rows, columns);
    let location = match axis {
        Axis::Row => TableCellLocation::at(grid.index, number, 1),
        Axis::Column => TableCellLocation::at(grid.index, 1, number),
    };
    let request = match axis {
        Axis::Row => DocRequest {
            delete_table_row: Some(DeleteTableRow {
                table_cell_location: location,
            }),
            ..DocRequest::default()
        },
        Axis::Column => DocRequest {
            delete_table_column: Some(DeleteTableColumn {
                table_cell_location: location,
            }),
            ..DocRequest::default()
        },
    };
    Ok(TableDeletePlan {
        paragraph,
        axis,
        number,
        rows,
        columns,
        new_rows,
        new_columns,
        going: outline.cell_paragraphs(&grid.line(axis, number)),
        revision_id: revision_id.trim().to_string(),
        requests: vec![request],
    })
}

/// The table a caller named by one of its cells, or the refusal that says a
/// paragraph of the body is not one.
fn table_at(outline: &Outline, paragraph: usize, axis: Axis) -> Result<&Grid> {
    outline.table_of(paragraph).ok_or_else(|| {
        Error::Unsupported(format!(
            "paragraph {paragraph} is not inside a table, so it has no {} to change: it is body \
             text. docs_list_paragraphs marks the paragraphs that are in one with in_table, and \
             this tool takes any cell of the table you mean. Nothing was written",
            axis.many()
        ))
    })
}

/// The row or the column a caller counted, against what the table holds. Both
/// are counted from 1 here, the way a person counts them and the way every
/// other ordinal in these tools works.
fn check_number(axis: Axis, number: usize, have: usize) -> Result<()> {
    if number == 0 {
        return Err(Error::Unsupported(format!(
            "the {} of a table are numbered from 1, so there is no {} 0. Nothing was written",
            axis.many(),
            axis.one()
        )));
    }
    if number > have {
        return Err(Error::Unsupported(format!(
            "this table has {}, so there is no {} {number}. Nothing was written",
            axis.counted(have),
            axis.one()
        )));
    }
    Ok(())
}

/// The text for a new row or column against the shape of the table. A row
/// holds one cell for each column, so a row's text has to have as many
/// entries as the table has columns; anything else is a miscount, and the
/// refusal names both numbers. No text at all is a row that goes in empty.
fn check_cells(axis: Axis, cells: &[String], wanted: usize) -> Result<()> {
    if cells.is_empty() {
        return Ok(());
    }
    if cells.len() != wanted {
        return Err(Error::Unsupported(format!(
            "the new {} holds {} cell{}, and this table has {}: a {} takes one cell for each {}. \
             Leave `cells` out for an empty {}. Nothing was written",
            axis.one(),
            cells.len(),
            if cells.len() == 1 { "" } else { "s" },
            axis.across().counted(wanted),
            axis.one(),
            axis.across().one(),
            axis.one()
        )));
    }
    for (at, text) in cells.iter().enumerate() {
        check_one_line(text, &format!("cell {} of the new {}", at + 1, axis.one()))?;
    }
    Ok(())
}

/// The second write in this module that sends two batches, for the reason
/// [`apply_table`] gives at length: an insert makes empty cells, and where
/// they are is Google's answer to the write that made them rather than
/// anything a formula here should guess.
///
/// So the row goes in, the document is read again, and the cells are filled
/// at the indexes that read answered with — from the last to the first, so
/// that no insert moves an index still to be used — in a batch carrying the
/// revision of that read.
///
/// When the second batch fails, the row is there and empty. The answer says
/// exactly that and names the paragraph numbers its cells now have.
pub async fn apply_insert_row_or_column(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    plan: TableInsertPlan,
) -> Result<InsertedCells> {
    let TableInsertPlan {
        axis,
        number,
        new_rows,
        new_columns,
        cells,
        index,
        revision_id,
        requests,
        ..
    } = plan;
    batch(client, connection_id, document_id, requests, revision_id)
        .await
        .map_err(stale)?;
    // From here a table in somebody's document has a row it did not have, so
    // every refusal below says so rather than reading as though nothing had
    // happened.
    let after = outline(client, connection_id, document_id)
        .await
        .map_err(|e| {
            Error::Unsupported(format!(
                "{}, and reading the document back to find its cells failed: {e}. Call \
                 docs_list_paragraphs to see where it is",
                added(axis, number)
            ))
        })?;
    let new = after
        .changed_table(index, new_rows, new_columns)
        .map(|grid| grid.line(axis, number))
        .filter(|cells| !cells.is_empty() && cells.iter().all(|c| c.empty && c.ordinal > 0))
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "{}, and this server could not find its empty cells in the document it read \
                 back, so nothing was written into them. Call docs_list_paragraphs to see where \
                 it is",
                added(axis, number)
            ))
        })?;
    let paragraphs: Vec<usize> = new.iter().map(|cell| cell.ordinal).collect();
    let requests = fill_cells(&new, &cells);
    if !requests.is_empty() {
        batch(
            client,
            connection_id,
            document_id,
            requests,
            after.revision_id.clone(),
        )
        .await
        .map_err(|e| empty_cells(axis, number, &paragraphs, e))?;
    }
    Ok(InsertedCells {
        rows: new_rows,
        columns: new_columns,
        paragraphs,
    })
}

/// The one `batchUpdate` a row or a column delete sends, against the revision
/// it was planned for. Docs has a request for this one, so it is one request
/// and one batch, where deleting a whole table is a span.
pub async fn apply_delete_row_or_column(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    plan: TableDeletePlan,
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

/// The requests that fill the new cells, in reverse document order, so that
/// each insert leaves the indexes of the ones still to come where they were.
/// An empty cell is skipped: Docs refuses an insert of no text, and empty is
/// what the cell already is.
fn fill_cells(cells: &[&Cell], texts: &[String]) -> Vec<DocRequest> {
    cells
        .iter()
        .zip(texts)
        .rev()
        .filter(|(_, text)| !text.is_empty())
        .map(|(cell, text)| insert_request(cell.index, text))
        .collect()
}

/// What the first batch did, in the words every refusal after it starts with.
fn added(axis: Axis, number: usize) -> String {
    format!(
        "the new {} is {} {number} of the table and is empty",
        axis.one(),
        axis.one()
    )
}

/// The second batch failed, so the row stands there with nothing in it. The
/// answer says that, says where its cells are in numbers the next call can
/// use, and says what to do about it.
fn empty_cells(axis: Axis, number: usize, paragraphs: &[usize], e: Error) -> Error {
    Error::Unsupported(format!(
        "{}: its cells are paragraphs {} of the document as it stands now, and none of the text \
         was written into them. Fill a cell with docs_insert_text before_paragraph=<that \
         number>, or take the {} out again with docs_delete_table_{}. Read the document with \
         docs_list_paragraphs first, because the numbers here are from this server's own read \
         and the write that failed may have been refused because somebody else was editing. \
         Google refused the second write: {e}",
        added(axis, number),
        numbered(paragraphs),
        axis.one(),
        axis.one()
    ))
}

/// A few numbers, for a sentence that names them: "5, 6".
fn numbered(numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// ----- taking content away ---------------------------------------------------

/// Content on its way out of a document: which paragraphs go, how they read
/// now, and the one `deleteContentRange` that takes them all together.
///
/// Both delete tools build this, so both are refused for the same reasons in
/// the same words, and a preview that comes back is a delete that would go
/// through. A range is one request over the whole span and never one request
/// per paragraph: Docs applies a batch in order, so a second request would
/// name indexes the first one had already moved.
#[derive(Debug)]
pub struct DeletePlan {
    /// The first paragraph the delete takes, as `docs_list_paragraphs`
    /// numbers them.
    pub from: usize,
    /// The last one, included.
    pub to: usize,
    /// Every paragraph that goes: its number, and how it reads now. This is
    /// what the preview lists.
    pub going: Vec<(usize, String)>,
    /// The tables that go whole, as rows by columns. A range that swallows a
    /// table takes the table with it, and the person has to be told that.
    pub tables: Vec<(usize, usize)>,
    /// The span that is deleted, in Docs' own index.
    pub start: i64,
    pub end: i64,
    revision_id: String,
    requests: Vec<DocRequest>,
}

/// Whole paragraphs, `from` to `to` and both included, with the paragraph
/// break that ends each one, so nothing is left behind as an empty line.
///
/// Both ends of a range are confirmed by the caller: `expect` for the first
/// paragraph and `expect_last` for the last. A miscount on an insert is undone
/// by deleting what was inserted; a miscount here takes a section of somebody's
/// article with it and nothing on this side can put it back.
pub fn plan_delete(
    outline: &Outline,
    revision_id: &str,
    from: usize,
    to: Option<usize>,
    expect: &str,
    expect_last: Option<&str>,
) -> Result<DeletePlan> {
    outline.check_revision(revision_id)?;
    let to = to.unwrap_or(from);
    if to < from {
        return Err(Error::Unsupported(format!(
            "the range runs backwards: `from` is {from} and `to` is {to}. `from` is the first \
             paragraph to delete and `to` the last, and both of them go"
        )));
    }
    let first = outline.paragraph(from)?;
    let last = outline.paragraph(to)?;
    first.check_expect_blank(expect)?;
    match expect_last {
        Some(expect_last) => last.check_expect_blank(expect_last)?,
        None if to != from => {
            return Err(Error::Unsupported(format!(
                "deleting paragraphs {from} to {to} needs `expect_last` as well: pass what \
                 paragraph {to} starts with, as docs_list_paragraphs reports it. Both ends of a \
                 range are confirmed here, because a range is where a miscount takes a whole \
                 section with it and nothing here can put it back"
            )));
        }
        None => {}
    }
    let (start, end) = (first.start_index, last.end_index);
    outline.check_deletable(from, to, end)?;
    Ok(DeletePlan {
        from,
        to,
        going: going(outline, from, to),
        tables: outline
            .tables
            .iter()
            .filter(|grid| start <= grid.index && grid.end <= end)
            .map(|grid| (grid.rows.len(), grid.columns()))
            .collect(),
        start,
        end,
        revision_id: revision_id.trim().to_string(),
        requests: vec![delete_request(start, end)],
    })
}

/// The whole table one paragraph sits in. Any cell of it names it.
///
/// The Docs API has no request that removes a table, so a table is deleted by
/// deleting its own span, exactly as Google's own guide says. That span starts
/// at the table and not at the newline in front of it, so the paragraph before
/// the table is left where it is.
pub fn plan_delete_table(
    outline: &Outline,
    revision_id: &str,
    paragraph: usize,
    expect: &str,
) -> Result<DeletePlan> {
    outline.check_revision(revision_id)?;
    outline.paragraph(paragraph)?.check_expect_blank(expect)?;
    let Some(grid) = outline.table_of(paragraph) else {
        return Err(Error::Unsupported(format!(
            "paragraph {paragraph} is not inside a table, so there is no table to delete: it is \
             body text. docs_list_paragraphs marks the paragraphs that are in one with in_table, \
             and this tool takes any cell of the table you mean. Nothing was written"
        )));
    };
    let (from, to) = grid.paragraphs();
    Ok(DeletePlan {
        from,
        to,
        going: going(outline, from, to),
        tables: vec![(grid.rows.len(), grid.columns())],
        start: grid.index,
        end: grid.end,
        revision_id: revision_id.trim().to_string(),
        requests: vec![delete_request(grid.index, grid.end)],
    })
}

/// The paragraphs a delete takes, numbered as the listing numbers them.
fn going(outline: &Outline, from: usize, to: usize) -> Vec<(usize, String)> {
    outline.paragraphs[from - 1..to]
        .iter()
        .map(|p| (p.ordinal, p.text.clone()))
        .collect()
}

/// The one `batchUpdate` a delete sends, against the revision it was planned
/// for. A document that moved in between refuses the batch rather than
/// deleting whatever now stands at those indexes.
pub async fn apply_delete(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    plan: DeletePlan,
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
    /// One past the end of the cell, which is where its own last newline
    /// sits. A delete that would take that newline is refused.
    end_index: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_table_row: Option<InsertTableRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_table_column: Option<InsertTableColumn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delete_table_row: Option<DeleteTableRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delete_table_column: Option<DeleteTableColumn>,
}

/// Where in a table a structural edit happens: the table itself, and one cell
/// of it. Docs counts the row and the column from 0 here and every tool of
/// this server counts them from 1, so [`TableCellLocation::at`] is the one
/// place the two meet.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TableCellLocation {
    table_start_location: Location,
    row_index: i64,
    column_index: i64,
}

impl TableCellLocation {
    /// The cell a person counted, as Docs counts it.
    fn at(index: i64, row: usize, column: usize) -> Self {
        Self {
            table_start_location: Location { index },
            row_index: row as i64 - 1,
            column_index: column as i64 - 1,
        }
    }
}

/// The new row goes below the cell's own row, never above it, because the
/// tool takes the number of the row to put it under.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertTableRow {
    table_cell_location: TableCellLocation,
    insert_below: bool,
}

/// The same the other way round: to the right of the cell's own column.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertTableColumn {
    table_cell_location: TableCellLocation,
    insert_right: bool,
}

/// The row the cell sits in goes, with everything in its cells. Docs deletes
/// the whole table when this is its last row, which is why that is refused
/// before the request is built.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteTableRow {
    table_cell_location: TableCellLocation,
}

/// The column the cell sits in, likewise.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteTableColumn {
    table_cell_location: TableCellLocation,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    font_size: Option<Dimension>,
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
