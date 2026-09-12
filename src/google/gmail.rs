//! Gmail: search, read, labels and drafts. The endpoints below are the whole
//! surface, and what is missing is missing on purpose — there is no `send`,
//! no `trash` and no `delete` on a message anywhere in this file, and the
//! module's tests grep it to keep it that way. A draft is the deliverable and
//! the person sends it from Gmail. `settings/sendAs` is the one endpoint here
//! whose name reads like sending: it lists the addresses the account may write
//! mail as, and reading it sends nothing.
//!
//! Messages come back from Google in `format=full`, which is already
//! decomposed into MIME parts with an `attachmentId` per attachment — the id
//! the attachments endpoint needs, and the reason the raw format is not used
//! for reading mail. A draft that is about to be written again is the one
//! thing read as `format=raw`: rebuilding it needs the bytes of the files it
//! already carries, and raw is the only format that brings them in the same
//! answer.
//!
//! `mail-parser` does the part of the job Google leaves alone: decoding header
//! values (RFC 2047 words, address lists, identifier lists), taking a raw
//! draft apart, and turning an HTML-only body into something readable.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeZone, Utc};
use lettre::message::header::{ContentType, InReplyTo, MessageId, References};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use mail_parser::parsers::MessageStream;
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders, PartType};
use serde::{Deserialize, Serialize};

use super::client::{Client, Error, Result, urlencode};
use super::multipart;

/// Everything is done as the connected account.
const USER: &str = "me";
/// Gmail is served from its own host.
const GMAIL: &str = "gmail";
/// Labels a tool may never add: this server does not trash and does not mark
/// spam. The MCP layer refuses them too; refusing here as well means no future
/// caller can get around it by accident.
const REFUSED_LABELS: [&str; 2] = ["TRASH", "SPAM"];
/// Which headers a summary needs, so a list does not pull whole bodies.
const SUMMARY_HEADERS: [&str; 4] = ["From", "To", "Subject", "Date"];
/// The label id Gmail puts on a message the account sent itself.
const SENT_LABEL: &str = "SENT";
/// What a file is called when the part carrying it names it nothing. A MIME
/// part may have no filename at all, and a message still has to be buildable
/// from what came back.
const UNNAMED_FILE: &str = "attachment";
/// Bytes nobody can vouch for.
const OCTET_STREAM: &str = "application/octet-stream";

/// One row of a search result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageSummary {
    pub id: String,
    pub thread_id: String,
    pub date: Option<DateTime<Utc>>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub labels: Vec<String>,
}

/// One message, read. The headers are the three the plan names plus the
/// ordinary ones; `text` is what a chat model reads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub date: Option<DateTime<Utc>>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// The `Delivered-To` header, which says which of the account's own
    /// addresses the mail arrived at. A reply is written from that one.
    pub delivered_to: Vec<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub labels: Vec<String>,
    /// The `Message-ID` header, which is what a reply threads against.
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub text: String,
    /// True when there was no text part and `text` is the HTML converted.
    pub text_from_html: bool,
    pub attachments: Vec<Attachment>,
    pub inline_images: Vec<InlineImage>,
}

impl Message {
    /// True when the account sent this message itself. Gmail says so with the
    /// `SENT` label, and a reply to such a message is addressed differently:
    /// its `From` is the account, so answering it would write to the person
    /// replying.
    pub fn is_sent(&self) -> bool {
        self.labels.iter().any(|l| l == SENT_LABEL)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Attachment {
    /// What `messages.attachments.get` is called with.
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

/// A picture the message refers to from its own HTML. It is listed by
/// `Content-ID` because that is the only name the body uses for it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InlineImage {
    /// The `Content-ID` header with its angle brackets stripped.
    pub content_id: String,
    pub attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

/// One address the account may write mail as, as `users.settings.sendAs`
/// reports it. Gmail rewrites a `From` that is not one of these, so this list
/// is what the draft tools check an argument against.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SendAs {
    pub email: String,
    pub display_name: Option<String>,
    /// The address Gmail composes with when the person does not choose.
    pub is_default: bool,
    /// The Google account's own address, which needs no verification.
    pub is_primary: bool,
    /// Google's own word: `accepted`, `pending`, and empty on the primary.
    pub verification_status: Option<String>,
    pub reply_to: Option<String>,
}

/// The verification state Google accepts mail from.
const VERIFIED: &str = "accepted";

impl SendAs {
    /// True when Gmail will keep a draft's `From` as written. An alias Google
    /// has not verified is rewritten to the primary address on send, so the
    /// draft tools refuse it rather than let that happen quietly.
    pub fn usable(&self) -> bool {
        self.is_primary
            || self
                .verification_status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(VERIFIED))
    }

    /// The header a draft carries: `"Display Name" <address>` when the alias
    /// has a display name, so the draft reads like one written by hand, and
    /// the bare address when it does not.
    pub fn header(&self) -> String {
        quoted(self.display_name.as_deref(), &self.email)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Thread {
    pub id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    /// `system` or `user`.
    pub kind: Option<String>,
    pub messages_total: Option<i64>,
    pub messages_unread: Option<i64>,
}

/// A draft as the drafts list reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftSummary {
    pub id: String,
    pub message: MessageSummary,
}

/// What a create or update answered: enough to name the draft and link to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftRef {
    pub id: String,
    pub message_id: String,
    pub thread_id: String,
}

impl DraftRef {
    /// Where a person opens the draft. Gmail's own web URL, which is the
    /// point of a draft: someone reads it and presses send.
    pub fn url(&self) -> String {
        format!(
            "https://mail.google.com/mail/u/0/#drafts?compose={}",
            self.id
        )
    }
}

/// What a draft is made of, attachments included. A file gets here as bytes
/// and nothing else: the server reads no path a caller supplies, because the
/// caller is on another machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftContent {
    /// Which of the account's send-as addresses the draft is written as.
    /// Gmail rewrites anything else, so the MCP layer resolves it against
    /// [`send_as`] before it gets here.
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    /// Threading, copied from the message being replied to.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Set so Gmail files the draft in the right conversation.
    pub thread_id: Option<String>,
    /// The files the message carries. Empty is the common case, and a draft
    /// with nothing here is built and sent to Gmail exactly as it was before
    /// attachments existed.
    pub attachments: Vec<NewAttachment>,
}

