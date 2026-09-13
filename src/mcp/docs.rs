//! The Docs tools. Reading goes through Drive's markdown export, because
//! Docs' own API has no text output; writing is the two edits this release
//! makes, appending at the end and replacing text throughout.
//!
//! Every one of the three writes takes `confirmed`. With `confirmed=false`
//! nothing is written and the answer says what would be — that is the step
//! where the person sees the change and agrees to it.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto::{self, Confirmable, PreviewOut};
use super::{Call, Gmcp, bad, cap_text};
use crate::domain::scope::Service;
use crate::google::{docs, drive, text};

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

#[tool_router(router = docs_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "A Google Doc as markdown, with its title, URL and tab list. This is \
                       drive_read_text for a Doc, plus the tabs. Pictures in the document are \
                       left out and each one says what it was: Drive writes them into the export \
                       as base64, which no model can see and which crowds the words out of the \
                       answer."
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
