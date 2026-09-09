//! The Gmail tools: reading, drafting and labelling. Nothing sends, nothing
//! trashes.
//!
//! The draft tools are the point of the whole server. A model writes a draft,
//! the person opens it in Gmail and decides; so the drafts take no `confirmed`
//! argument — the draft *is* the confirmation — and the results carry the
//! Gmail URL rather than a promise that something went out.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text, capped, refuse};
use crate::domain::scope::Service;
use crate::google::{gmail, text};
use crate::http::links::{self, NewDownload, Target};

/// Gmail's own label ids for the two places nothing here ever puts a message.
const REFUSED: [&str; 2] = ["TRASH", "SPAM"];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// Gmail search syntax: `from:marta subject:invoice has:attachment is:unread`
    pub query: String,
    /// How many messages to return; default 20, at most 100
    pub max: Option<u32>,
    /// A Gmail age shorthand added to the query, e.g. "7d", "2m", "1y"
    pub newer_than: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ThreadParam {
    pub account: String,
    /// The thread id, as gmail_search reports it
    pub thread_id: String,
    /// Keep only the last N messages of a long conversation
    pub max_messages: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MessageParam {
    pub account: String,
    pub message_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentParam {
    pub account: String,
    pub message_id: String,
    /// The attachment id from the message's `attachments`
    pub attachment_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AttachmentTextParam {
    pub account: String,
    pub message_id: String,
    pub attachment_id: String,
    /// Stop after this many characters, with a notice saying what was cut
    pub max_chars: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ViewImageParam {
    pub account: String,
    pub message_id: String,
    /// The attachment id of a picture, or the `content_id` of an inline image
    /// from the message's `inline_images`
    pub attachment_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateDraftParam {
    pub account: String,
    /// Recipients, plain addresses or "Name <address>"
    pub to: Vec<String>,
    pub subject: String,
    /// The plain-text body, which is what most people read
    pub body: String,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    /// An HTML alternative of the same message; the plain text stays required
    pub html: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplyDraftParam {
    pub account: String,
    /// The message being replied to; its threading headers and subject are copied
    pub message_id: String,
    pub body: String,
    /// Copy everyone the original went to, not only its sender
    pub reply_all: Option<bool>,
    pub html: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateDraftParam {
    pub account: String,
    pub draft_id: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub html: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListDraftsParam {
    pub account: String,
    /// How many drafts to return; default 20, at most 100
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteDraftParam {
    pub account: String,
    pub draft_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ModifyLabelsParam {
    pub account: String,
    /// The messages to change, by id
    pub message_ids: Vec<String>,
    /// Labels to add, by name ("Invoices") or by id ("STARRED")
    pub add: Option<Vec<String>>,
    /// Labels to remove, by name or id
    pub remove: Option<Vec<String>>,
    /// Take the messages out of the inbox
    pub archive: Option<bool>,
    /// True marks them read, false marks them unread
    pub mark_read: Option<bool>,
    /// True stars them, false unstars them
    pub star: Option<bool>,
}

#[tool_router(router = gmail_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "Search one account's mail with Gmail's own query syntax and get a compact \
                       list back: message and thread ids, date, sender, recipients, subject, \
                       snippet, labels and how many attachments each has. Use gmail_get_message or \
                       gmail_get_thread for the bodies."
    )]
    async fn gmail_search(
        &self,
        Parameters(p): Parameters<SearchParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::MessagesOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let mut query = p.query.trim().to_string();
        if let Some(age) = p
            .newer_than
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            if age.split_whitespace().count() != 1 {
                return Err(bad(format!(
                    "{age:?} is not a Gmail age; write it as 7d, 2m or 1y"
                )));
            }
            query = format!("{query} newer_than:{age}").trim().to_string();
        }
        let found = gmail::search(
            &self.google()?.client,
            connection.id,
            &query,
            capped(p.max, 20, 100),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::MessagesOut {
            account: connection.label,
            count: found.len(),
            messages: found.into_iter().map(Into::into).collect(),
        }))
    }

    #[tool(
        description = "A whole conversation in order, with each message's text body (converted \
                       from HTML when there is no plain-text part), its attachments by id, name, \
                       type and size, and its inline images by Content-ID."
    )]
    async fn gmail_get_thread(
        &self,
        Parameters(p): Parameters<ThreadParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::ThreadOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let thread = gmail::get_thread(
            &self.google()?.client,
            connection.id,
            p.thread_id.trim(),
            p.max_messages.map(|m| m.max(1) as usize),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::ThreadOut {
            account: connection.label,
            thread_id: thread.id,
            messages: thread.messages.into_iter().map(Into::into).collect(),
        }))
    }

    #[tool(
        description = "One message in full: headers including Message-ID, In-Reply-To and \
                       References, the text body, the attachments by id and the inline images by \
                       Content-ID."
    )]
    async fn gmail_get_message(
        &self,
        Parameters(p): Parameters<MessageParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::MessageOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let message =
            gmail::get_message(&self.google()?.client, connection.id, p.message_id.trim())
                .await
                .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(message.into()))
    }

    #[tool(description = "The account's labels, with their ids and message counts.")]
    async fn gmail_list_labels(
        &self,
        Parameters(p): Parameters<super::AccountParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::LabelsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let labels = gmail::list_labels(&self.google()?.client, connection.id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::LabelsOut {
            account: connection.label,
            labels: labels.into_iter().map(Into::into).collect(),
        }))
    }

    #[tool(
        description = "A download URL for one attachment. The link lives 15 minutes and may be \
                       fetched a few times; give it to the person or curl it. For something you \
                       want to read yourself, use gmail_attachment_text instead."
    )]
    async fn gmail_attachment_link(
        &self,
        Parameters(p): Parameters<AttachmentParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::LinkOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let message =
            gmail::get_message(&self.google()?.client, connection.id, p.message_id.trim())
                .await
                .map_err(|e| self.google_err_for(&connection, e))?;
        let attachment = find_attachment(&message, p.attachment_id.trim())?;
        let minted = links::mint(
            &self.state,
            call.principal.user().id,
            NewDownload {
                connection_id: connection.id,
                token_id: self.token_id(&call)?,
                target: Target::GmailAttachment {
                    message_id: message.id.clone(),
                    attachment_id: attachment.id.clone(),
                },
                filename: attachment.filename.clone(),
                mime_type: attachment.mime_type.clone(),
                size: i64::try_from(attachment.size).ok(),
            },
        )
        .await
        .map_err(api_err)?;
        Ok(Json(link_out(minted)))
    }

    #[tool(
        description = "The text of one attachment, extracted on the server: PDF through poppler, \
                       DOCX from its document part, CSV and plain text as they are. Long text is \
                       cut with a notice saying how much was left out."
    )]
    async fn gmail_attachment_text(
        &self,
        Parameters(p): Parameters<AttachmentTextParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::TextOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let client = &self.google()?.client;
        let message = gmail::get_message(client, connection.id, p.message_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let attachment = find_attachment(&message, p.attachment_id.trim())?.clone();
        let extraction = text::gmail_attachment(
            client,
            connection.id,
            &self.extractor,
            &message.id,
            &attachment,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        let (body, truncated) = cap_text(extraction.text, extraction.truncated_chars, p.max_chars);
        Ok(Json(dto::TextOut {
            account: connection.label,
            source: extraction.source.to_string(),
            filename: Some(attachment.filename),
            chars: body.chars().count(),
            truncated_chars: truncated,
            text: body,
        }))
    }

    #[tool(
        description = "One picture from a message, downscaled and returned as an image you can \
                       look at. Takes an attachment id or the Content-ID of an inline image. The \
                       picture is visible only in the turn it is fetched; call again to look \
                       later. Formats this server cannot decode (HEIC, SVG) are link-only."
    )]
    async fn gmail_view_image(
        &self,
        Parameters(p): Parameters<ViewImageParam>,
        Extension(call): Extension<Call>,
    ) -> Result<CallToolResult, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let client = &self.google()?.client;
        let message = gmail::get_message(client, connection.id, p.message_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let picture = find_picture(&message, p.attachment_id.trim())?;
        let bytes = gmail::get_attachment(client, connection.id, &message.id, &picture.id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        images::content(
            call.principal.client_profile(),
            Source {
                connection_id: connection.id,
                kind: Kind::GmailAttachment,
                ids: &[&message.id, &picture.id],
                filename: &picture.filename,
                mime_type: &picture.mime_type,
            },
            &bytes,
        )
    }

    #[tool(
        description = "Write a new draft in the account's Gmail and return its id and URL. \
                       Nothing is sent: the person opens the draft and presses send themselves, \
                       so say that rather than claiming the mail went out. No confirmation \
                       argument, because the draft is the confirmation."
    )]
    async fn gmail_create_draft(
        &self,
        Parameters(p): Parameters<CreateDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let content = gmail::DraftContent {
            from: connection.google_email.clone(),
            to: addresses(p.to, "to")?,
            cc: p
                .cc
                .map(|c| addresses(c, "cc"))
                .transpose()?
                .unwrap_or_default(),
            bcc: p
                .bcc
                .map(|c| addresses(c, "bcc"))
                .transpose()?
                .unwrap_or_default(),
            subject: p.subject,
            text: p.body,
            html: p.html,
            ..gmail::DraftContent::default()
        };
        let draft = gmail::create_draft(&self.google()?.client, connection.id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft)))
    }

    #[tool(
        description = "Write a reply to a message as a draft, threaded properly: In-Reply-To, \
                       References and the thread id come from the message being replied to, and \
                       the subject keeps one Re:. Nothing is sent; the person sends it from Gmail."
    )]
    async fn gmail_reply_draft(
        &self,
        Parameters(p): Parameters<ReplyDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let client = &self.google()?.client;
        let message = gmail::get_message(client, connection.id, p.message_id.trim())
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let mut content = gmail::DraftContent::reply_to(
            &message,
            &connection.google_email,
            &p.body,
            p.reply_all.unwrap_or(false),
        );
        content.html = p.html;
        let draft = gmail::create_draft(client, connection.id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft)))
    }

    #[tool(
        description = "Replace an existing draft's whole message: recipients, subject and body \
                       are written as given, and anything left out is dropped. Read the draft \
                       with gmail_list_drafts first if you mean to keep part of it. Still nothing \
                       is sent."
    )]
    async fn gmail_update_draft(
        &self,
        Parameters(p): Parameters<UpdateDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let content = gmail::DraftContent {
            from: connection.google_email.clone(),
            to: addresses(p.to, "to")?,
            cc: p
                .cc
                .map(|c| addresses(c, "cc"))
                .transpose()?
                .unwrap_or_default(),
            bcc: p
                .bcc
                .map(|c| addresses(c, "bcc"))
                .transpose()?
                .unwrap_or_default(),
            subject: p.subject,
            text: p.body,
            html: p.html,
            ..gmail::DraftContent::default()
        };
        let draft = gmail::update_draft(
            &self.google()?.client,
            connection.id,
            p.draft_id.trim(),
            &content,
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft)))
    }

    #[tool(description = "The account's drafts, newest first, with their ids and Gmail URLs.")]
    async fn gmail_list_drafts(
        &self,
        Parameters(p): Parameters<ListDraftsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let drafts = gmail::list_drafts(
            &self.google()?.client,
            connection.id,
            capped(p.max, 20, 100),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::DraftsOut {
            account: connection.label,
            count: drafts.len(),
            drafts: drafts.into_iter().map(Into::into).collect(),
        }))
    }

    #[tool(
        description = "Throw a draft away. This is the undo for a draft you just wrote and the \
                       only thing this server deletes in Gmail; nothing else here removes mail."
    )]
    async fn gmail_delete_draft(
        &self,
        Parameters(p): Parameters<DeleteDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let draft_id = p.draft_id.trim().to_string();
        gmail::delete_draft(&self.google()?.client, connection.id, &draft_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::DraftOut {
            account: connection.label,
            url: String::new(),
            message_id: String::new(),
            thread_id: String::new(),
            draft_id,
            note: "the draft is gone".into(),
        }))
    }

    #[tool(
        description = "Add and remove labels on messages, archive them, mark them read or \
                       unread, star or unstar them. Labels may be named or given by id. TRASH and \
                       SPAM are refused: this server never bins mail and never marks it spam. \
                       Messages are changed one by one: any that fail come back in `failed` while \
                       the others still change, so a retry names only the ones that failed."
    )]
    async fn gmail_modify_labels(
        &self,
        Parameters(p): Parameters<ModifyLabelsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::ModifiedOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let client = &self.google()?.client;
        let ids: Vec<String> = p
            .message_ids
            .iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(bad("message_ids is empty; name at least one message"));
        }
        let add = p.add.unwrap_or_default();
        let remove = p.remove.unwrap_or_default();
        for label in add.iter().chain(remove.iter()) {
            if REFUSED.contains(&label.trim().to_ascii_uppercase().as_str()) {
                return Err(refuse(format!(
                    "this server never moves mail to {}: nothing is trashed and nothing is \
                     marked spam. Archive it instead",
                    label.trim().to_ascii_uppercase()
                )));
            }
        }
        let flags = [
            (p.archive.unwrap_or(false), false, "INBOX"),
            (p.mark_read == Some(true), false, "UNREAD"),
            (p.mark_read == Some(false), true, "UNREAD"),
            (p.star == Some(true), true, "STARRED"),
            (p.star == Some(false), false, "STARRED"),
        ];
        if add.is_empty() && remove.is_empty() && !flags.iter().any(|(set, _, _)| *set) {
            return Err(bad(
                "nothing to change: pass add, remove, archive, mark_read or star",
            ));
        }
        // Only what the caller named needs the label list; the ids the three
        // flags stand for are Gmail's own and are never in a person's labels
        // under a different name.
        let labels = gmail::list_labels(client, connection.id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let mut add = label_ids(&labels, &add)?;
        let mut remove = label_ids(&labels, &remove)?;
        for (set, on, id) in flags {
            let side = if on { &mut add } else { &mut remove };
            if set && !side.iter().any(|l| l == id) {
                side.push(id.to_string());
            }
        }
        // One request per message, so one message Gmail will not change — an
        // id that is gone, a message another client moved — must not throw
        // away the ones that were changed before it. Only a call where nothing
        // at all worked is an error.
        let mut messages = Vec::with_capacity(ids.len());
        let mut failed: Vec<dto::FailedMessageOut> = Vec::new();
        let mut first_failure: Option<ErrorData> = None;
        for id in &ids {
            match gmail::modify_labels(client, connection.id, id, &add, &remove).await {
                Ok(message) => messages.push(dto::MessageBriefOut {
                    message_id: message.id,
                    thread_id: message.thread_id,
                    date: dto::instant(message.date),
                    from: message.from,
                    to: message.to,
                    subject: message.subject,
                    snippet: message.snippet,
                    labels: message.labels,
                    attachments: message.attachments.len(),
                }),
                Err(e) => {
                    let error = self.google_err_for(&connection, e);
                    failed.push(dto::FailedMessageOut {
                        message_id: id.clone(),
                        error: error.message.to_string(),
                    });
                    first_failure.get_or_insert(error);
                }
            }
        }
        if messages.is_empty() {
            return Err(first_failure.expect("a message that neither changed nor failed"));
        }
        Ok(Json(dto::ModifiedOut {
            account: connection.label,
            modified: messages.len(),
            added: add,
            removed: remove,
            messages,
            failed,
        }))
    }
}

