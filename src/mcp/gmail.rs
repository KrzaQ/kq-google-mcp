//! The Gmail tools: reading, drafting and labelling. Nothing sends, nothing
//! trashes.
//!
//! The draft tools are the point of the whole server. A model writes a draft,
//! the person opens it in Gmail and decides; so the drafts take no `confirmed`
//! argument — the draft *is* the confirmation — and the results carry the
//! Gmail URL rather than a promise that something went out.

use chrono_tz::Tz;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::dto;
use super::images::{self, Kind, Source};
use super::{Call, Gmcp, api_err, bad, cap_text, capped, refuse};
use crate::db::Connection;
use crate::domain::limits::ATTACHMENT_MAX_BYTES;
use crate::domain::scope::Service;
use crate::google::{gmail, text};
use crate::http::links::{self, NewDownload, Target};
use crate::http::uploads;

/// Gmail's own label ids for the two places nothing here ever puts a message.
const REFUSED: [&str; 2] = ["TRASH", "SPAM"];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ThreadParam {
    pub account: String,
    /// The thread id, as gmail_search reports it
    pub thread_id: String,
    /// Keep only the last N messages of a long conversation
    pub max_messages: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageParam {
    pub account: String,
    pub message_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentParam {
    pub account: String,
    pub message_id: String,
    /// Which file: the `part` of one of the message's `attachments` or
    /// `inline_images`, or its filename. Gmail's `attachment_id` changes
    /// every time the message is read; `part` does not
    #[serde(alias = "attachment_id")]
    pub part: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentTextParam {
    pub account: String,
    pub message_id: String,
    /// Which file: the `part` of one of the message's `attachments` or
    /// `inline_images`, or its filename. Gmail's `attachment_id` changes
    /// every time the message is read; `part` does not
    #[serde(alias = "attachment_id")]
    pub part: String,
    /// Stop after this many characters, with a notice saying what was cut
    pub max_chars: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewImageParam {
    pub account: String,
    pub message_id: String,
    /// Which picture: the `part` of one of the message's `attachments` or
    /// `inline_images`, the `content_id` of an inline one, or its filename.
    /// Gmail's `attachment_id` changes every time the message is read;
    /// `part` does not
    #[serde(alias = "attachment_id")]
    pub part: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Which address to write as: a verified send-as address of this account,
    /// as "sales@example.test" or "Sales <sales@example.test>".
    /// gmail_list_send_as reports the ones that work. Defaults to the
    /// account's default address
    pub from: Option<String>,
    /// Files to attach, by the upload_id each POST to a gmail_upload_link URL
    /// answered. Each id is used once and the file is then forgotten
    pub attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyDraftParam {
    pub account: String,
    /// The message being replied to; its threading headers and subject are copied
    pub message_id: String,
    pub body: String,
    /// Copy everyone the original went to, not only its sender
    pub reply_all: Option<bool>,
    pub html: Option<String>,
    /// Which address to write as: a verified send-as address of this account,
    /// as "sales@example.test" or "Sales <sales@example.test>".
    /// gmail_list_send_as reports the ones that work. Left out, the reply
    /// comes from the address the original was delivered to
    pub from: Option<String>,
    /// Files to attach, by the upload_id each POST to a gmail_upload_link URL
    /// answered. Each id is used once and the file is then forgotten
    pub attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDraftParam {
    pub account: String,
    pub draft_id: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub html: Option<String>,
    /// Which address to write as: a verified send-as address of this account,
    /// as "sales@example.test" or "Sales <sales@example.test>".
    /// gmail_list_send_as reports the ones that work. Defaults to the
    /// account's default address
    pub from: Option<String>,
    /// Files to attach, by the upload_id each POST to a gmail_upload_link URL
    /// answered. The rewrite replaces the whole message, so a file the draft
    /// already carries is kept only by uploading it again
    pub attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachToDraftParam {
    pub account: String,
    pub draft_id: String,
    /// Files to add, by the upload_id each POST to a gmail_upload_link URL
    /// answered. Everything the draft already carries is kept
    pub attachments: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadLinkParam {
    /// The name the recipient sees, e.g. "Faktura 04-2026.pdf"
    pub filename: String,
    /// What the file is, e.g. "application/pdf". Left out, the filename decides
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDraftsParam {
    pub account: String,
    /// How many drafts to return; default 20, at most 100
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteDraftParam {
    pub account: String,
    pub draft_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
                       snippet and labels. A row does not say whether the message carries files, \
                       because Gmail answers a listing without the part tree; narrow the search \
                       with `has:attachment`, and read one message with gmail_get_message to see \
                       what it carries. Use gmail_get_message or gmail_get_thread for the bodies."
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
            messages: found
                .into_iter()
                .map(|m| dto::MessageBriefOut::new(m, call.tz))
                .collect(),
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
            messages: thread
                .messages
                .into_iter()
                .map(|m| dto::MessageOut::new(m, call.tz))
                .collect(),
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
        Ok(Json(dto::MessageOut::new(message, call.tz)))
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
        description = "The addresses this account may write mail as: its own Google address and \
                       every alias on it, with the display name each one writes under, which is \
                       the default and which may be used. Only an address marked usable_as_from \
                       can be passed as `from` to the draft tools — Gmail rewrites a From it has \
                       not verified, so one that is not verified is refused rather than sent \
                       under the wrong name."
    )]
    async fn gmail_list_send_as(
        &self,
        Parameters(p): Parameters<super::AccountParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::SendAsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let addresses = self.send_as(&connection).await?;
        let default = gmail::default_send_as(&addresses).map(|a| a.email.clone());
        let note = match default {
            Some(address) => format!(
                "a draft with no `from` is written as {address}; \
                 pass one of the usable_as_from addresses to write as another"
            ),
            None => "this account reports no send-as address at all".to_string(),
        };
        Ok(Json(dto::SendAsOut {
            account: connection.label,
            count: addresses.len(),
            addresses: addresses.into_iter().map(Into::into).collect(),
            note,
        }))
    }

    #[tool(
        description = "A download URL for one attachment. Name the file by the `part` \
                       gmail_get_message reports, or by its filename: Gmail mints a new \
                       attachment id every time a message is read, so an id you are holding \
                       names nothing, while a part id is the same on every read. The old name \
                       of this argument, `attachment_id`, still works and takes the same three \
                       things. The link lives 15 minutes and may be fetched a few times; give \
                       it to the person or curl it. For something you want to read yourself, \
                       use gmail_attachment_text instead."
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
        // The id the link carries comes from the read this call just made,
        // never from the caller: the one the caller passed is a read old.
        let attachment = resolve_part(&message, p.part.trim())?;
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
        Ok(Json(link_out(minted, call.tz)))
    }

    #[tool(
        description = "The text of one attachment, extracted on the server: PDF through poppler, \
                       DOCX from its document part, CSV and plain text as they are. Name the \
                       file by the `part` gmail_get_message reports, or by its filename: Gmail \
                       mints a new attachment id every time a message is read, so an id you are \
                       holding names nothing, while a part id is the same on every read. The old \
                       name of this argument, `attachment_id`, still works and takes the same \
                       three things. Long text is cut with a notice saying how much was left out."
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
        let attachment = resolve_part(&message, p.part.trim())?;
        let extraction = text::gmail_attachment(
            client,
            connection.id,
            &self.extractor,
            &message.id,
            &attachment.attachment(),
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
                       look at. Name it by the `part` gmail_get_message reports, by the \
                       Content-ID of an inline image, or by its filename: Gmail mints a new \
                       attachment id every time a message is read, so an id you are holding \
                       names nothing, while a part id is the same on every read. The old name \
                       of this argument, `attachment_id`, still works and takes the same three \
                       things. The picture is visible only in the turn it is fetched; call again \
                       to look later. Formats this server cannot decode (HEIC, SVG) are \
                       link-only."
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
        let picture = find_picture(&message, p.part.trim())?;
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
        description = "A URL to upload one file to, so a draft can carry it. Attaching a file \
                       takes three steps and you do the middle one yourself: call this, then \
                       POST the bytes to the `url` it answers (`curl --data-binary @file URL`), \
                       then pass the `upload_id` you read back in the `attachments` of \
                       gmail_create_draft, gmail_reply_draft, gmail_update_draft or \
                       gmail_attach_to_draft. This server cannot read a file on your machine, so \
                       uploading it is the only way to attach it. There is no `account` here \
                       because a staged file belongs to you and not to a mailbox: the draft tool \
                       you pass it to decides which account it lands in. The URL takes one upload \
                       and lives 15 minutes; the file itself waits an hour to be attached and is \
                       forgotten once it is."
    )]
    async fn gmail_upload_link(
        &self,
        Parameters(p): Parameters<UploadLinkParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::UploadLinkOut>, ErrorData> {
        let filename = p.filename.trim();
        if filename.is_empty() {
            return Err(bad(
                "filename is empty; name the file the recipient will see",
            ));
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
            note: "POST the file to this URL as the whole request body — \
                   `curl --data-binary @/path/to/file URL` — and pass the upload_id it answers \
                   to gmail_create_draft, gmail_reply_draft or gmail_update_draft as one of \
                   `attachments`. The URL works once and for 15 minutes."
                .into(),
        }))
    }

    #[tool(
        description = "Write a new draft in the account's Gmail and return its id and URL. \
                       Nothing is sent: the person opens the draft and presses send themselves, \
                       so say that rather than claiming the mail went out. No confirmation \
                       argument, because the draft is the confirmation. `from` must be one of \
                       the account's verified send-as addresses, which gmail_list_send_as \
                       reports; left out, the draft comes from the account's default address. \
                       Files are attached by uploading them first with gmail_upload_link and \
                       passing the upload ids as `attachments`; the result lists what the draft \
                       actually carries."
    )]
    async fn gmail_create_draft(
        &self,
        Parameters(p): Parameters<CreateDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let from = self.draft_from(&connection, p.from.as_deref()).await?;
        let content = gmail::DraftContent {
            from: from.header.clone(),
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
            // Last, so a call refused for anything else leaves the files
            // where they are and can simply be made again.
            attachments: self.attachments(&call, p.attachments)?,
            ..gmail::DraftContent::default()
        };
        let draft = gmail::create_draft(&self.google()?.client, connection.id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft, from, &content)))
    }

    #[tool(
        description = "Write a reply to a message as a draft, threaded properly: In-Reply-To, \
                       References and the thread id come from the message being replied to, and \
                       the subject keeps one Re:. The reply comes from the address the original \
                       was delivered to, which is what Gmail itself does; pass `from` to write as \
                       another of the account's verified send-as addresses, which \
                       gmail_list_send_as reports. The result says which address was chosen and \
                       why — tell the person, so a wrong guess is caught before they send. \
                       Replying to a message the account itself sent writes to that message's \
                       own recipients, not back to the account, because the person is carrying \
                       on a thread they started; the result says so in `to_reason`. \
                       Files are attached by uploading them first with gmail_upload_link and \
                       passing the upload ids as `attachments`. \
                       Nothing is sent; the person sends it from Gmail."
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
        let from = self
            .reply_from(&connection, &message, p.from.as_deref())
            .await?;
        let mut content = gmail::DraftContent::reply_to(
            &message,
            &from.header,
            &p.body,
            p.reply_all.unwrap_or(false),
        );
        content.html = p.html;
        content.attachments = self.attachments(&call, p.attachments)?;
        let draft = gmail::create_draft(client, connection.id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let mut out = draft_out(connection.label, draft, from, &content);
        if message.is_sent() {
            out.to_reason = Some(
                "this account sent the original, so the reply goes to its recipients rather \
                 than back to its sender"
                    .into(),
            );
        }
        Ok(Json(out))
    }

    #[tool(
        description = "Replace an existing draft's whole message: recipients, subject and body \
                       are written as given, and anything left out is dropped. Read the draft \
                       with gmail_list_drafts first if you mean to keep part of it. The \
                       conversation is kept: a draft that answers a message goes on answering it, \
                       with its In-Reply-To, References and thread id, so correcting a recipient \
                       does not start a new thread. `from` must be one of the account's verified \
                       send-as addresses, which gmail_list_send_as reports; left out, the draft \
                       comes from the account's default address. The message is replaced whole, \
                       attachments included: a file the draft already carries is kept only by \
                       uploading it again with gmail_upload_link and naming it in \
                       `attachments`. To add a file and change nothing else, use \
                       gmail_attach_to_draft instead. Still nothing is sent."
    )]
    async fn gmail_update_draft(
        &self,
        Parameters(p): Parameters<UpdateDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let from = self.draft_from(&connection, p.from.as_deref()).await?;
        let to = addresses(p.to, "to")?;
        let cc =
            p.cc.map(|c| addresses(c, "cc"))
                .transpose()?
                .unwrap_or_default();
        let bcc = p
            .bcc
            .map(|c| addresses(c, "bcc"))
            .transpose()?
            .unwrap_or_default();
        let client = &self.google()?.client;
        let draft_id = p.draft_id.trim();
        // The caller rewrites the message, not the conversation. Reading the
        // draft first is what says which conversation that is: leaving the
        // threading out files the rewritten draft as a new one, and a reply
        // the person corrected one address of would leave its thread.
        let existing = gmail::get_draft(client, connection.id, draft_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        let content = gmail::DraftContent {
            from: from.header.clone(),
            to,
            cc,
            bcc,
            subject: p.subject,
            text: p.body,
            html: p.html,
            // A draft that was never a reply has neither header, and carrying
            // nothing forward writes nothing. Its thread id is its own, and
            // handing it back keeps the draft where it already is.
            in_reply_to: existing.in_reply_to.filter(|id| !id.trim().is_empty()),
            references: existing.references,
            thread_id: Some(existing.thread_id).filter(|id| !id.trim().is_empty()),
            attachments: self.attachments(&call, p.attachments)?,
        };
        let draft = gmail::update_draft(client, connection.id, draft_id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft, from, &content)))
    }

    #[tool(
        description = "Add files to a draft that already exists and keep everything else it has: \
                       its recipients, its subject, both bodies and the conversation it belongs \
                       to, as well as the files it already carries. Upload each file with \
                       gmail_upload_link first and pass the upload ids as `attachments`. Use this \
                       rather than gmail_update_draft to attach something, because that tool \
                       replaces the whole message and every recipient and both bodies would have \
                       to be restated correctly. A draft with pictures inside its HTML body is \
                       refused: rebuilding it would break them. The result lists every file the \
                       draft now carries, the old ones and the new ones together. Nothing is sent."
    )]
    async fn gmail_attach_to_draft(
        &self,
        Parameters(p): Parameters<AttachToDraftParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::DraftOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Gmail).await?;
        let ids = upload_ids(p.attachments)?;
        let client = &self.google()?.client;
        let draft_id = p.draft_id.trim();
        // Raw, because that answer carries the bytes of the files the draft
        // already has: a draft with three of them costs one call rather than
        // four, and the message is rebuilt from what came back.
        let existing = gmail::get_draft_raw(client, connection.id, draft_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        if !existing.inline_ids.is_empty() {
            return Err(refuse(format!(
                "this draft has pictures inside its HTML body (Content-ID {}), and adding a file \
                 means rebuilding the message, which would leave the body pointing at pictures \
                 that are no longer in it — the draft would still look right in a list and be \
                 broken when it is opened. Nothing was changed and the uploads are still waiting. \
                 Write this one again with gmail_update_draft, which replaces the message whole \
                 and takes files as `attachments`",
                existing.inline_ids.join(", ")
            )));
        }
        // Weighed before anything is taken: a draft that cannot hold the files
        // leaves them staged, so the person attaches them somewhere else
        // rather than uploading them again.
        let adding = self
            .state
            .staging
            .sizes(call.principal.user().id, &ids)
            .map_err(take_err)?;
        let carried: Vec<(String, usize)> = existing
            .content
            .attachments
            .iter()
            .map(|file| (file.filename.clone(), file.bytes.len()))
            .collect();
        let total: usize = carried
            .iter()
            .chain(adding.iter())
            .map(|(_, size)| size)
            .sum();
        if total > ATTACHMENT_MAX_BYTES {
            return Err(bad(too_large(&carried, &adding)));
        }
        let mut content = existing.content;
        let from = match content.from.trim() {
            "" => {
                // Gmail always writes one, so this is the draft that came from
                // somewhere else. The account's own address is what Gmail
                // would compose with.
                let chosen = self.draft_from(&connection, None).await?;
                content.from = chosen.header.clone();
                chosen
            }
            header => ChosenFrom {
                header: header.to_string(),
                reason: "the draft was already written as it".into(),
            },
        };
        content
            .attachments
            .extend(self.attachments(&call, Some(ids))?);
        let draft = gmail::update_draft(client, connection.id, draft_id, &content)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(draft_out(connection.label, draft, from, &content)))
    }

    #[tool(
        description = "The account's drafts, newest first, with their ids and Gmail URLs. A row \
                       does not say whether a draft carries files; gmail_get_message on the \
                       draft's message_id names them."
    )]
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
            drafts: drafts
                .into_iter()
                .map(|d| dto::DraftBriefOut::new(d, call.tz))
                .collect(),
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
            from: None,
            from_reason: None,
            to_reason: None,
            attachments: Vec::new(),
            attachment_warning: None,
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
                    date: dto::instant(message.date, call.tz),
                    from: message.from,
                    to: message.to,
                    subject: message.subject,
                    snippet: message.snippet,
                    labels: message.labels,
                    attachments: Some(message.attachments.len()),
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