/// A file to attach to a draft: the bytes themselves, the name the recipient
/// sees, and what it is.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAttachment {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl DraftContent {
    /// A reply to `message`, with the threading the plan spells out: the
    /// replied message's `Message-ID` in `In-Reply-To`, its `References` plus
    /// that id in `References`, its thread id on the draft, and its subject
    /// with one `Re:`.
    pub fn reply_to(message: &Message, from: &str, body: &str, reply_all: bool) -> Self {
        let (to, cc) = if message.is_sent() {
            // The account wrote this one, so its `From` is the account itself
            // and a reply built from it would go back to the person writing.
            // What continues the thread is the message's own recipients.
            // Nothing is dropped for being an address of this account: writing
            // to yourself is the point once you have sent the mail.
            let cc = if reply_all {
                message.cc.clone()
            } else {
                Vec::new()
            };
            (message.to.clone(), cc)
        } else {
            let mut to: Vec<String> = message.from.clone().into_iter().collect();
            let mut cc = Vec::new();
            if reply_all {
                // Everyone the message went to, minus the account replying.
                for address in message.to.iter().chain(message.cc.iter()) {
                    if !same_address(address, from) && !to.iter().any(|t| same_address(t, address))
                    {
                        cc.push(address.clone());
                    }
                }
            }
            to.retain(|address| !same_address(address, from) || message.to.len() <= 1);
            (to, cc)
        };
        let mut references = message.references.clone();
        if let Some(id) = &message.message_id
            && !references.contains(id)
        {
            references.push(id.clone());
        }
        Self {
            from: from.to_string(),
            to,
            cc,
            bcc: Vec::new(),
            subject: reply_subject(message.subject.as_deref()),
            text: body.to_string(),
            html: None,
            in_reply_to: message.message_id.clone(),
            references,
            thread_id: Some(message.thread_id.clone()),
            attachments: Vec::new(),
        }
    }
}

/// `Re: ` exactly once, however the original was spelled. The prefix is
/// looked for on character boundaries: a subject that starts with an emoji
/// has a multi-byte character across the first three bytes, and slicing
/// through it would panic on a mail nobody controls but the sender.
fn reply_subject(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or_default().trim();
    if subject
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("re:"))
    {
        subject.to_string()
    } else if subject.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// Two header values name the same mailbox. Display names differ freely, so
/// only the address inside the angle brackets is compared.
fn same_address(a: &str, b: &str) -> bool {
    bare_address(a).eq_ignore_ascii_case(&bare_address(b))
}

/// One mailbox written the way a mail library reads it back: `"Display Name"
/// <address>`, or the bare address when there is no name. The quotes are what
/// makes it safe to read again — a name with a comma or a full stop in it is
/// one mailbox inside them and two, or none, without them.
fn quoted(name: Option<&str>, address: &str) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => {
            let escaped = name.replace('\\', r"\\").replace('"', "\\\"");
            format!("\"{escaped}\" <{address}>")
        }
        None => address.to_string(),
    }
}

/// The address inside a header value, with any display name dropped: both
/// `Sales <sales@example.test>` and `sales@example.test` come out the same.
pub fn bare_address(value: &str) -> String {
    match (value.rfind('<'), value.rfind('>')) {
        (Some(open), Some(close)) if close > open => value[open + 1..close].trim().to_string(),
        _ => value.trim().to_string(),
    }
}

