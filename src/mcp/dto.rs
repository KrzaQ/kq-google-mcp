//! What the tools answer with.
//!
//! A Google payload is never handed on as it arrived: it is wide, it repeats
//! itself, and half of it is of no use to a model that has a few thousand
//! tokens to spend on the answer. Every tool result is one of the small
//! shapes below, built from the typed values `google/` returns.
//!
//! Instants are RFC 3339 strings rather than `DateTime`, because these types
//! carry a JSON schema to the client and the schema generator has no opinion
//! about chrono.
//!
//! Every one of them is written on the acting person's clock, with the offset
//! that was in force at that instant: `2026-09-10T13:35:28+02:00` for a
//! September morning in Warsaw and `+01:00` for a January one. A model that
//! reads `11:35Z` tells the person their mail arrived two hours before it did,
//! so nothing here renders UTC unless the person's zone is UTC. The zone
//! arrives as an argument from the call rather than from a global, because two
//! people on one server have two different clocks.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rmcp::schemars;
use serde::Serialize;

use crate::db::Connection;
use crate::google::{calendar, docs, drive, gmail, sheets};

/// One instant, on `tz`'s clock and with `tz`'s offset for that day.
pub fn instant(at: Option<DateTime<Utc>>, tz: Tz) -> Option<String> {
    at.map(|at| at_zone(at, tz))
}