/// Which address a draft is written as, and why that one. A reply chooses on
/// its own, so the reason travels back to the model and from there to the
/// person: a wrong guess should be visible rather than silent.
struct ChosenFrom {
    /// The `From` header the draft carries.
    header: String,
    reason: String,
}

impl Gmcp {
    /// The files a draft is about to carry, taken off the shelf as the acting
    /// person. Every id is checked before any file is taken, so a call naming
    /// one id that is not there leaves the rest where they are: a draft is
    /// written once, with all of its files or with none.
    ///
    /// The files are gone once this answers. That is what makes an upload id
    /// a thing that cannot be attached twice by accident.
    fn attachments(
        &self,
        call: &Call,
        ids: Option<Vec<String>>,
    ) -> Result<Vec<gmail::NewAttachment>, ErrorData> {
        let ids: Vec<String> = ids
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let files = self
            .state
            .staging
            .take(call.principal.user().id, &ids)
            .map_err(take_err)?;
        Ok(files
            .into_iter()
            .map(|file| gmail::NewAttachment {
                filename: file.filename,
                mime_type: file.mime_type,
                bytes: file.bytes,
            })
            .collect())
    }

    /// The account's send-as addresses. The Google client keeps them for a few
    /// minutes per connection, so drafting several replies asks Gmail once.
    async fn send_as(&self, connection: &Connection) -> Result<Vec<gmail::SendAs>, ErrorData> {
        gmail::send_as(&self.google()?.client, connection.id)
            .await
            .map_err(|e| self.google_err_for(connection, e))
    }