/// `users.messages.list` with Gmail's own query syntax, followed by one
/// `format=metadata` read per hit so the rows carry senders and subjects.
pub async fn search(
    client: &Client,
    connection_id: i64,
    query: &str,
    max: u32,
) -> Result<Vec<MessageSummary>> {
    let request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/messages"))?
        .query(&[("q", query), ("maxResults", &max.to_string())]);
    let list: WireMessageList = client.json(connection_id, request).await?;
    let mut out = Vec::with_capacity(list.messages.len());
    for reference in list.messages {
        out.push(summary(client, connection_id, &reference.id).await?);
    }
    Ok(out)
}

async fn summary(client: &Client, connection_id: i64, id: &str) -> Result<MessageSummary> {
    let mut request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/messages/{}", urlencode(id)))?
        .query(&[("format", "metadata")]);
    for header in SUMMARY_HEADERS {
        request = request.query(&[("metadataHeaders", header)]);
    }
    let wire: WireMessage = client.json(connection_id, request).await?;
    Ok(wire.summary())
}

/// `users.messages.get` with `format=full`: the whole part tree, which is
/// where attachment ids and `Content-ID`s live.
pub async fn get_message(client: &Client, connection_id: i64, id: &str) -> Result<Message> {
    let request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/messages/{}", urlencode(id)))?
        .query(&[("format", "full")]);
    let wire: WireMessage = client.json(connection_id, request).await?;
    Ok(wire.message())
}

/// `users.threads.get`, the whole conversation in order.
pub async fn get_thread(
    client: &Client,
    connection_id: i64,
    thread_id: &str,
    max_messages: Option<usize>,
) -> Result<Thread> {
    let request = client
        .service(GMAIL)
        .get(&format!(
            "gmail/v1/users/{USER}/threads/{}",
            urlencode(thread_id)
        ))?
        .query(&[("format", "full")]);
    let wire: WireThread = client.json(connection_id, request).await?;
    let mut messages: Vec<Message> = wire.messages.into_iter().map(|m| m.message()).collect();
    if let Some(max) = max_messages
        && messages.len() > max
    {
        // The last ones: a long thread is read for what was said recently.
        messages.drain(..messages.len() - max);
    }
    Ok(Thread {
        id: wire.id,
        messages,
    })
}

/// `users.messages.attachments.get`. The bytes come back base64url encoded
/// inside JSON, which is Gmail's shape and not a choice made here.
pub async fn get_attachment(
    client: &Client,
    connection_id: i64,
    message_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>> {
    let request = client.service(GMAIL).get(&format!(
        "gmail/v1/users/{USER}/messages/{}/attachments/{}",
        urlencode(message_id),
        urlencode(attachment_id)
    ))?;
    let wire: WireBody = client.json(connection_id, request).await?;
    decode_body(wire.data.as_deref().unwrap_or_default())
}

/// The alias an address names, matched on the address alone and without
/// regard to case, so a bare address and a `Name <address>` form both find it.
pub fn find_send_as<'a>(list: &'a [SendAs], wanted: &str) -> Option<&'a SendAs> {
    let wanted = bare_address(wanted);
    list.iter().find(|s| s.email.eq_ignore_ascii_case(&wanted))
}

/// What Gmail composes with when nobody chooses: the default alias, the
/// primary address behind it, and the first entry behind that.
pub fn default_send_as(list: &[SendAs]) -> Option<&SendAs> {
    list.iter()
        .find(|s| s.is_default)
        .or_else(|| list.iter().find(|s| s.is_primary))
        .or_else(|| list.first())
}

/// The account's send-as addresses, read at most once every few minutes per
/// connection. Drafting a handful of replies must not ask Google for the same
/// list each time; the cache lives on the client and holds no lock across the
/// fetch.
pub async fn send_as(client: &Client, connection_id: i64) -> Result<Vec<SendAs>> {
    client
        .send_as_cache()
        .get_or_fetch(connection_id, list_send_as(client, connection_id))
        .await
}

/// `users.settings.sendAs.list`, the addresses this account may write mail as.
/// `gmail.modify` already grants it, so no connection has to be made again for
/// this. It is not a way to send anything: the endpoint reads settings.
pub async fn list_send_as(client: &Client, connection_id: i64) -> Result<Vec<SendAs>> {
    let request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/settings/sendAs"))?;
    let wire: WireSendAsList = client.json(connection_id, request).await?;
    Ok(wire
        .send_as
        .into_iter()
        .map(|s| SendAs {
            email: s.send_as_email,
            display_name: s.display_name.filter(|n| !n.trim().is_empty()),
            is_default: s.is_default,
            is_primary: s.is_primary,
            verification_status: s.verification_status.filter(|v| !v.trim().is_empty()),
            reply_to: s.reply_to_address.filter(|r| !r.trim().is_empty()),
        })
        .collect())
}