/// Label names as the person says them, turned into the ids Gmail's API takes.
/// A name that matches nothing is refused with the list, because inventing a
/// label id silently creates nothing and changes nothing.
fn label_ids(labels: &[gmail::Label], wanted: &[String]) -> Result<Vec<String>, ErrorData> {
    let mut out: Vec<String> = Vec::with_capacity(wanted.len());
    for name in wanted {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let found = labels
            .iter()
            .find(|l| l.id == name)
            .or_else(|| labels.iter().find(|l| l.name.eq_ignore_ascii_case(name)));
        let Some(label) = found else {
            let known: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
            return Err(bad(format!(
                "there is no label called {name:?}; this account has {}",
                known.join(", ")
            )));
        };
        if !out.contains(&label.id) {
            out.push(label.id.clone());
        }
    }
    Ok(out)
}

fn find_attachment<'a>(
    message: &'a gmail::Message,
    attachment_id: &str,
) -> Result<&'a gmail::Attachment, ErrorData> {
    message
        .attachments
        .iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| {
            let known: Vec<String> = message
                .attachments
                .iter()
                .map(|a| format!("{} ({})", a.filename, a.id))
                .collect();
            bad(format!(
                "message {} has no attachment {attachment_id:?}; it has {}",
                message.id,
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ))
        })
}