    /// The `From` a new or replaced draft carries: the address the caller
    /// named, or the account's default one.
    async fn draft_from(
        &self,
        connection: &Connection,
        wanted: Option<&str>,
    ) -> Result<ChosenFrom, ErrorData> {
        match named(wanted) {
            Some(wanted) => choose(&self.send_as(connection).await?, wanted),
            None => Ok(self.default_from(connection).await),
        }
    }

    /// The `From` a reply carries, which is what the Gmail web UI does: the
    /// address the original was delivered to. `Delivered-To` says it outright;
    /// without one, an address of this account in `To` and then in `Cc` says
    /// it well enough. A named `from` wins over all of that.
    async fn reply_from(
        &self,
        connection: &Connection,
        message: &gmail::Message,
        wanted: Option<&str>,
    ) -> Result<ChosenFrom, ErrorData> {
        if let Some(wanted) = named(wanted) {
            return choose(&self.send_as(connection).await?, wanted);
        }
        let Ok(addresses) = gmail::send_as(&self.google()?.client, connection.id).await else {
            return Ok(self.default_from(connection).await);
        };
        let alias_for = |value: &String| {
            gmail::find_send_as(&addresses, value)
                .filter(|alias| alias.usable())
                .map(|alias| ChosenFrom {
                    header: alias.header(),
                    reason: String::new(),
                })
        };
        if let Some(mut chosen) = message.delivered_to.iter().find_map(alias_for) {
            chosen.reason = "the message was delivered to it".into();
            return Ok(chosen);
        }
        for (field, values) in [("To", &message.to), ("Cc", &message.cc)] {
            if let Some(mut chosen) = values.iter().find_map(alias_for) {
                chosen.reason = format!("the original's {field} names it");
                return Ok(chosen);
            }
        }
        let mut chosen = default_of(&addresses, connection);
        chosen.reason = format!(
            "{}; the original named no verified address of this account",
            chosen.reason
        );
        Ok(chosen)
    }