/// `users.labels.list`.
pub async fn list_labels(client: &Client, connection_id: i64) -> Result<Vec<Label>> {
    let request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/labels"))?;
    let wire: WireLabelList = client.json(connection_id, request).await?;
    Ok(wire
        .labels
        .into_iter()
        .map(|l| Label {
            id: l.id,
            name: l.name,
            kind: l.label_type,
            messages_total: l.messages_total,
            messages_unread: l.messages_unread,
        })
        .collect())
}

/// `users.messages.modify`: label ids on and off, and nothing else. `TRASH`
/// and `SPAM` are refused here as well as in the tool, because this is the
/// last place that can refuse them.
pub async fn modify_labels(
    client: &Client,
    connection_id: i64,
    message_id: &str,
    add: &[String],
    remove: &[String],
) -> Result<Message> {
    for label in add.iter().chain(remove.iter()) {
        if REFUSED_LABELS.contains(&label.trim().to_ascii_uppercase().as_str()) {
            return Err(Error::Unsupported(format!(
                "this server never moves mail to {}: nothing is trashed and nothing is marked spam",
                label.trim().to_ascii_uppercase()
            )));
        }
    }
    let request = client
        .service(GMAIL)
        .post(&format!(
            "gmail/v1/users/{USER}/messages/{}/modify",
            urlencode(message_id)
        ))?
        .json(&ModifyRequest {
            add_label_ids: add.to_vec(),
            remove_label_ids: remove.to_vec(),
        });
    let wire: WireMessage = client.json(connection_id, request).await?;
    Ok(wire.message())
}

/// `users.drafts.list`, newest first as Gmail returns them.
pub async fn list_drafts(
    client: &Client,
    connection_id: i64,
    max: u32,
) -> Result<Vec<DraftSummary>> {
    let request = client
        .service(GMAIL)
        .get(&format!("gmail/v1/users/{USER}/drafts"))?
        .query(&[("maxResults", max.to_string())]);
    let list: WireDraftList = client.json(connection_id, request).await?;
    let mut out = Vec::with_capacity(list.drafts.len());
    for draft in list.drafts {
        let message = match draft.message {
            Some(message) if message.payload.is_some() => message.summary(),
            _ => summary(client, connection_id, &draft_message_id(&draft)?).await?,
        };
        out.push(DraftSummary {
            id: draft.id,
            message,
        });
    }
    Ok(out)
}

fn draft_message_id(draft: &WireDraft) -> Result<String> {
    draft
        .message
        .as_ref()
        .map(|m| m.id.clone())
        .ok_or_else(|| Error::Malformed(format!("draft {} has no message", draft.id)))
}

/// `users.drafts.get`, the draft read back in full. An update reads the draft
/// this way to learn which conversation it belongs to.
pub async fn get_draft(client: &Client, connection_id: i64, draft_id: &str) -> Result<Message> {
    let request = client
        .service(GMAIL)
        .get(&format!(
            "gmail/v1/users/{USER}/drafts/{}",
            urlencode(draft_id)
        ))?
        .query(&[("format", "full")]);
    let wire: WireDraft = client.json(connection_id, request).await?;
    wire.message
        .map(|m| m.message())
        .ok_or_else(|| Error::Malformed(format!("draft {draft_id} came back without a message")))
}

/// A draft read back as the message it is: everything a rewrite has to keep,
/// and the files it already carries with their bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDraft {
    /// The draft as [`create_draft`] and [`update_draft`] take it, so adding
    /// a file to it is pushing one onto `attachments` and writing it back.
    pub content: DraftContent,
    /// The `Content-ID` of every part the HTML body names with `cid:`. A
    /// draft that has any of these cannot be rebuilt from its parts — the
    /// pictures in the body would lose what the body refers to them by — so
    /// the caller refuses rather than writing a broken message.
    pub inline_ids: Vec<String>,
}

/// `users.drafts.get` with `format=raw`, taken apart into the message it is.
///
/// Raw is the format that makes this one call: it carries the bytes of every
/// attachment with the draft, where `format=full` carries ids and would need
/// one more call per file. `mail-parser` does the taking apart, as it does
/// everywhere else in this module.
pub async fn get_draft_raw(
    client: &Client,
    connection_id: i64,
    draft_id: &str,
) -> Result<RawDraft> {
    let request = client
        .service(GMAIL)
        .get(&format!(
            "gmail/v1/users/{USER}/drafts/{}",
            urlencode(draft_id)
        ))?
        .query(&[("format", "raw")]);
    let wire: WireDraft = client.json(connection_id, request).await?;
    let message = wire
        .message
        .ok_or_else(|| Error::Malformed(format!("draft {draft_id} came back without a message")))?;
    let raw = message.raw.as_deref().ok_or_else(|| {
        Error::Malformed(format!(
            "draft {draft_id} came back without its raw message"
        ))
    })?;
    parse_draft(&decode_body(raw)?, message.thread_id)
}