pub fn at_zone(at: DateTime<Utc>, tz: Tz) -> String {
    at.with_timezone(&tz)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AccountsOut {
    pub accounts: Vec<AccountOut>,
    /// The IANA zone every time in every tool is shown in and read on, for
    /// this person.
    pub timezone: String,
    /// What time it is on that clock right now, so "today" and "this
    /// afternoon" need no guessing.
    pub now: String,
    /// What to do when the list is empty or an account needs attention.
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AccountOut {
    /// What every other tool's `account` argument takes.
    pub label: String,
    pub google_email: String,
    /// Which of gmail, drive, docs, sheets, calendar this account was
    /// connected with. A tool for a service that is missing is refused.
    pub services: Vec<String>,
    pub status: String,
    /// True when the person must reconnect this account in the portal before
    /// anything works; retrying will not help.
    pub needs_reauth: bool,
    pub last_used_at: Option<String>,
}

impl AccountOut {
    pub fn new(c: &Connection, tz: Tz) -> Self {
        Self {
            label: c.label.clone(),
            google_email: c.google_email.clone(),
            services: c.services.clone(),
            status: c.status.to_string(),
            needs_reauth: c.status == crate::db::ConnectionStatus::NeedsReauth,
            last_used_at: instant(c.last_used_at, tz),
        }
    }
}

// ----- gmail ----------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessagesOut {
    pub account: String,
    pub count: usize,
    pub messages: Vec<MessageBriefOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessageBriefOut {
    pub message_id: String,
    pub thread_id: String,
    pub date: Option<String>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub labels: Vec<String>,
    pub attachments: usize,
}

impl MessageBriefOut {
    pub fn new(m: gmail::MessageSummary, tz: Tz) -> Self {
        Self {
            message_id: m.id,
            thread_id: m.thread_id,
            date: instant(m.date, tz),
            from: m.from,
            to: m.to,
            subject: m.subject,
            snippet: m.snippet,
            labels: m.labels,
            attachments: m.attachment_count,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ThreadOut {
    pub account: String,
    pub thread_id: String,
    pub messages: Vec<MessageOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MessageOut {
    pub message_id: String,
    pub thread_id: String,
    pub date: Option<String>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub labels: Vec<String>,
    /// The RFC 2822 `Message-ID`, which is what a reply threads against.
    pub rfc822_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub text: String,
    /// True when there was no plain-text part and the text above was made
    /// from the HTML one.
    pub text_from_html: bool,
    pub attachments: Vec<AttachmentOut>,
    /// Pictures embedded in the body. `gmail_view_image` takes either the
    /// `attachment_id` or the `content_id` of one of these.
    pub inline_images: Vec<InlineImageOut>,
}

impl MessageOut {
    pub fn new(m: gmail::Message, tz: Tz) -> Self {
        Self {
            message_id: m.id,
            thread_id: m.thread_id,
            date: instant(m.date, tz),
            from: m.from,
            to: m.to,
            cc: m.cc,
            subject: m.subject,
            labels: m.labels,
            rfc822_message_id: m.message_id,
            in_reply_to: m.in_reply_to,
            references: m.references,
            text: m.text,
            text_from_html: m.text_from_html,
            attachments: m.attachments.iter().map(AttachmentOut::from).collect(),
            inline_images: m.inline_images.iter().map(InlineImageOut::from).collect(),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct AttachmentOut {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

impl From<&gmail::Attachment> for AttachmentOut {
    fn from(a: &gmail::Attachment) -> Self {
        Self {
            attachment_id: a.id.clone(),
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InlineImageOut {
    pub content_id: String,
    pub attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

impl From<&gmail::InlineImage> for InlineImageOut {
    fn from(i: &gmail::InlineImage) -> Self {
        Self {
            content_id: i.content_id.clone(),
            attachment_id: i.attachment_id.clone(),
            filename: i.filename.clone(),
            mime_type: i.mime_type.clone(),
            size: i.size,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LabelsOut {
    pub account: String,
    pub labels: Vec<LabelOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LabelOut {
    pub id: String,
    pub name: String,
    /// "system" for Gmail's own labels, "user" for the person's.
    pub kind: Option<String>,
    pub messages_total: Option<i64>,
    pub messages_unread: Option<i64>,
}

impl From<gmail::Label> for LabelOut {
    fn from(l: gmail::Label) -> Self {
        Self {
            id: l.id,
            name: l.name,
            kind: l.kind,
            messages_total: l.messages_total,
            messages_unread: l.messages_unread,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DraftOut {
    pub account: String,
    pub draft_id: String,
    pub message_id: String,
    pub thread_id: String,
    /// Where the person opens the draft and presses send. This server never
    /// sends anything itself.
    pub url: String,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DraftsOut {
    pub account: String,
    pub count: usize,
    pub drafts: Vec<DraftBriefOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DraftBriefOut {
    pub draft_id: String,
    pub url: String,
    pub message: MessageBriefOut,
}

impl DraftBriefOut {
    pub fn new(d: gmail::DraftSummary, tz: Tz) -> Self {
        Self {
            url: format!("https://mail.google.com/mail/u/0/#drafts?compose={}", d.id),
            draft_id: d.id,
            message: MessageBriefOut::new(d.message, tz),
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ModifiedOut {
    pub account: String,
    pub modified: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub messages: Vec<MessageBriefOut>,
    /// The messages that could not be changed, with what Google said about
    /// each. The rest were changed all the same, so a retry should name only
    /// these.
    pub failed: Vec<FailedMessageOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FailedMessageOut {
    pub message_id: String,
    pub error: String,
}

// ----- files, links and text -------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LinkOut {
    pub url: String,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<i64>,
    pub expires_at: String,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TextOut {
    pub account: String,
    /// What the text was made from: "text", "pdftotext", "docx",
    /// "google-doc" or "google-sheet".
    pub source: String,
    pub filename: Option<String>,
    pub chars: usize,
    /// How many characters were cut off the end; zero when nothing was.
    pub truncated_chars: usize,
    pub text: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FilesOut {
    pub account: String,
    pub count: usize,
    pub files: Vec<FileOut>,
    /// Present only when `modified_after` had to be decided, such as an hour
    /// the clock change made happen twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FileOut {
    pub file_id: String,
    pub name: String,
    pub mime_type: String,
    pub modified_time: Option<String>,
    pub size: Option<u64>,
    pub owners: Vec<String>,
    pub web_view_link: Option<String>,
}

impl FileOut {
    pub fn new(f: drive::FileMeta, tz: Tz) -> Self {
        Self {
            file_id: f.id,
            name: f.name,
            mime_type: f.mime_type,
            modified_time: instant(f.modified_time, tz),
            size: f.size,
            owners: f.owners,
            web_view_link: f.web_view_link,
        }
    }
}

// ----- docs ------------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocOut {
    pub account: String,
    pub doc_id: String,
    pub title: String,
    pub url: String,
    pub tabs: Vec<DocTabOut>,
    pub chars: usize,
    pub truncated_chars: usize,
    /// The document as markdown, through Drive's export.
    pub text: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocTabOut {
    pub id: String,
    pub title: String,
}

impl From<docs::Tab> for DocTabOut {
    fn from(t: docs::Tab) -> Self {
        Self {
            id: t.id,
            title: t.title,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocWriteOut {
    pub account: String,
    pub doc_id: String,
    pub url: String,
    pub title: Option<String>,
    /// What happened, in one line the model can repeat to the person.
    pub written: String,
    /// How many occurrences `docs_replace_text` replaced.
    pub replacements: Option<i64>,
}

// ----- sheets ----------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SpreadsheetOut {
    pub account: String,
    pub spreadsheet_id: String,
    pub title: String,
    pub url: String,
    pub tabs: Vec<TabOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TabOut {
    pub title: String,
    pub index: i64,
    pub rows: i64,
    pub columns: i64,
}

impl From<sheets::Tab> for TabOut {
    fn from(t: sheets::Tab) -> Self {
        Self {
            title: t.title,
            index: t.index,
            rows: t.rows,
            columns: t.columns,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RangeOut {
    pub account: String,
    pub spreadsheet_id: String,
    pub range: String,
    pub row_count: usize,
    /// True when `max_rows` cut the answer short.
    pub truncated: bool,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SheetWriteOut {
    pub account: String,
    pub spreadsheet_id: String,
    pub url: Option<String>,
    pub updated_range: Option<String>,
    pub updated_rows: Option<i64>,
    pub updated_cells: Option<i64>,
    pub written: String,
}

// ----- calendar --------------------------------------------------------------

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CalendarsOut {
    pub account: String,
    pub calendars: Vec<CalendarOut>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CalendarOut {
    pub calendar_id: String,
    pub summary: String,
    pub primary: bool,
    pub access_role: Option<String>,
    pub time_zone: Option<String>,
}

impl From<calendar::Calendar> for CalendarOut {
    fn from(c: calendar::Calendar) -> Self {
        Self {
            calendar_id: c.id,
            summary: c.summary,
            primary: c.primary,
            access_role: c.access_role,
            time_zone: c.time_zone,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventsOut {
    pub account: String,
    pub calendar_id: String,
    pub from: String,
    pub to: String,
    pub count: usize,
    pub events: Vec<EventOut>,
    /// Present only when a time given for the window had to be decided, such
    /// as an hour the clock change made happen twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventOut {
    pub event_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// RFC 3339 for a timed event, `YYYY-MM-DD` for an all-day one.
    pub start: Option<String>,
    pub end: Option<String>,
    pub all_day: bool,
    pub status: Option<String>,
    pub url: Option<String>,
    pub organizer: Option<String>,
    /// How many people are invited. This server never adds or notifies any:
    /// an event with attendees can be read but not changed here.
    pub attendees: usize,
}

impl EventOut {
    pub fn new(e: calendar::Event, tz: Tz) -> Self {
        let all_day = e.start.date.is_some();
        Self {
            event_id: e.id,
            title: e.summary,
            description: e.description,
            location: e.location,
            start: when(&e.start, tz),
            end: when(&e.end, tz),
            all_day,
            status: e.status,
            url: e.html_link,
            organizer: e.organizer,
            attendees: e.attendee_count,
        }
    }
}

fn when(w: &calendar::When, tz: Tz) -> Option<String> {
    w.date
        .clone()
        .or_else(|| instant(w.date_time.map(|d| d.with_timezone(&Utc)), tz))
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EventWriteOut {
    pub account: String,
    pub calendar_id: String,
    pub written: String,
    /// Absent after a delete.
    pub event: Option<EventOut>,
    pub note: String,
}

// ----- the answer to `confirmed = false` -------------------------------------

/// What a Docs, Sheets or Calendar write answers when `confirmed` is false:
/// what it would do, and nothing done. This is a result and not an error,
/// because showing the person the change is the step being asked for.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PreviewOut {
    pub confirmed: bool,
    pub written: bool,
    /// One line naming the change.
    pub action: String,
    /// The change itself, as lines to show the person.
    pub details: Vec<String>,
    pub next: String,
}

impl PreviewOut {
    pub fn new(action: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            confirmed: false,
            written: false,
            action: action.into(),
            details,
            next: "Nothing was written. Show the person exactly this and call the same tool \
                   again with confirmed=true only after they say yes."
                .into(),
        }
    }
}

/// Either a preview or the write's own answer. A tool returns one enum so the
/// two shapes travel under one schema and the model is never surprised.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Confirmable<T> {
    Preview(PreviewOut),
    Done(T),
}