    /// The account's default alias. When Gmail will not say what it is — the
    /// settings call failed, or the account reports nothing — the connection's
    /// own Google address stands in, because that is the one address Gmail
    /// never rewrites. This is the only place an address is chosen without
    /// checking the list, and it says so in the reason it hands back.
    async fn default_from(&self, connection: &Connection) -> ChosenFrom {
        let Ok(google) = self.google() else {
            return own_address(connection);
        };
        match gmail::send_as(&google.client, connection.id).await {
            Ok(addresses) => default_of(&addresses, connection),
            Err(e) => {
                tracing::warn!("connection {}: send-as addresses: {e}", connection.id);
                own_address(connection)
            }
        }
    }
}

/// The account's default address, out of a list already in hand.
fn default_of(addresses: &[gmail::SendAs], connection: &Connection) -> ChosenFrom {
    match gmail::default_send_as(addresses) {
        Some(alias) => ChosenFrom {
            header: alias.header(),
            reason: "it is the account's default send-as address".into(),
        },
        None => own_address(connection),
    }
}

/// The connection's own Google address, which Gmail never rewrites.
fn own_address(connection: &Connection) -> ChosenFrom {
    ChosenFrom {
        header: connection.google_email.clone(),
        reason: "the account's own Google address, because Gmail did not report its send-as \
                 addresses"
            .into(),
    }
}