/// One RFC 2822 message, as the draft it can be written back as.
fn parse_draft(raw: &[u8], thread_id: String) -> Result<RawDraft> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| Error::Malformed("the draft is not a message that can be read".into()))?;
    let one = |address: Option<&Address>| -> String {
        mailboxes(address)
            .first()
            .map(|(name, address)| quoted(name.as_deref(), address))
            .unwrap_or_default()
    };
    let many = |address: Option<&Address>| -> Vec<String> {
        mailboxes(address)
            .iter()
            .map(|(name, address)| quoted(name.as_deref(), address))
            .collect()
    };
    let html = message.html_bodies().find_map(|part| match &part.body {
        PartType::Html(html) => Some(html.as_ref().to_string()),
        _ => None,
    });
    let text: Vec<&str> = message
        .text_bodies()
        .filter_map(|part| match &part.body {
            PartType::Text(text) => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    // A draft written in HTML alone keeps its HTML and gains the plain-text
    // reading of it, which is what this module does with any such message.
    let text = match text.join("\n").trim_end().to_string() {
        text if text.is_empty() => html.as_deref().map(html_to_text).unwrap_or_default(),
        text => text,
    };
    let mut attachments = Vec::new();
    let mut inline_ids = Vec::new();
    for (index, part) in message.parts.iter().enumerate() {
        let id = index as u32;
        // The bodies are the message, not files beside it, and a multipart
        // part is only the box the others came in.
        if part.is_multipart() || message.text_body.contains(&id) || message.html_body.contains(&id)
        {
            continue;
        }
        if let Some(content_id) = part.content_id() {
            inline_ids.push(
                content_id
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            );
            continue;
        }
        attachments.push(NewAttachment {
            filename: part.attachment_name().unwrap_or(UNNAMED_FILE).to_string(),
            mime_type: part_mime(part),
            bytes: part.contents().to_vec(),
        });
    }
    Ok(RawDraft {
        content: DraftContent {
            from: one(message.from()),
            to: many(message.to()),
            cc: many(message.cc()),
            bcc: many(message.bcc()),
            subject: message.subject().unwrap_or_default().to_string(),
            text,
            html,
            in_reply_to: identifiers(message.in_reply_to()).into_iter().next(),
            references: identifiers(message.references()),
            thread_id: Some(thread_id).filter(|id| !id.trim().is_empty()),
            attachments,
        },
        inline_ids,
    })
}

/// What one part says it is, as a content type a message can be built with.
fn part_mime(part: &mail_parser::MessagePart) -> String {
    match part.content_type() {
        Some(content_type) => match content_type.subtype() {
            Some(subtype) => format!("{}/{subtype}", content_type.ctype()),
            None => content_type.ctype().to_string(),
        },
        None => OCTET_STREAM.to_string(),
    }
}

/// `users.drafts.create`. The one way a message is ever written here.
pub async fn create_draft(
    client: &Client,
    connection_id: i64,
    content: &DraftContent,
) -> Result<DraftRef> {
    let mime = build_mime(content)?;
    let request = if content.attachments.is_empty() {
        client
            .service(GMAIL)
            .post(&format!("gmail/v1/users/{USER}/drafts"))?
            .json(&draft_body(content, &mime))
    } else {
        upload(
            client
                .service(GMAIL)
                .post(&format!("upload/gmail/v1/users/{USER}/drafts"))?,
            content,
            &mime,
        )
    };
    let wire: WireDraft = client.json(connection_id, request).await?;
    draft_ref(wire)
}

/// `users.drafts.update`, which replaces the whole message.
pub async fn update_draft(
    client: &Client,
    connection_id: i64,
    draft_id: &str,
    content: &DraftContent,
) -> Result<DraftRef> {
    let mime = build_mime(content)?;
    let request = if content.attachments.is_empty() {
        client
            .service(GMAIL)
            .put(&format!(
                "gmail/v1/users/{USER}/drafts/{}",
                urlencode(draft_id)
            ))?
            .json(&draft_body(content, &mime))
    } else {
        upload(
            client.service(GMAIL).put(&format!(
                "upload/gmail/v1/users/{USER}/drafts/{}",
                urlencode(draft_id)
            ))?,
            content,
            &mime,
        )
    };
    let wire: WireDraft = client.json(connection_id, request).await?;
    draft_ref(wire)
}

/// The same draft, sent the way Gmail takes a message too large to carry as
/// JSON: `uploadType=multipart`, a metadata part naming the conversation and
/// the raw message beside it as `message/rfc822`. A draft with a file
/// attached to it is such a message — base64 inside JSON would be a third
/// again as large as the file, and the JSON endpoint has its own limit well
/// under what one mail may carry.
fn upload(
    request: reqwest::RequestBuilder,
    content: &DraftContent,
    mime: &[u8],
) -> reqwest::RequestBuilder {
    let metadata = match &content.thread_id {
        Some(thread_id) => serde_json::json!({ "message": { "threadId": thread_id } }),
        None => serde_json::json!({ "message": {} }),
    };
    let boundary = multipart::boundary();
    let body = multipart::related(&boundary, &metadata.to_string(), "message/rfc822", mime);
    request
        .query(&[("uploadType", "multipart")])
        .header(reqwest::header::CONTENT_TYPE, multipart::header(&boundary))
        .body(body)
}

/// `users.drafts.delete`, the undo for a draft and the only delete in this
/// server.
pub async fn delete_draft(client: &Client, connection_id: i64, draft_id: &str) -> Result<()> {
    let request = client.service(GMAIL).delete(&format!(
        "gmail/v1/users/{USER}/drafts/{}",
        urlencode(draft_id)
    ))?;
    client.drain(connection_id, request).await
}

fn draft_ref(wire: WireDraft) -> Result<DraftRef> {
    let message = wire
        .message
        .ok_or_else(|| Error::Malformed("the draft came back without a message".into()))?;
    Ok(DraftRef {
        id: wire.id,
        thread_id: message.thread_id.clone(),
        message_id: message.id,
    })
}

fn draft_body(content: &DraftContent, mime: &[u8]) -> DraftRequest {
    DraftRequest {
        message: DraftMessage {
            raw: URL_SAFE_NO_PAD.encode(mime),
            thread_id: content.thread_id.clone(),
        },
    }
}

/// The RFC 2822 message a draft is made of, built with lettre. Text alone, or
/// text and HTML as alternatives so a plain-text reader still gets something;
/// with files, that same body goes inside a `multipart/mixed` with them.
///
/// A draft with no attachments is built exactly as it was before there were
/// any, down to the byte, and the module's tests hold it to that.
pub fn build_mime(content: &DraftContent) -> Result<Vec<u8>> {
    let mailbox = |value: &str| -> Result<Mailbox> {
        value
            .trim()
            .parse::<Mailbox>()
            .map_err(|e| Error::Malformed(format!("{value:?} is not an email address: {e}")))
    };
    let mut builder = lettre::Message::builder()
        // lettre takes the blind copies out of the message once it has made
        // an envelope from them, which is right for a mail server and wrong
        // here: Gmail is handed the message alone and reads the recipients
        // out of it, so a draft built without this loses every Bcc silently.
        .keep_bcc()
        .from(mailbox(&content.from)?)
        .subject(content.subject.clone());
    for address in &content.to {
        builder = builder.to(mailbox(address)?);
    }
    for address in &content.cc {
        builder = builder.cc(mailbox(address)?);
    }
    for address in &content.bcc {
        builder = builder.bcc(mailbox(address)?);
    }
    if let Some(in_reply_to) = &content.in_reply_to {
        builder = builder.header(InReplyTo::from(in_reply_to.clone()));
    }
    if !content.references.is_empty() {
        builder = builder.header(References::from(content.references.join(" ")));
    }
    let message = if content.attachments.is_empty() {
        match &content.html {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(
                content.text.clone(),
                html.clone(),
            )),
            None => builder.singlepart(SinglePart::plain(content.text.clone())),
        }
    } else {
        let body = match &content.html {
            Some(html) => MultiPart::mixed().multipart(MultiPart::alternative_plain_html(
                content.text.clone(),
                html.clone(),
            )),
            None => MultiPart::mixed().singlepart(SinglePart::plain(content.text.clone())),
        };
        let mut mixed = body;
        for file in &content.attachments {
            mixed = mixed.singlepart(attachment_part(file)?);
        }
        builder.multipart(mixed)
    }
    .map_err(|e| Error::Malformed(format!("the draft could not be built: {e}")))?;
    Ok(message.formatted())
}

