//! The Docs tools: the whole document as markdown, the document as numbered
//! paragraphs, and the writes.
//!
//! There are two kinds of write here, and they are not rivals. docs_create,
//! docs_append and docs_replace_text are the broad ones: make a document, add
//! to the end, change every match. docs_insert_text, docs_edit_paragraph,
//! docs_style_paragraph, docs_insert_code and docs_insert_table are the
//! careful ones, and each takes a paragraph number and the revision id that
//! docs_list_paragraphs answered with — the two locks that keep a write off
//! the paragraph it was not meant for.
//!
//! Every write takes `confirmed`. With `confirmed=false` nothing is written
//! and the answer says what would change, the affected paragraph as it is and
//! as it would read. That is the step where the person sees the change and
//! agrees to it.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::gmail::link_out;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text};
use crate::domain::image;
use crate::domain::limits::{INSERT_IMAGE_MAX_BYTES, INSERT_IMAGE_MAX_PIXELS};
use crate::domain::scope::Service;
use crate::google::{docs, drive, text};
use crate::http::links::{self, NewDownload, Target};
use crate::http::uploads;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsReadParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The document id, the long string in its Docs URL
    pub doc_id: String,
    /// Stop after this many characters, with a notice saying what was cut
    pub max_chars: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsImagesParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The document id, the long string in its Docs URL
    pub doc_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsImageParam {
    pub account: String,
    pub doc_id: String,
    /// Which picture: the label docs_list_images gives it and the text of the
    /// document is left with, e.g. "image1", or the object id
    pub image: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsCreateParam {
    pub account: String,
    pub title: String,
    /// The document's content as markdown; Drive converts it to a real Doc,
    /// so headings, lists and emphasis come out formatted
    pub markdown: String,
    /// The Drive folder to put it in; the account's root by default
    pub folder_id: Option<String>,
    /// Must be true to write. Call with false first, show the person the title
    /// and the content, and pass true only after they agree.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsAppendParam {
    pub account: String,
    pub doc_id: String,
    /// Plain text added at the end of the document. This is not rendered as
    /// markdown: a "# " arrives as those two characters.
    pub text: String,
    /// Must be true to write. Call with false first and show the person the text.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsReplaceParam {
    pub account: String,
    pub doc_id: String,
    /// The text to look for, everywhere in the document
    pub find: String,
    /// What to put in its place
    pub replace: String,
    pub match_case: Option<bool>,
    /// Must be true to write. Call with false first and show the person both
    /// strings; this changes every occurrence at once.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsParagraphsParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The document id, the long string in its Docs URL
    pub doc_id: String,
    /// The first paragraph to show, counting from 1; the start of the
    /// document by default
    pub from: Option<u32>,
    /// The last paragraph to show, counting from 1 and included; the end of
    /// the document by default
    pub to: Option<u32>,
    /// Show each paragraph in full rather than cut. Use it with from and to
    /// for the few paragraphs you are about to change.
    pub full: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsFormattingParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// The document id, the long string in its Docs URL
    pub doc_id: String,
    /// One paragraph, as docs_list_paragraphs numbers them. Give this, or
    /// from and to, not both.
    pub paragraph: Option<u32>,
    /// The first paragraph to read, counting from 1; the start of the
    /// document by default
    pub from: Option<u32>,
    /// The last paragraph to read, counting from 1 and included; the end of
    /// the document by default
    pub to: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsInsertParam {
    pub account: String,
    pub doc_id: String,
    /// Put the new paragraph after this one, as docs_list_paragraphs numbers
    /// them. Give this or before_paragraph, not both.
    pub after_paragraph: Option<u32>,
    /// Put the new paragraph before this one instead.
    pub before_paragraph: Option<u32>,
    /// The text of the new paragraph, as plain text. Markdown is not
    /// rendered: a "## " arrives as those characters.
    pub text: String,
    /// The named style for what is inserted: NORMAL_TEXT, TITLE, SUBTITLE or
    /// HEADING_1 to HEADING_6. Without it the new paragraph takes the style
    /// of the one it is put beside.
    pub style: Option<String>,
    /// What the paragraph you named starts with, as docs_list_paragraphs
    /// reports it. Optional here, and worth passing: it catches a paragraph
    /// number that has moved.
    pub expect: Option<String>,
    /// The revision_id docs_list_paragraphs answered with. The write is
    /// refused when the document has changed since.
    pub revision_id: String,
    /// Must be true to write. Call with false first and show the person what
    /// comes back.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsEditParam {
    pub account: String,
    pub doc_id: String,
    /// Which paragraph to change, as docs_list_paragraphs numbers them
    pub paragraph: u32,
    /// The text to look for inside that one paragraph
    pub find: String,
    /// Which match to change, counting from 1: occurrence 2 changes the
    /// second match in the paragraph and leaves the first alone
    pub occurrence: u32,
    /// What to put in its place; empty deletes the match
    pub replace: String,
    /// What the paragraph starts with, as docs_list_paragraphs reports it.
    /// The paragraph is not written to when it says something else.
    pub expect: String,
    /// The revision_id docs_list_paragraphs answered with
    pub revision_id: String,
    /// Must be true to write. Call with false first and show the person the
    /// paragraph as it is and as it would read.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsStyleParam {
    pub account: String,
    pub doc_id: String,
    /// Which paragraph to restyle, as docs_list_paragraphs numbers them
    pub paragraph: u32,
    /// NORMAL_TEXT, TITLE, SUBTITLE or HEADING_1 to HEADING_6
    pub style: String,
    /// What the paragraph starts with, as docs_list_paragraphs reports it
    pub expect: String,
    /// The revision_id docs_list_paragraphs answered with
    pub revision_id: String,
    /// Must be true to write. Call with false first.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsCodeParam {
    pub account: String,
    pub doc_id: String,
    /// Put the listing after this paragraph, as docs_list_paragraphs numbers
    /// them
    pub after_paragraph: u32,
    /// The code itself, as plain text and with its own line breaks
    pub code: String,
    /// The monospace font to set it in; Courier New by default
    pub font: Option<String>,
    /// The point size for the whole listing, 1 to 400. Left out, the listing
    /// takes the document's own size, which is usually 11
    pub size_pt: Option<f64>,
    /// What to colour, and how. Offsets count characters from the start of
    /// `code`, spans may not overlap, and you work the tokens out yourself:
    /// this server highlights nothing.
    pub spans: Option<Vec<DocsSpanParam>>,
    /// What the paragraph you named starts with. Optional, and worth passing.
    pub expect: Option<String>,
    /// The revision_id docs_list_paragraphs answered with
    pub revision_id: String,
    /// Must be true to write. Call with false first and show the person the
    /// code.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsTableParam {
    pub account: String,
    pub doc_id: String,
    /// Put the table after this paragraph, as docs_list_paragraphs numbers
    /// them
    pub after_paragraph: u32,
    /// The grid, row by row and cell by cell:
    /// [["Model", "Parametry"], ["Mistral-7B", "7 mld"]]. Every row holds the
    /// same number of cells, and a cell is one line of plain text.
    pub rows: Vec<Vec<String>>,
    /// Set the first row bold. That is the only formatting this tool writes.
    pub header: Option<bool>,
    /// What the paragraph you named starts with. Optional, and worth passing.
    pub expect: Option<String>,
    /// The revision_id docs_list_paragraphs answered with
    pub revision_id: String,
    /// Must be true to write. Call with false first and show the person the
    /// grid.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsUploadLinkParam {
    /// What to call the file, e.g. "wykres.png". Docs gives a picture no name
    /// of its own, so this names it only while it is on its way in.
    pub filename: String,
    /// What the file is: image/png, image/jpeg or image/gif. Left out, the
    /// filename decides
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsInsertImageParam {
    pub account: String,
    pub doc_id: String,
    /// Put the picture after this paragraph, as docs_list_paragraphs numbers
    /// them
    pub after_paragraph: u32,
    /// The upload_id you read back from POSTing the file to a docs_upload_link
    /// URL
    pub upload_id: String,
    /// How wide on the page, in points; 72 points is an inch. Left out, Docs
    /// uses the picture's own size.
    pub width_pt: Option<f64>,
    /// How tall on the page, in points. Give one of the two and Docs works
    /// the other out from the picture.
    pub height_pt: Option<f64>,
    /// What the paragraph you named starts with. Optional, and worth passing.
    pub expect: Option<String>,
    /// The revision_id docs_list_paragraphs answered with
    pub revision_id: String,
    /// Must be true to write. Call with false first and show the person which
    /// picture goes where.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DocsSpanParam {
    /// The first character of the span, counting from 0
    pub start: u32,
    /// One past the last character of the span
    pub end: u32,
    /// The colour, as #rrggbb
    // Spelled the British way, and the American spelling is taken too: a
    // model that writes `color` means this field and should not be refused
    // over a vowel.
    #[serde(alias = "color")]
    pub colour: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

impl From<&DocsSpanParam> for docs::Span {
    fn from(s: &DocsSpanParam) -> Self {
        Self {
            start: s.start as usize,
            end: s.end as usize,
            colour: s.colour.clone(),
            bold: s.bold,
            italic: s.italic,
        }
    }
}

#[tool_router(router = docs_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "A Google Doc as markdown, with its title, URL and tab list. This is \
                       drive_read_text for a Doc, plus the tabs. Pictures in the document are \
                       left out and each one says what it was and what to call to see it: Drive \
                       writes them into the export as base64, which no model can see and which \
                       crowds the words out of the answer. docs_list_images lists them."
    )]
    async fn docs_read(
        &self,
        Parameters(p): Parameters<DocsReadParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DocOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim();
        let document = docs::get(client, connection.id, doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let markdown = text::google_doc(client, connection.id, doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let extraction = text::Extraction::new(markdown, "google-doc");
        let (body, truncated) = cap_text(extraction.text, extraction.truncated_chars, p.max_chars);
        Ok(Json(dto::DocOut {
            account: connection.label,
            doc_id: document.document_id.clone(),
            url: document.url(),
            title: document.title,
            tabs: document.tabs.into_iter().map(Into::into).collect(),
            chars: body.chars().count(),
            truncated_chars: truncated,
            text: body,
        }))
    }

    #[tool(
        description = "What pictures a Google Doc holds, in the order they appear in it: the \
                       label to call each one by, the alt text where the document carries any, \
                       and how large it is on the page. docs_read leaves the pictures out of the \
                       text and names them image1, image2 and so on; this says what they are, and \
                       docs_view_image shows one."
    )]
    async fn docs_list_images(
        &self,
        Parameters(p): Parameters<DocsImagesParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DocImagesOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let held = docs::images(&self.google()?.client, connection.id, p.doc_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let note = if held.images.is_empty() {
            "this document holds no pictures".to_string()
        } else {
            "pass one of these as `image` to docs_view_image to look at it, or to \
             docs_image_link for a URL the person can download"
                .to_string()
        };
        Ok(Json(dto::DocImagesOut {
            account: connection.label,
            url: format!(
                "https://docs.google.com/document/d/{}/edit",
                held.document_id
            ),
            doc_id: held.document_id,
            title: held.title,
            count: held.images.len(),
            images: held.images.into_iter().map(Into::into).collect(),
            note,
        }))
    }

    #[tool(
        description = "One picture from a Google Doc, downscaled and returned as an image you \
                       can look at. Name it by its label — image1 is the first picture in the \
                       document — or by its object id. It is visible only in the turn it is \
                       fetched; call again to look later."
    )]
    async fn docs_view_image(
        &self,
        Parameters(p): Parameters<DocsImageParam>,
        Extension(call): Extension<Call>,
    ) -> Result<CallToolResult, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let held = docs::images(client, connection.id, p.doc_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let picture = held
            .find(&p.image)
            .map_err(|e| self.google_err_for(&connection, e))?;
        let download = docs::open_image(client, picture)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let mime_type = download.mime_type().unwrap_or("image/*").to_string();
        let bytes = download
            .collect()
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        images::content(
            call.principal.client_profile(),
            Source {
                connection_id: connection.id,
                kind: Kind::DocsImage,
                ids: &[&held.document_id, &picture.object_id],
                filename: &picture.filename(&held.title, Some(&mime_type)),
                mime_type: &mime_type,
            },
            &bytes,
        )
    }

    #[tool(
        description = "A download URL for one picture in a Google Doc, at its original size. \
                       Name it by its label or its object id, as docs_list_images reports them. \
                       The link lives 15 minutes and may be fetched a few times; give it to the \
                       person or curl it. To look at the picture yourself, use docs_view_image."
    )]
    async fn docs_image_link(
        &self,
        Parameters(p): Parameters<DocsImageParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::LinkOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let held = docs::images(client, connection.id, p.doc_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let picture = held
            .find(&p.image)
            .map_err(|e| self.google_err_for(&connection, e))?;
        // What the picture is and how big it is comes from Google's own
        // headers, because Docs says neither. The body is never read here: the
        // bytes leave through the link, and this answer is only what to expect
        // and whether it is inside the download cap.
        let head = docs::open_image(client, picture)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let mime_type = head.mime_type().unwrap_or("image/*").to_string();
        let size = head.size().and_then(|s| i64::try_from(s).ok());
        let minted = links::mint(
            &self.state,
            call.principal.user().id,
            NewDownload {
                connection_id: connection.id,
                token_id: self.token_id(&call)?,
                target: Target::DocsImage {
                    doc_id: held.document_id.clone(),
                    object_id: picture.object_id.clone(),
                },
                filename: picture.filename(&held.title, Some(&mime_type)),
                mime_type,
                size,
            },
        )
        .await
        .map_err(api_err)?;
        Ok(Json(link_out(minted, call.tz)))
    }

    #[tool(
        description = "Create a Google Doc from markdown and return its id and URL. Needs \
                       confirmed=true: call once with confirmed=false, show the person the title \
                       and the content, and write only after they say yes."
    )]
    async fn docs_create(
        &self,
        Parameters(p): Parameters<DocsCreateParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let title = p.title.trim();
        if title.is_empty() {
            return Err(bad("a document needs a title"));
        }
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "create a new Google Doc called {title:?} in `{}`",
                    connection.label
                ),
                vec![
                    format!("{} characters of markdown", p.markdown.chars().count()),
                    first_lines(&p.markdown),
                ],
            ))));
        }
        let file = drive::create_doc_from_markdown(
            &self.google()?.client,
            connection.id,
            title,
            &p.markdown,
            p.folder_id
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty()),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocWriteOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{}/edit", file.id),
            doc_id: file.id,
            title: Some(file.name),
            written: format!(
                "created from {} characters of markdown",
                p.markdown.chars().count()
            ),
            replacements: None,
        })))
    }

    #[tool(
        description = "Add plain text to the end of a Google Doc. Markdown is not rendered here \
                       — the characters arrive as typed — so write prose, or make a new document \
                       with docs_create instead. To put a paragraph anywhere but the end, use \
                       docs_insert_text. Needs confirmed=true after the person has seen the text."
    )]
    async fn docs_append(
        &self,
        Parameters(p): Parameters<DocsAppendParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let doc_id = p.doc_id.trim().to_string();
        if p.text.is_empty() {
            return Err(bad("there is nothing to append: the text is empty"));
        }
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "append to the Google Doc {doc_id} in `{}`",
                    connection.label
                ),
                vec![
                    format!("{} characters, as plain text", p.text.chars().count()),
                    first_lines(&p.text),
                ],
            ))));
        }
        let index = docs::append_text(&self.google()?.client, connection.id, &doc_id, &p.text)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocWriteOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            title: None,
            written: format!(
                "{} characters appended at index {index}",
                p.text.chars().count()
            ),
            replacements: None,
        })))
    }

    #[tool(
        description = "Replace every occurrence of one string with another throughout a Google \
                       Doc and report how many were changed. This is the broad tool: it changes \
                       every match in the document at once and cannot be undone from here, so it \
                       needs confirmed=true after the person has seen both strings. For careful \
                       work — one occurrence, in one paragraph you have read — use \
                       docs_edit_paragraph instead."
    )]
    async fn docs_replace_text(
        &self,
        Parameters(p): Parameters<DocsReplaceParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let doc_id = p.doc_id.trim().to_string();
        if p.find.is_empty() {
            return Err(bad(
                "the text to find is empty; that would match everywhere",
            ));
        }
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "replace text throughout the Google Doc {doc_id} in `{}`",
                    connection.label
                ),
                vec![
                    format!("find: {:?}", p.find),
                    format!("replace with: {:?}", p.replace),
                    format!(
                        "case {}",
                        if p.match_case.unwrap_or(false) {
                            "must match"
                        } else {
                            "is ignored"
                        }
                    ),
                    "every occurrence changes at once, and this cannot be undone from here".into(),
                ],
            ))));
        }
        let count = docs::replace_all_text(
            &self.google()?.client,
            connection.id,
            &doc_id,
            &p.find,
            &p.replace,
            p.match_case.unwrap_or(false),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocWriteOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            title: None,
            written: format!("{count} occurrences replaced"),
            replacements: Some(count),
        })))
    }

    #[tool(
        description = "A Google Doc as numbered paragraphs: for each one its number, its named \
                       style (HEADING_2, NORMAL_TEXT, …), how many characters it holds and its \
                       text, and the document's revision_id once at the top. This is the read \
                       every careful edit starts from — docs_insert_text, docs_edit_paragraph, \
                       docs_style_paragraph and docs_insert_code all take a paragraph number and \
                       that revision_id. The text of each paragraph is cut unless full=true, and \
                       from and to narrow the range, so one paragraph can be read exactly without \
                       pulling a whole article. Numbering follows the body, table cells included. \
                       One write moves every number and changes the revision id, so read again \
                       after each write."
    )]
    async fn docs_list_paragraphs(
        &self,
        Parameters(p): Parameters<DocsParagraphsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DocParagraphsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let outline = docs::outline(&self.google()?.client, connection.id, p.doc_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let count = outline.paragraphs.len();
        let from = (p.from.unwrap_or(1) as usize).max(1);
        let to = p.to.map(|t| t as usize).unwrap_or(count).min(count);
        let full = p.full.unwrap_or(false);
        let mut budget = LISTING_MAX_CHARS;
        let mut stopped = None;
        let mut paragraphs = Vec::new();
        for paragraph in outline.paragraphs.iter().skip(from.saturating_sub(1)) {
            if paragraph.ordinal > to {
                break;
            }
            if budget == 0 {
                stopped = Some(paragraph.ordinal);
                break;
            }
            let (text, truncated) = cut(
                &paragraph.text,
                if full {
                    budget
                } else {
                    PARAGRAPH_CHARS.min(budget)
                },
            );
            budget -= text.chars().count().min(budget);
            paragraphs.push(dto::DocParagraphOut {
                paragraph: paragraph.ordinal,
                style: paragraph.style.clone(),
                chars: paragraph.chars(),
                in_table: paragraph.in_table,
                text,
                truncated,
            });
        }
        let note = match stopped {
            Some(at) => format!(
                "stopped at paragraph {at} to keep this answer small; ask again with from={at}. \
                 Pass revision_id to every write and read again afterwards"
            ),
            None if paragraphs.is_empty() => format!(
                "this document has {count} paragraphs, so there is nothing to show from {from}"
            ),
            None => "pass revision_id to every write. One write moves every number here and \
                     changes the revision id, so call this again before the next write"
                .to_string(),
        };
        Ok(Json(dto::DocParagraphsOut {
            account: connection.label,
            url: outline.url(),
            doc_id: outline.document_id,
            title: outline.title,
            revision_id: outline.revision_id,
            count,
            from,
            to: paragraphs.last().map(|p| p.paragraph).unwrap_or(to),
            paragraphs,
            note,
        }))
    }

    #[tool(
        description = "The colours, fonts and weights a Google Doc actually holds, paragraph by \
                       paragraph. docs_read goes through Drive's markdown export, which throws \
                       every colour, font and size away, so this is the only way to read back a \
                       listing docs_insert_code coloured. For each paragraph it answers the named \
                       style and the text runs; a run is {start, end, text} plus only the \
                       properties the document sets on it — colour as #rrggbb, bold, italic, font \
                       and size in points — so a paragraph nobody styled comes back as one bare \
                       run rather than a wall of defaults. start and end count characters from \
                       the beginning of the paragraph, the same units docs_insert_code takes for \
                       its spans, so what comes out here can be fed back in. Two things to expect \
                       before you conclude the document is wrong: Docs merges neighbouring \
                       characters that share a style into one run, so a listing written as twenty \
                       spans reads back as fewer runs than that, and the comparison to make is \
                       what colour sits at a given offset rather than how many runs there are. \
                       Ask for one paragraph with `paragraph`, or a range with from and to; the \
                       answer is capped and says which paragraph to ask from next."
    )]
    async fn docs_read_formatting(
        &self,
        Parameters(p): Parameters<DocsFormattingParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DocFormattingOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        if p.paragraph.is_some() && (p.from.is_some() || p.to.is_some()) {
            return Err(bad(
                "say which paragraphs once: `paragraph` for one, or from and to for a range",
            ));
        }
        let outline = docs::outline(&self.google()?.client, connection.id, p.doc_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let count = outline.paragraphs.len();
        let from = (p.paragraph.or(p.from).unwrap_or(1) as usize).max(1);
        let to = p
            .paragraph
            .or(p.to)
            .map(|t| t as usize)
            .unwrap_or(count)
            .min(count);
        let mut budget = LISTING_MAX_CHARS;
        let mut stopped = None;
        let mut paragraphs = Vec::new();
        for paragraph in outline.paragraphs.iter().skip(from.saturating_sub(1)) {
            if paragraph.ordinal > to {
                break;
            }
            if budget == 0 {
                stopped = Some(paragraph.ordinal);
                break;
            }
            // A paragraph is answered whole or not at all: a run cut in half
            // would report an offset that covers fewer characters than it
            // says, and the offsets are the point of this tool.
            let runs: Vec<dto::DocRunOut> =
                paragraph.formatting().into_iter().map(Into::into).collect();
            budget -= paragraph.chars().min(budget);
            paragraphs.push(dto::DocRunsOut {
                paragraph: paragraph.ordinal,
                style: paragraph.style.clone(),
                chars: paragraph.chars(),
                in_table: paragraph.in_table,
                runs,
            });
        }
        let note = match stopped {
            Some(at) => format!(
                "stopped at paragraph {at} to keep this answer small; ask again with from={at}"
            ),
            None if paragraphs.is_empty() => format!(
                "this document has {count} paragraphs, so there is nothing to read from {from}"
            ),
            None => "Docs merges neighbouring characters that share a style into one run, so a \
                     listing comes back as fewer runs than the spans that wrote it. Compare the \
                     colour at an offset, not the number of runs."
                .to_string(),
        };
        Ok(Json(dto::DocFormattingOut {
            account: connection.label,
            url: outline.url(),
            doc_id: outline.document_id,
            title: outline.title,
            revision_id: outline.revision_id,
            count,
            from,
            to: paragraphs.last().map(|p| p.paragraph).unwrap_or(to),
            paragraphs,
            note,
        }))
    }

    #[tool(
        description = "Insert plain text as a new paragraph, after or before the paragraph you \
                       name — the edit docs_append cannot make, because that one only adds at the \
                       end. Markdown is not rendered: \"## Heading\" arrives as those characters, \
                       which is what `style` is for. Pass the revision_id docs_list_paragraphs \
                       answered with; when the document changed since, nothing is written and you \
                       must read it again. `expect` is optional here — inserting beside the wrong \
                       paragraph can be undone in a way that overwriting cannot — and worth \
                       passing anyway. Needs confirmed=true after the person has seen the \
                       preview. One write moves every paragraph number and changes the revision \
                       id."
    )]
    async fn docs_insert_text(
        &self,
        Parameters(p): Parameters<DocsInsertParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let at = match (p.after_paragraph, p.before_paragraph) {
            (Some(a), None) => docs::At::After(a as usize),
            (None, Some(b)) => docs::At::Before(b as usize),
            (Some(_), Some(_)) => {
                return Err(bad(
                    "say where the text goes once: after_paragraph or before_paragraph, not both",
                ));
            }
            (None, None) => {
                return Err(bad(
                    "say where the text goes: after_paragraph or before_paragraph, \
                     as docs_list_paragraphs numbers them",
                ));
            }
        };
        let style = p
            .style
            .as_deref()
            .map(docs::named_style)
            .transpose()
            .map_err(|e| self.google_err_for(&connection, e))?;
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let plan = docs::plan_insert(
            &outline,
            &p.revision_id,
            at,
            &p.text,
            style,
            p.expect.as_deref(),
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        let beside = match at {
            docs::At::After(n) => format!("after paragraph {n}"),
            docs::At::Before(n) => format!("before paragraph {n}"),
        };
        let styled = match style {
            Some(style) => format!("as {style}"),
            None => "in the style of the paragraph beside it".to_string(),
        };
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "insert a new paragraph {beside} of the Google Doc {doc_id} in `{}`",
                    connection.label
                ),
                vec![
                    format!(
                        "paragraph {} reads now: {}",
                        plan.paragraph,
                        first_lines(&plan.before)
                    ),
                    format!("the new paragraph would read: {}", first_lines(&plan.after)),
                    format!("it would be set {styled}"),
                ],
            ))));
        }
        let (paragraph, text, index) = (plan.paragraph, plan.after.clone(), plan.index);
        docs::apply(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!(
                "{} characters inserted {beside} {styled}, at index {index}",
                text.chars().count()
            ),
            text,
            next: REREAD.to_string(),
        })))
    }

    #[tool(
        description = "Change one occurrence of one string inside one paragraph. This is \
                       docs_replace_text for careful work: docs_replace_text changes every match \
                       in the whole document at once, and this changes the one you mean. \
                       occurrence=1 is the first match in that paragraph, occurrence=2 the second. \
                       `expect` is required and is the words the paragraph starts with, as \
                       docs_list_paragraphs reports them: when the paragraph says something else \
                       nothing is written and the answer quotes what is there. Pass the \
                       revision_id from the same read, and confirmed=true after the person has \
                       seen the paragraph as it is and as it would read. One write moves every \
                       paragraph number and changes the revision id."
    )]
    async fn docs_edit_paragraph(
        &self,
        Parameters(p): Parameters<DocsEditParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let plan = docs::plan_edit(
            &outline,
            &p.revision_id,
            p.paragraph as usize,
            &p.find,
            p.occurrence as usize,
            &p.replace,
            &p.expect,
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        let change = format!(
            "occurrence {} of {:?} becomes {:?}",
            p.occurrence, p.find, p.replace
        );
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "change paragraph {} of the Google Doc {doc_id} in `{}`",
                    plan.paragraph, connection.label
                ),
                vec![
                    change.clone(),
                    format!("it reads now: {}", first_lines(&plan.before)),
                    format!("it would read: {}", first_lines(&plan.after)),
                ],
            ))));
        }
        let (paragraph, text) = (plan.paragraph, plan.after.clone());
        docs::apply(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!("paragraph {paragraph} changed: {change}"),
            text,
            next: REREAD.to_string(),
        })))
    }

    #[tool(
        description = "Set the named style of one paragraph: NORMAL_TEXT, TITLE, SUBTITLE or \
                       HEADING_1 to HEADING_6. That is all this does — not alignment, not \
                       spacing, not indentation, not font or size. `expect` is required and is \
                       the words the paragraph starts with, so a number that has moved restyles \
                       nothing. Pass the revision_id from the same docs_list_paragraphs, and \
                       confirmed=true after the person has seen which paragraph it is. One write \
                       moves every paragraph number and changes the revision id."
    )]
    async fn docs_style_paragraph(
        &self,
        Parameters(p): Parameters<DocsStyleParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let plan = docs::plan_style(
            &outline,
            &p.revision_id,
            p.paragraph as usize,
            &p.style,
            &p.expect,
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        let text = outline
            .paragraph(plan.paragraph)
            .map(|p| p.text.clone())
            .unwrap_or_default();
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "restyle paragraph {} of the Google Doc {doc_id} in `{}`",
                    plan.paragraph, connection.label
                ),
                vec![
                    format!("it is {} and would become {}", plan.before, plan.after),
                    format!("it reads: {}", first_lines(&text)),
                    "only the named style changes; the words stay as they are".to_string(),
                ],
            ))));
        }
        let (paragraph, was, now) = (plan.paragraph, plan.before.clone(), plan.after.clone());
        docs::apply(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!("paragraph {paragraph} is {now} now; it was {was}"),
            text,
            next: REREAD.to_string(),
        })))
    }

    #[tool(
        description = "Insert a code listing after the paragraph you name and colour it in one \
                       write: the code as plain text, a monospace font at the size you name over \
                       all of it, and one \
                       colour, bold or italic per span. A span is {start, end, colour, bold, \
                       italic}, where start and end count characters from the beginning of \
                       `code`. You work out where the tokens are: this server highlights nothing \
                       and takes no language. Spans may not overlap, may not be empty and may not \
                       run past the end of the code, and a colour is #rrggbb — a listing that \
                       cannot be coloured completely is refused rather than half written. size_pt sets the whole listing, 1 to 400 \
                       points; left out, it takes the document's own size. Pass \
                       the revision_id from docs_list_paragraphs and confirmed=true. One write \
                       moves every paragraph number and changes the revision id."
    )]
    async fn docs_insert_code(
        &self,
        Parameters(p): Parameters<DocsCodeParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let spans: Vec<docs::Span> = p.spans.iter().flatten().map(Into::into).collect();
        let font = p
            .font
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .unwrap_or(docs::CODE_FONT)
            .to_string();
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let plan = docs::plan_code(
            &outline,
            &p.revision_id,
            p.after_paragraph as usize,
            &p.code,
            docs::CodeStyle {
                font: Some(&font),
                size_pt: p.size_pt,
            },
            &spans,
            p.expect.as_deref(),
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "insert a code listing after paragraph {} of the Google Doc {doc_id} in `{}`",
                    plan.paragraph, connection.label
                ),
                vec![
                    format!(
                        "paragraph {} reads now: {}",
                        plan.paragraph,
                        first_lines(&plan.before)
                    ),
                    format!(
                        "{} characters of code, in {font}, with {} spans coloured",
                        plan.after.chars().count(),
                        spans.len()
                    ),
                    first_lines(&plan.after),
                ],
            ))));
        }
        let (paragraph, text, requests) = (plan.paragraph, plan.after.clone(), plan.requests());
        docs::apply(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!(
                "{} characters of code inserted after paragraph {paragraph} in one batch of \
                 {requests} requests: the text, the {font} font and {} spans",
                text.chars().count(),
                spans.len()
            ),
            text,
            next: REREAD.to_string(),
        })))
    }

    #[tool(
        description = "Insert a table of plain text after the paragraph you name. `rows` is the \
                       grid, row by row: [[\"Model\", \"Parametry\"], [\"Mistral-7B\", \"7 \
                       mld\"]]. Every row must hold the same number of cells, a cell is one line \
                       of plain text, and a table written here is at most 100 rows by 20 columns \
                       — a ragged grid or a bigger one is refused before any call is made. \
                       header=true sets the first row bold and is the only formatting on offer: \
                       no markdown in a cell, no colours, no column widths, no borders and no \
                       merged cells. This is the one tool here that writes twice, because the \
                       indexes inside a table do not exist until the table does: it inserts the \
                       empty grid, reads the document again to see where Google put each cell, \
                       and fills them in a second write. If that second write is refused the \
                       answer says so plainly — the table is there and empty, and it names the \
                       paragraph numbers its cells now have. Pass the revision_id from \
                       docs_list_paragraphs and confirmed=true after the person has seen the \
                       grid. One write moves every paragraph number and changes the revision id."
    )]
    async fn docs_insert_table(
        &self,
        Parameters(p): Parameters<DocsTableParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let header = p.header.unwrap_or(false);
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let plan = docs::plan_table(
            &outline,
            &p.revision_id,
            p.after_paragraph as usize,
            &p.rows,
            header,
            p.expect.as_deref(),
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        let shape = format!("{} by {}", plan.rows, plan.columns);
        let grid = plan.lines().join("\n");
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "insert a {shape} table after paragraph {} of the Google Doc {doc_id} in `{}`",
                    plan.paragraph, connection.label
                ),
                vec![
                    format!(
                        "paragraph {} reads now: {}",
                        plan.paragraph,
                        first_lines(&plan.before)
                    ),
                    first_lines(&grid),
                    match header {
                        true => "the first row is set bold; nothing else is formatted".to_string(),
                        false => "nothing is formatted; pass header=true for a bold first row"
                            .to_string(),
                    },
                    "this writes twice: the empty table, and then its cells at the indexes \
                     Google answers with"
                        .to_string(),
                ],
            ))));
        }
        let paragraph = plan.paragraph;
        let written = docs::apply_table(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!(
                "a {shape} table was inserted after paragraph {paragraph}, in two writes: the \
                 empty grid and then its cells. The cells are paragraphs {} to {} of the \
                 document now{}",
                written.first_paragraph,
                written.last_paragraph,
                match header {
                    true => ", and the first row is bold",
                    false => "",
                }
            ),
            text: grid,
            next: REREAD.to_string(),
        })))
    }

    #[tool(
        description = "A URL to upload one picture to, so a Google Doc can hold it. Putting a \
                       picture in a document takes three steps and you do the middle one \
                       yourself: call this, then POST the bytes to the `url` it answers \
                       (`curl --data-binary @picture.png URL`), then pass the `upload_id` you \
                       read back to docs_insert_image. This server cannot read a file on your \
                       machine, so uploading it is the only way. Docs holds PNG, JPEG and GIF and \
                       nothing else. There is no `account` here because a staged file belongs to \
                       you and not to a document: docs_insert_image decides which document it \
                       lands in. The URL takes one upload and lives 15 minutes; the file itself \
                       waits an hour to be used and is forgotten once it is. This is the Docs \
                       twin of gmail_upload_link — same staging, its own name — and either one's \
                       upload_id works only with the tools of its own service."
    )]
    async fn docs_upload_link(
        &self,
        Parameters(p): Parameters<DocsUploadLinkParam>,
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
            note: "POST the picture to this URL as the whole request body — \
                   `curl --data-binary @/path/to/picture.png URL` — and pass the upload_id it \
                   answers to docs_insert_image. The URL works once and for 15 minutes."
                .into(),
        }))
    }

    #[tool(
        description = "Put a picture into a Google Doc, as its own new paragraph after the \
                       paragraph you name. Upload it first with docs_upload_link and pass the \
                       upload_id here. PNG, JPEG and GIF only, at most 25 megapixels — Docs takes \
                       nothing else and the picture is measured here rather than believed, so a \
                       file Docs would refuse is refused before any call is made. width_pt and \
                       height_pt say how large it is on the page, 72 points to the inch; give one \
                       and Docs works the other out, give neither and the picture keeps its own \
                       size. There is no alt text: the Docs API has no request that sets it, so \
                       the person adds it in Docs. The Docs API cannot take image bytes either — \
                       it takes a URL and Google fetches it — so this server mints an \
                       unguessable download link for the length of the one call and the picture \
                       is served from there. Pass the revision_id from docs_list_paragraphs and \
                       confirmed=true after the person has seen the preview. The upload is spent \
                       by a confirmed call, so upload the picture again to insert it twice. One \
                       write moves every paragraph number and changes the revision id."
    )]
    async fn docs_insert_image(
        &self,
        Parameters(p): Parameters<DocsInsertImageParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::DocEditOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Docs).await?;
        let client = &self.google()?.client;
        let doc_id = p.doc_id.trim().to_string();
        let upload_id = p.upload_id.trim().to_string();
        let user_id = call.principal.user().id;
        // The picture is read without being spent, so that a refusal below
        // leaves the upload_id good for the next attempt. What it is comes
        // from its own header and not from what the upload was called.
        let staged = self
            .state
            .staging
            .peek(user_id, &upload_id)
            .map_err(|e| bad(e.to_string()))?;
        let (width, height) = image::insertable(&staged.mime_type, &staged.bytes)
            .map_err(|e| refuse_picture(&staged.filename, e))?;
        let described = format!(
            "{} ({}, {}, {width}×{height} pixels)",
            staged.filename,
            staged.mime_type,
            uploads::megabytes(staged.bytes.len())
        );
        let sized = match (p.width_pt, p.height_pt) {
            (None, None) => "at the size the picture itself is".to_string(),
            (Some(w), None) => format!("{w} points wide"),
            (None, Some(h)) => format!("{h} points tall"),
            (Some(w), Some(h)) => format!("{w} by {h} points"),
        };
        let outline = docs::outline(client, connection.id, &doc_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        // The whole plan is built once before the upload is spent on it, so a
        // stale revision or an `expect` that does not match refuses with the
        // file still staged. The URI is empty here because this plan is never
        // sent: the confirmed write builds it again around the real link.
        let checked = docs::plan_image(
            &outline,
            &p.revision_id,
            p.after_paragraph as usize,
            &docs::NewImage {
                uri: "",
                width_pt: p.width_pt,
                height_pt: p.height_pt,
                label: &staged.filename,
            },
            p.expect.as_deref(),
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "insert a picture after paragraph {} of the Google Doc {doc_id} in `{}`",
                    checked.paragraph, connection.label
                ),
                vec![
                    format!(
                        "paragraph {} reads now: {}",
                        checked.paragraph,
                        first_lines(&checked.before)
                    ),
                    format!("the picture is {described}"),
                    format!("it goes in {sized}, with no alt text, as its own new paragraph"),
                    "Google fetches the picture from a short-lived link this server mints for \
                     that one call; nothing is written now"
                        .to_string(),
                ],
            ))));
        }
        // From here the upload is spent: it leaves the staging store and is
        // held for the link, which is the only thing that can read it.
        let held = self
            .state
            .staging
            .hold(user_id, &upload_id)
            .map_err(|e| bad(e.to_string()))?;
        let minted = links::mint(
            &self.state,
            user_id,
            NewDownload {
                connection_id: connection.id,
                token_id: self.token_id(&call)?,
                target: Target::Upload {
                    upload_id: held.id.clone(),
                },
                filename: held.filename.clone(),
                mime_type: held.mime_type.clone(),
                size: i64::try_from(held.size).ok(),
            },
        )
        .await
        .map_err(api_err)?;
        let plan = docs::plan_image(
            &outline,
            &p.revision_id,
            p.after_paragraph as usize,
            &docs::NewImage {
                uri: &minted.url,
                width_pt: p.width_pt,
                height_pt: p.height_pt,
                label: &held.filename,
            },
            p.expect.as_deref(),
        )
        .map_err(|e| self.google_err_for(&connection, e))?;
        let (paragraph, index) = (plan.paragraph, plan.index);
        docs::apply(client, connection.id, &doc_id, plan)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(dto::DocEditOut {
            account: connection.label,
            url: format!("https://docs.google.com/document/d/{doc_id}/edit"),
            doc_id,
            paragraph,
            written: format!(
                "the picture {described} was inserted after paragraph {paragraph} at index \
                 {index}, {sized}"
            ),
            text: held.filename,
            next: REREAD.to_string(),
        })))
    }
}