/// A `from` argument that says something.
fn named(wanted: Option<&str>) -> Option<&str> {
    wanted.map(str::trim).filter(|w| !w.is_empty())
}

/// The address a `from` argument names, checked against the send-as list.
/// Gmail rewrites a `From` it has not verified, so an address that is not on
/// the list, and one on it that Google has not verified, are both refused with
/// the addresses that would have worked. Nothing falls back to the primary.
fn choose(addresses: &[gmail::SendAs], wanted: &str) -> Result<ChosenFrom, ErrorData> {
    let Some(alias) = gmail::find_send_as(addresses, wanted) else {
        return Err(bad(format!(
            "this account cannot write mail as {wanted:?}, and Gmail would replace it with the \
             account's own address. It can write as {}. gmail_list_send_as reports them in full",
            usable(addresses)
        )));
    };
    if !alias.usable() {
        return Err(refuse(format!(
            "{} is a send-as address of this account but Google has not verified it (Gmail says \
             {:?}), so Gmail would replace it on send. The person verifies it in Gmail's \
             settings. The addresses that work today are {}",
            alias.email,
            alias.verification_status.as_deref().unwrap_or("unverified"),
            usable(addresses)
        )));
    }
    // A caller who wrote a display name keeps it; a bare address gains the
    // alias's own, so the draft reads like one written by hand.
    let header = if wanted.contains('<') {
        wanted.to_string()
    } else {
        alias.header()
    };
    Ok(ChosenFrom {
        header,
        reason: "the `from` argument named it".into(),
    })
}

