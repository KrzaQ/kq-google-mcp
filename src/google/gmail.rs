//! Gmail: search, read, labels and drafts. The endpoints below are the whole
//! surface, and what is missing is missing on purpose — there is no `send`,
//! no `trash` and no `delete` on a message anywhere in this file, and the
//! module's tests grep it to keep it that way. A draft is the deliverable and
//! the person sends it from Gmail.
//!
//! Messages come back from Google in `format=full`, which is already
//! decomposed into MIME parts with an `attachmentId` per attachment — the id
//! the attachments endpoint needs, and the reason the raw format is not used.
//! `mail-parser` does the part of the job Google leaves alone: decoding header
//! values (RFC 2047 words, address lists, identifier lists) and turning an
//! HTML-only body into something readable.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeZone, Utc};
use lettre::message::header::{InReplyTo, MessageId, References};
use lettre::message::{Mailbox, MultiPart, SinglePart};
use mail_parser::parsers::MessageStream;
use mail_parser::{Address, HeaderValue};
use serde::{Deserialize, Serialize};

use super::client::{Client, Error, Result, urlencode};

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
    pub attachment_count: usize,
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

/// What a draft is made of. There is no attachment field: attaching files is
/// deliberately out of this release.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftContent {
    /// The connected account; Gmail rewrites it, but RFC 2822 needs it.
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
}

impl DraftContent {
    /// A reply to `message`, with the threading the plan spells out: the
    /// replied message's `Message-ID` in `In-Reply-To`, its `References` plus
    /// that id in `References`, its thread id on the draft, and its subject
    /// with one `Re:`.
    pub fn reply_to(message: &Message, from: &str, body: &str, reply_all: bool) -> Self {
        let mut to: Vec<String> = message.from.clone().into_iter().collect();
        let mut cc = Vec::new();
        if reply_all {
            // Everyone the message went to, minus the account replying.
            for address in message.to.iter().chain(message.cc.iter()) {
                if !same_address(address, from) && !to.iter().any(|t| same_address(t, address)) {
                    cc.push(address.clone());
                }
            }
        }
        to.retain(|address| !same_address(address, from) || message.to.len() <= 1);
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
        }
    }
}

/// `Re: ` exactly once, however the original was spelled.
fn reply_subject(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or_default().trim();
    if subject.len() >= 3 && subject[..3].eq_ignore_ascii_case("re:") {
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

fn bare_address(value: &str) -> String {
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

/// `users.drafts.get`, the draft read back in full.
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

/// `users.drafts.create`. The one way a message is ever written here.
pub async fn create_draft(
    client: &Client,
    connection_id: i64,
    content: &DraftContent,
) -> Result<DraftRef> {
    let request = client
        .service(GMAIL)
        .post(&format!("gmail/v1/users/{USER}/drafts"))?
        .json(&draft_body(content)?);
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
    let request = client
        .service(GMAIL)
        .put(&format!(
            "gmail/v1/users/{USER}/drafts/{}",
            urlencode(draft_id)
        ))?
        .json(&draft_body(content)?);
    let wire: WireDraft = client.json(connection_id, request).await?;
    draft_ref(wire)
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

fn draft_body(content: &DraftContent) -> Result<DraftRequest> {
    Ok(DraftRequest {
        message: DraftMessage {
            raw: URL_SAFE_NO_PAD.encode(build_mime(content)?),
            thread_id: content.thread_id.clone(),
        },
    })
}

/// The RFC 2822 message a draft is made of, built with lettre. Text alone, or
/// text and HTML as alternatives so a plain-text reader still gets something.
pub fn build_mime(content: &DraftContent) -> Result<Vec<u8>> {
    let mailbox = |value: &str| -> Result<Mailbox> {
        value
            .trim()
            .parse::<Mailbox>()
            .map_err(|e| Error::Malformed(format!("{value:?} is not an email address: {e}")))
    };
    let mut builder = lettre::Message::builder()
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
    let message = match &content.html {
        Some(html) => builder.multipart(MultiPart::alternative_plain_html(
            content.text.clone(),
            html.clone(),
        )),
        None => builder.singlepart(SinglePart::plain(content.text.clone())),
    }
    .map_err(|e| Error::Malformed(format!("the draft could not be built: {e}")))?;
    Ok(message.formatted())
}

/// A `Message-ID` header, for the rare caller that needs to set one.
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

    fn date(&self) -> Option<DateTime<Utc>> {
        let millis: i64 = self.internal_date.as_ref()?.parse().ok()?;
        Utc.timestamp_millis_opt(millis).single()
    }

    fn summary(&self) -> MessageSummary {
        let mut bodies = Bodies::default();
        if let Some(payload) = &self.payload {
            payload.walk(&mut bodies);
        }
        MessageSummary {
            id: self.id.clone(),
            thread_id: self.thread_id.clone(),
            date: self.date(),
            from: self.header("From").map(text_header),
            to: self.header("To").map(address_header).unwrap_or_default(),
            subject: self.header("Subject").map(text_header),
            snippet: self.snippet.clone(),
            labels: self.label_ids.clone(),
            attachment_count: bodies.attachments.len(),
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
    let mut out = Vec::new();
    let mut push = |addr: &mail_parser::Addr| {
        let address = addr.address.as_deref().unwrap_or_default().trim();
        if address.is_empty() {
            return;
        }
        out.push(match addr.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{name} <{address}>"),
            _ => address.to_string(),
        });
    };
    match parse(value, |s| s.parse_address()) {
        HeaderValue::Address(Address::List(list)) => list.iter().for_each(&mut push),
        HeaderValue::Address(Address::Group(groups)) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .for_each(&mut push),
        _ => out.push(value.trim().to_string()),
    }
    out
}

/// An identifier header (`Message-ID`, `In-Reply-To`, `References`), as the
/// list of ids it names, angle brackets stripped by the parser.
fn id_header(value: &str) -> Vec<String> {
    let wrap = |id: &str| {
        format!(
            "<{}>",
            id.trim().trim_start_matches('<').trim_end_matches('>')
        )
    };
    match parse(value, |s| s.parse_id()) {
        HeaderValue::Text(id) => vec![wrap(&id)],
        HeaderValue::TextList(ids) => ids.iter().map(|id| wrap(id)).collect(),
        _ => value.split_whitespace().map(wrap).collect(),
    }
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