/// Why a staged file cannot go into a document, in the words that say what to
/// do about it. The three caps are Google's own; this server checks them so
/// that a call is never spent on a picture Docs was going to refuse, and so
/// that the answer names the one thing that was wrong.
fn refuse_picture(filename: &str, e: image::InsertError) -> ErrorData {
    let cap = match &e {
        image::InsertError::Format(_) => format!(
            "a document holds {} and nothing else; upload it again, converted, and say which \
             with docs_upload_link's content_type",
            image::INSERTABLE.join(", ")
        ),
        image::InsertError::TooLarge(_) => format!(
            "Docs fetches at most {}",
            uploads::megabytes(INSERT_IMAGE_MAX_BYTES)
        ),
        image::InsertError::TooManyPixels(..) => format!(
            "Docs takes at most {} megapixels; scale the picture down and upload it again",
            INSERT_IMAGE_MAX_PIXELS / 1_000_000
        ),
        image::InsertError::Unreadable(_) => {
            "the bytes are not the picture the upload said they were".to_string()
        }
    };
    bad(format!(
        "{filename}: {e}. {cap}. Nothing was written and no call was made to Google"
    ))
}

/// How much of one paragraph a listing shows when `full` is not set. Enough
/// to recognise a paragraph and to count the ones before it; a model that is
/// about to edit one asks for that one in full.
const PARAGRAPH_CHARS: usize = 400;
/// How much text one listing answers with at most, `full` or not. Past this
/// it stops and says which paragraph to ask from, because a 20,000-character
/// article in one answer is what these tools exist to avoid.
const LISTING_MAX_CHARS: usize = 20_000;

/// What every write says when it is done. One write moves every paragraph
/// number after it and gives the document a new revision, so the read that
/// planned it cannot plan the next one.
const REREAD: &str = "The paragraph numbers and the revision id are stale now. Call \
                      docs_list_paragraphs again before the next write.";

/// One paragraph of a listing, cut to `max` characters.
fn cut(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_string(), false);
    }
    (text.chars().take(max).collect(), true)
}

/// The beginning of what would be written, for the person to recognise. A
/// preview that reprinted a whole document would be a second copy of it in the
/// conversation.
pub(super) fn first_lines(text: &str) -> String {
    const KEEP: usize = 400;
    let head: String = text.chars().take(KEEP).collect();
    if text.chars().count() > KEEP {
        format!("{head}…")
    } else {
        head
    }
}