/// The addresses a draft may be written as, for a refusal to name.
fn usable(addresses: &[gmail::SendAs]) -> String {
    let usable: Vec<&str> = addresses
        .iter()
        .filter(|a| a.usable())
        .map(|a| a.email.as_str())
        .collect();
    if usable.is_empty() {
        "no address at all".to_string()
    } else {
        usable.join(", ")
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

/// One fetchable part of a message, as the read that is happening right now
/// reports it.
///
/// A file attached inline — a PDF dropped into a reply, or a forwarded one —
/// carries a `Content-ID`, so Gmail files it among the inline parts rather
/// than the attachments. It is still a file with an attachment id, and
/// `messages.attachments.get` fetches it the same way, so both arrays are one
/// list here.
#[derive(Clone)]
struct Part {
    /// Where the part sits in the message. The same on every read.
    part_id: String,
    /// What the attachments endpoint is called with. Minted by the read this
    /// value came from, and good for that read only.
    id: String,
    content_id: Option<String>,
    filename: String,
    mime_type: String,
    size: u64,
}

impl Part {
    /// What to call the part when talking to the caller about it.
    fn name(&self) -> &str {
        if !self.filename.is_empty() {
            return &self.filename;
        }
        match &self.content_id {
            Some(cid) if !cid.is_empty() => cid,
            _ => &self.part_id,
        }
    }

    fn attachment(&self) -> gmail::Attachment {
        gmail::Attachment {
            part_id: self.part_id.clone(),
            id: self.id.clone(),
            filename: self.filename.clone(),
            mime_type: self.mime_type.clone(),
            size: self.size,
        }
    }
}

/// Every part of the message a tool can fetch, attachments first.
fn parts_of(message: &gmail::Message) -> Vec<Part> {
    let attachments = message.attachments.iter().map(|a| Part {
        part_id: a.part_id.clone(),
        id: a.id.clone(),
        content_id: None,
        filename: a.filename.clone(),
        mime_type: a.mime_type.clone(),
        size: a.size,
    });
    let inline = message.inline_images.iter().map(|i| Part {
        part_id: i.part_id.clone(),
        id: i.attachment_id.clone().unwrap_or_default(),
        content_id: Some(i.content_id.clone()),
        filename: i.filename.clone(),
        mime_type: i.mime_type.clone(),
        size: i.size,
    });
    attachments.chain(inline).collect()
}

/// A `Content-ID` without its angle brackets, which is how a `cid:` in an
/// HTML body spells it.
fn bare_cid(value: &str) -> &str {
    value.trim_start_matches('<').trim_end_matches('>')
}

/// The part a caller named, resolved against the message as it reads today.
///
/// Gmail mints a new attachment id every time a message is read, so the id a
/// caller is holding names nothing by the time it comes back: the tools kept
/// answering "message X has no part Y" and naming, as the alternative, a
/// third id that was already dead as well. The part id is the same on every
/// read, so it is the handle the tools hand out, and the fetch that follows
/// uses the attachment id from the read this call just made.
///
/// Four names are tried in turn, and each one has to pick out exactly one
/// part: the part id, a `Content-ID`, a filename, then an attachment id from
/// this read. A message with one part and nothing matched is the last case —
/// a caller holding a stale id means that file, and there is nothing else it
/// could mean.
fn resolve_part(message: &gmail::Message, wanted: &str) -> Result<Part, ErrorData> {
    let parts = parts_of(message);
    let one = |f: &dyn Fn(&Part) -> bool| {
        let mut hits = parts.iter().filter(|p| f(p));
        match (hits.next(), hits.next()) {
            (Some(only), None) => Some(only.clone()),
            _ => None,
        }
    };
    let found = one(&|p| !p.part_id.is_empty() && p.part_id == wanted)
        .or_else(|| {
            one(&|p| match &p.content_id {
                Some(cid) => !cid.is_empty() && bare_cid(cid) == bare_cid(wanted),
                None => false,
            })
        })
        .or_else(|| one(&|p| !p.filename.is_empty() && p.filename.eq_ignore_ascii_case(wanted)))
        .or_else(|| one(&|p| !p.id.is_empty() && p.id == wanted))
        .or_else(|| match parts.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        });
    let part = found.ok_or_else(|| bad(no_such_part(message, wanted)))?;
    if part.id.is_empty() {
        return Err(bad(format!(
            "{} is carried in the message body rather than stored as an attachment, so it \
             cannot be fetched on its own",
            part.name()
        )));
    }
    Ok(part)
}

/// What the message does hold, by the handles that will still work in a
/// minute.
///
/// Naming both counts matters: "it has none" was the old answer to a message
/// carrying two inline files, which sent the caller looking for the wrong
/// problem. Naming the part rather than the attachment id matters for the
/// same reason: an id out of this listing is dead as soon as it is printed.
fn no_such_part(message: &gmail::Message, wanted: &str) -> String {
    let names = |listed: Vec<String>| match listed.is_empty() {
        true => "none".to_string(),
        false => listed.join(", "),
    };
    format!(
        "message {} has no part {wanted:?}. Its {} attachments: {}. Its {} inline parts: {}. \
         Gmail mints a new attachment id every time a message is read, so an id from an earlier \
         read is not one of these; name the part or the filename.",
        message.id,
        message.attachments.len(),
        names(
            message
                .attachments
                .iter()
                .map(|a| format!("{} (part {})", a.filename, a.part_id))
                .collect()
        ),
        message.inline_images.len(),
        names(
            message
                .inline_images
                .iter()
                .map(|i| format!(
                    "{} (part {}, Content-ID {})",
                    i.filename, i.part_id, i.content_id
                ))
                .collect()
        ),
    )
}

/// The named part, once it is established that it is a picture at all. A
/// caller names parts the same way for every tool: nothing stops a model from
/// handing over the PDF, and downloading one to feed it to an image decoder
/// would waste the fetch and answer with a decoding error instead of the two
/// tools that do read a PDF. This mirrors `drive_view_image`.
fn find_picture(message: &gmail::Message, wanted: &str) -> Result<Part, ErrorData> {
    let picture = resolve_part(message, wanted)?;
    if !picture.mime_type.starts_with("image/") {
        return Err(refuse(format!(
            "{} is a {}, not a picture; use gmail_attachment_text or gmail_attachment_link",
            picture.name(),
            picture.mime_type
        )));
    }
    Ok(picture)
}

/// A staged upload that cannot be read is this server's problem; every other
/// way one can fail is the caller's to fix, and the message says how.
fn take_err(e: uploads::TakeError) -> ErrorData {
    match e {
        uploads::TakeError::Io(_) => ErrorData::internal_error(e.to_string(), None),
        _ => bad(e.to_string()),
    }
}

/// The upload ids an argument names. A call that names none is refused rather
/// than rewriting a draft to change nothing about it.
fn upload_ids(values: Vec<String>) -> Result<Vec<String>, ErrorData> {
    let ids: Vec<String> = values
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Err(bad(
            "attachments is empty; name at least one upload_id from gmail_upload_link",
        ));
    }
    Ok(ids)
}