/// One file as a MIME part. The type is whatever the upload settled on, and
/// a type that will not parse is refused here rather than written into a
/// header: the message must stay a message.
fn attachment_part(file: &NewAttachment) -> Result<SinglePart> {
    let content_type = ContentType::parse(&file.mime_type).map_err(|e| {
        Error::Malformed(format!(
            "{} is not a content type for {}: {e}",
            file.mime_type, file.filename
        ))
    })?;
    // Spelled out, because `Attachment` in this module is one that arrived.
    Ok(lettre::message::Attachment::new(file.filename.clone())
        .body(file.bytes.clone(), content_type))
}

/// A `Message-ID` header, for the rare caller that needs to set one.
#[allow(dead_code)]
pub fn message_id_header(value: &str) -> MessageId {
    MessageId::from(value.to_string())
}

// ---------------------------------------------------------------------------
// Google's own JSON, and the walk that turns it into the types above.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireMessageList {
    messages: Vec<WireMessageRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessageRef {
    id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireThread {
    id: String,
    messages: Vec<WireMessage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireDraftList {
    drafts: Vec<WireDraft>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireDraft {
    id: String,
    message: Option<WireMessage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireMessage {
    id: String,
    thread_id: String,
    label_ids: Vec<String>,
    snippet: Option<String>,
    /// Milliseconds since the epoch, as a string. More reliable than the
    /// `Date` header, which the sender writes.
    internal_date: Option<String>,
    payload: Option<WirePart>,
    /// The whole message, base64url, as `format=raw` answers it.
    raw: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WirePart {
    mime_type: String,
    filename: String,
    headers: Vec<WireHeader>,
    body: WireBody,
    parts: Vec<WirePart>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireHeader {
    name: String,
    value: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBody {
    attachment_id: Option<String>,
    size: u64,
    data: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSendAsList {
    send_as: Vec<WireSendAs>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireSendAs {
    send_as_email: String,
    display_name: Option<String>,
    is_default: bool,
    is_primary: bool,
    verification_status: Option<String>,
    reply_to_address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireLabelList {
    labels: Vec<WireLabel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireLabel {
    id: String,
    name: String,
    #[serde(rename = "type")]
    label_type: Option<String>,
    messages_total: Option<i64>,
    messages_unread: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModifyRequest {
    add_label_ids: Vec<String>,
    remove_label_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DraftRequest {
    message: DraftMessage,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DraftMessage {
    raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
}

/// What the walk over one message's parts collected.
#[derive(Default)]
struct Bodies {
    text: Vec<String>,
    html: Vec<String>,
    attachments: Vec<Attachment>,
    inline: Vec<InlineImage>,
}

impl WireMessage {
    fn header(&self, name: &str) -> Option<&str> {
        self.payload.as_ref().and_then(|p| p.header(name))
    }

    /// Every value of a header that may appear more than once. `Delivered-To`
    /// is written once per hop, so the first one is not always the only one.
    fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.payload.iter().flat_map(move |p| p.header_values(name))
    }

    fn date(&self) -> Option<DateTime<Utc>> {
        let millis: i64 = self.internal_date.as_ref()?.parse().ok()?;
        Utc.timestamp_millis_opt(millis).single()
    }

    /// A row of a listing. `format=metadata` answers headers and no part
    /// tree, so nothing here can say what a message carries; the count this
    /// used to hold was always zero against real Gmail, however many files
    /// the message had.
    fn summary(&self) -> MessageSummary {
        MessageSummary {
            id: self.id.clone(),
            thread_id: self.thread_id.clone(),
            date: self.date(),
            from: self.header("From").map(text_header),
            to: self.header("To").map(address_header).unwrap_or_default(),
            subject: self.header("Subject").map(text_header),
            snippet: self.snippet.clone(),
            labels: self.label_ids.clone(),
        }
    }

    fn message(&self) -> Message {
        let mut bodies = Bodies::default();
        if let Some(payload) = &self.payload {
            payload.walk(&mut bodies);
        }
        let text = bodies.text.join("\n").trim().to_string();
        let (text, text_from_html) = if text.is_empty() && !bodies.html.is_empty() {
            (html_to_text(&bodies.html.join("\n")), true)
        } else {
            (text, false)
        };
        Message {
            id: self.id.clone(),
            thread_id: self.thread_id.clone(),
            date: self.date(),
            from: self.header("From").map(text_header),
            to: self.header("To").map(address_header).unwrap_or_default(),
            cc: self.header("Cc").map(address_header).unwrap_or_default(),
            delivered_to: self
                .header_values("Delivered-To")
                .flat_map(address_header)
                .collect(),
            subject: self.header("Subject").map(text_header),
            snippet: self.snippet.clone(),
            labels: self.label_ids.clone(),
            message_id: self.header("Message-ID").map(|v| id_header(v).join(" ")),
            in_reply_to: self.header("In-Reply-To").map(|v| id_header(v).join(" ")),
            references: self.header("References").map(id_header).unwrap_or_default(),
            text,
            text_from_html,
            attachments: bodies.attachments,
            inline_images: bodies.inline,
        }
    }
}

impl WirePart {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    fn is_attachment(&self) -> bool {
        !self.filename.is_empty() || self.body.attachment_id.is_some()
    }

    /// Depth-first over the part tree. Text and HTML leaves become the body;
    /// anything with a filename or an attachment id becomes an attachment,
    /// unless it carries a `Content-ID`, in which case the body refers to it
    /// by that name and it is listed as an inline image instead.
    fn walk(&self, out: &mut Bodies) {
        if !self.parts.is_empty() {
            for part in &self.parts {
                part.walk(out);
            }
            return;
        }
        if let Some(content_id) = self.header("Content-ID") {
            let content_id = content_id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            if !content_id.is_empty() {
                out.inline.push(InlineImage {
                    content_id: content_id.to_string(),
                    attachment_id: self.body.attachment_id.clone(),
                    filename: self.filename.clone(),
                    mime_type: self.mime_type.clone(),
                    size: self.body.size,
                });
                return;
            }
        }
        if self.is_attachment() {
            out.attachments.push(Attachment {
                id: self.body.attachment_id.clone().unwrap_or_default(),
                filename: self.filename.clone(),
                mime_type: self.mime_type.clone(),
                size: self.body.size,
            });
            return;
        }
        let Some(data) = self.body.data.as_deref() else {
            return;
        };
        let Ok(bytes) = decode_body(data) else {
            return;
        };
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if self.mime_type.starts_with("text/html") {
            out.html.push(body);
        } else if self.mime_type.starts_with("text/") || self.mime_type.is_empty() {
            out.text.push(body);
        }
    }
}

/// Gmail encodes part bodies base64url, usually without padding. Standard
/// base64 is accepted too because attachment bytes have been seen with it.
fn decode_body(data: &str) -> Result<Vec<u8>> {
    let trimmed: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let unpadded = trimmed.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(unpadded)
        .or_else(|_| BASE64.decode(unpadded))
        .map_err(|e| Error::Malformed(format!("gmail sent a body this is not base64: {e}")))
}

/// An unstructured header (`Subject`), with RFC 2047 encoded words decoded.
fn text_header(value: &str) -> String {
    match parse(value, |s| s.parse_unstructured()) {
        HeaderValue::Text(text) => text.trim().to_string(),
        _ => value.trim().to_string(),
    }
}

/// An address header, as `Name <address>` or the bare address.
fn address_header(value: &str) -> Vec<String> {
    match parse(value, |s| s.parse_address()) {
        HeaderValue::Address(address) => mailboxes(Some(&address))
            .into_iter()
            .map(|(name, address)| match name {
                Some(name) => format!("{name} <{address}>"),
                None => address,
            })
            .collect(),
        _ => vec![value.trim().to_string()],
    }
}

/// Every mailbox an address header names, as its display name and its
/// address. Groups are flattened: a draft is written to people, and a group
/// is a list of them.
fn mailboxes(address: Option<&Address>) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    let mut push = |addr: &mail_parser::Addr| {
        let address = addr.address.as_deref().unwrap_or_default().trim();
        if address.is_empty() {
            return;
        }
        let name = addr
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        out.push((name, address.to_string()));
    };
    match address {
        Some(Address::List(list)) => list.iter().for_each(&mut push),
        Some(Address::Group(groups)) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .for_each(&mut push),
        None => {}
    }
    out
}

/// An identifier header (`Message-ID`, `In-Reply-To`, `References`), as the
/// list of ids it names, angle brackets stripped by the parser.
fn id_header(value: &str) -> Vec<String> {
    let ids = identifiers(&parse(value, |s| s.parse_id()));
    if ids.is_empty() {
        return value.split_whitespace().map(wrap_id).collect();
    }
    ids
}

/// The ids inside an identifier header a message was parsed from, each one in
/// the angle brackets a `References` line is written with.
fn identifiers(value: &HeaderValue) -> Vec<String> {
    match value {
        HeaderValue::Text(id) => vec![wrap_id(id)],
        HeaderValue::TextList(ids) => ids.iter().map(|id| wrap_id(id)).collect(),
        _ => Vec::new(),
    }
}

fn wrap_id(id: &str) -> String {
    format!(
        "<{}>",
        id.trim().trim_start_matches('<').trim_end_matches('>')
    )
}

/// mail-parser's field parsers read a header value up to its terminating
/// newline, so one is added before handing the value over.
fn parse(
    value: &str,
    parser: impl for<'a> FnOnce(&mut MessageStream<'a>) -> HeaderValue<'a>,
) -> HeaderValue<'static> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    parser(&mut MessageStream::new(&bytes)).into_owned()
}

/// HTML to something readable. mail-parser's own converter is used rather
/// than a second HTML crate: it is already a dependency, it is what the rest
/// of this module parses mail with, and it handles the entity and block-level
/// cases an email body actually contains.
pub fn html_to_text(html: &str) -> String {
    let text = mail_parser::decoders::html::html_to_text(html);
    // The converter leaves runs of blank lines where a table or a div stack
    // was; a model reads the result better without them.
    let mut out = String::with_capacity(text.len());
    let mut blank = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}
