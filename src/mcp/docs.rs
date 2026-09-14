//! The Docs tools. Reading goes through Drive's markdown export, because
//! Docs' own API has no text output, with docs_list_paragraphs beside it for
//! the shape of the document; writing is the two edits this release makes,
//! appending at the end and replacing text throughout.
//!
//! Every one of the three writes takes `confirmed`. With `confirmed=false`
//! nothing is written and the answer says what would be — that is the step
//! where the person sees the change and agrees to it.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::gmail::link_out;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text};
use crate::domain::scope::Service;
use crate::google::{docs, drive, text};
use crate::http::links::{self, NewDownload, Target};

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
                       with docs_create instead. Needs confirmed=true after the person has seen \
                       the text."
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
                       Doc and report how many were changed. This cannot be undone from here, so \
                       it needs confirmed=true after the person has seen both strings."
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
}

/// How much of one paragraph a listing shows when `full` is not set. Enough
/// to recognise a paragraph and to count the ones before it; a model that is
/// about to edit one asks for that one in full.
const PARAGRAPH_CHARS: usize = 400;
/// How much text one listing answers with at most, `full` or not. Past this
/// it stops and says which paragraph to ask from, because a 20,000-character
/// article in one answer is what these tools exist to avoid.
const LISTING_MAX_CHARS: usize = 20_000;

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