/// Why a draft may not have these files: what it carries, what was being
/// added, and what the two come to. The sizes are named the way an upload
/// that is too large on its own already names them, because "too large" on
/// its own leaves a person guessing which file to leave out.
fn too_large(carried: &[(String, usize)], adding: &[(String, usize)]) -> String {
    let named = |files: &[(String, usize)]| match files {
        [] => "nothing".to_string(),
        files => files
            .iter()
            .map(|(name, size)| format!("{name} {}", uploads::megabytes(*size)))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let total: usize = carried
        .iter()
        .chain(adding.iter())
        .map(|(_, size)| size)
        .sum();
    format!(
        "the draft carries {} and adding {} comes to {}, over the {} Gmail allows one message; \
         attach fewer files, or send the rest as download links. The draft was not changed and \
         the uploads are still waiting",
        named(carried),
        named(adding),
        uploads::megabytes(total),
        uploads::megabytes(ATTACHMENT_MAX_BYTES)
    )
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

fn draft_out(
    account: String,
    draft: gmail::DraftRef,
    from: ChosenFrom,
    content: &gmail::DraftContent,
) -> dto::DraftOut {
    dto::DraftOut {
        account,
        url: draft.url(),
        draft_id: draft.id,
        message_id: draft.message_id,
        thread_id: draft.thread_id,
        from: Some(from.header),
        from_reason: Some(from.reason),
        to_reason: None,
        attachments: content
            .attachments
            .iter()
            .map(|f| f.filename.clone())
            .collect(),
        attachment_warning: attachment_warning(content),
        note: "nothing was sent; the person opens this draft in Gmail and sends it themselves"
            .into(),
    }
}

/// The sentence a draft gets when its body talks about attaching something
/// and it carries nothing. The mail this whole feature is for went out
/// promising three files, with a tool result that read as success; a model
/// that reads this has to tell the person before they press send.
fn attachment_warning(content: &gmail::DraftContent) -> Option<String> {
    if !content.attachments.is_empty() {
        return None;
    }
    let html = content.html.as_deref().unwrap_or_default();
    let promises = dto::promises_attachment(&content.text) || dto::promises_attachment(html);
    promises.then(|| {
        "this draft says something is attached and it carries no file. Tell the person before \
         they send it, or attach the file: mint a URL with gmail_upload_link, POST the file to \
         it, and pass the upload_id to gmail_update_draft as one of `attachments`"
            .to_string()
    })
}

pub(super) fn link_out(minted: links::Minted, tz: Tz) -> dto::LinkOut {
    dto::LinkOut {
        url: minted.url,
        filename: minted.filename,
        mime_type: minted.mime_type,
        size: minted.size,
        expires_at: dto::at_zone(minted.expires_at, tz),
        note: "the link expires in 15 minutes and may be fetched a few times; mint a new one \
               afterwards"
            .into(),
    }
}