/// A picture named either by attachment id or by `Content-ID`, which is how an
/// inline image in an HTML body is referred to.
struct Picture {
    id: String,
    filename: String,
    mime_type: String,
}

/// The named part, once it is established that it is a picture at all. An
/// attachment id is an attachment id: nothing stops a model from handing over
/// the PDF's, and downloading one to feed it to an image decoder would waste
/// the fetch and answer with a decoding error instead of the two tools that do
/// read a PDF. This mirrors `drive_view_image`.
fn find_picture(message: &gmail::Message, wanted: &str) -> Result<Picture, ErrorData> {
    let picture = locate_picture(message, wanted)?;
    if !picture.mime_type.starts_with("image/") {
        return Err(refuse(format!(
            "{} is a {}, not a picture; use gmail_attachment_text or gmail_attachment_link",
            picture.filename, picture.mime_type
        )));
    }
    Ok(picture)
}

fn locate_picture(message: &gmail::Message, wanted: &str) -> Result<Picture, ErrorData> {
    let bare = wanted.trim_start_matches('<').trim_end_matches('>');
    if let Some(a) = message.attachments.iter().find(|a| a.id == wanted) {
        return Ok(Picture {
            id: a.id.clone(),
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
        });
    }
    let inline = message.inline_images.iter().find(|i| {
        i.attachment_id.as_deref() == Some(wanted)
            || i.content_id.trim_start_matches('<').trim_end_matches('>') == bare
    });
    if let Some(i) = inline {
        let id = i.attachment_id.clone().ok_or_else(|| {
            bad(format!(
                "the inline image {} is embedded in the message body rather than stored as an \
                 attachment, so it cannot be fetched separately",
                i.content_id
            ))
        })?;
        return Ok(Picture {
            id,
            filename: i.filename.clone(),
            mime_type: i.mime_type.clone(),
        });
    }
    let mut known: Vec<String> = message
        .attachments
        .iter()
        .filter(|a| a.mime_type.starts_with("image/"))
        .map(|a| format!("{} ({})", a.filename, a.id))
        .collect();
    known.extend(
        message
            .inline_images
            .iter()
            .map(|i| format!("{} ({})", i.filename, i.content_id)),
    );
    Err(bad(format!(
        "message {} has no picture {wanted:?}; it has {}",
        message.id,
        if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        }
    )))
}

/// Recipients as written, minus the empty ones. An address list that ends up
/// empty is refused here rather than by Gmail, whose message is unhelpful.
fn addresses(values: Vec<String>, field: &str) -> Result<Vec<String>, ErrorData> {
    let out: Vec<String> = values
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    if out.is_empty() && field == "to" {
        return Err(bad("a draft needs at least one recipient in `to`"));
    }
    Ok(out)
}

fn draft_out(account: String, draft: gmail::DraftRef) -> dto::DraftOut {
    dto::DraftOut {
        account,
        url: draft.url(),
        draft_id: draft.id,
        message_id: draft.message_id,
        thread_id: draft.thread_id,
        note: "nothing was sent; the person opens this draft in Gmail and sends it themselves"
            .into(),
    }
}

pub(super) fn link_out(minted: links::Minted) -> dto::LinkOut {
    dto::LinkOut {
        url: minted.url,
        filename: minted.filename,
        mime_type: minted.mime_type,
        size: minted.size,
        expires_at: minted
            .expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        note: "the link expires in 15 minutes and may be fetched a few times; mint a new one \
               afterwards"
            .into(),
    }
}
