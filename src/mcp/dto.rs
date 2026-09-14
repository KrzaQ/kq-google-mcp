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
    /// How many files the message carries, when that was read. A listing
    /// leaves it out rather than saying zero: Gmail answers a listing without
    /// the part tree, so nothing there can count what a message holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<usize>,
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
            attachments: None,
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
    /// The `From` the draft carries, and why that address was chosen. A reply
    /// picks one on its own, so a wrong guess is visible here rather than
    /// silent; tell the person which address the draft is from.
    pub from: Option<String>,
    pub from_reason: Option<String>,
    /// Why a reply went to these recipients, when the answer is not the
    /// obvious one. Set when the original was a message the account itself
    /// sent, because then the reply goes to the original's recipients rather
    /// than to its sender; tell the person, so a reply that went somewhere
    /// they did not expect is visible here.
    pub to_reason: Option<String>,
    /// The files this draft actually carries, by name. Read them back to the
    /// person: what they are told is attached and what the mail carries have
    /// to be the same thing.
    pub attachments: Vec<String>,
    /// Set when the body talks about attaching something and the draft
    /// carries nothing. A mail that promises three files and carries none is
    /// the failure this whole field exists for; say it to the person rather
    /// than reporting the draft as done.
    pub attachment_warning: Option<String>,
    /// Where the person opens the draft and presses send. This server never
    /// sends anything itself.
    pub url: String,
    pub note: String,
}

/// The words a body uses when it says a file is coming with it, in the two
/// languages this server is written and read in. They are stems rather than
/// whole words, so `attach` covers attached and attachment, `załącz` covers
/// załączam and w załączniku, and `dołącz` covers dołączam.
///
/// Without diacritics as well, because that is how half of Polish is typed.
const ATTACHMENT_WORDS: [&str; 6] = ["attach", "enclos", "załąc", "zalacz", "dołąc", "dolacz"];

/// Whether a draft's body promises a file. Matched on the lower-cased text:
/// `eq_ignore_ascii_case` and its relatives leave `Ł` and `Ą` exactly as they
/// were, so a body that opens with `Załączam` would go unnoticed.
///
/// It errs towards saying yes. A false positive costs one line in a result
/// the model reads; a false negative costs a mail that promised a file and
/// carried nothing, which is what this is here to stop.
pub fn promises_attachment(text: &str) -> bool {
    let lower = text.to_lowercase();
    ATTACHMENT_WORDS.iter().any(|word| lower.contains(word))
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SendAsOut {
    pub account: String,
    pub count: usize,
    pub addresses: Vec<SendAsAddressOut>,
    pub note: String,
}

/// One address the account may write mail as.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SendAsAddressOut {
    /// The bare address, which is what the draft tools' `from` takes.
    pub address: String,
    /// The header a draft carries when this address is chosen.
    pub from: String,
    pub display_name: Option<String>,
    /// True for the address Gmail composes with when nobody chooses.
    pub is_default: bool,
    /// True for the Google account's own address.
    pub is_primary: bool,
    /// False when Google has not verified the alias. Gmail would rewrite the
    /// `From` on send, so the draft tools refuse it.
    pub usable_as_from: bool,
    /// Google's own word: "accepted" or "pending".
    pub verification_status: Option<String>,
    pub reply_to: Option<String>,
}

impl From<gmail::SendAs> for SendAsAddressOut {
    fn from(s: gmail::SendAs) -> Self {
        Self {
            from: s.header(),
            usable_as_from: s.usable(),
            address: s.email,
            display_name: s.display_name,
            is_default: s.is_default,
            is_primary: s.is_primary,
            verification_status: s.verification_status,
            reply_to: s.reply_to,
        }
    }
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

/// Where one file goes on its way into a draft. There is no id here: the id
/// that matters comes back from the upload itself, so a model cannot mistake
/// the ticket for the file.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UploadLinkOut {
    pub url: String,
    pub filename: String,
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

/// The pictures a document holds, in the order they appear in it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocImagesOut {
    pub account: String,
    pub doc_id: String,
    pub title: String,
    pub url: String,
    pub count: usize,
    pub images: Vec<DocImageOut>,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocImageOut {
    /// What to pass as `image`: image1 is the first picture in the document,
    /// and these are the labels the text of a read document is left with.
    pub image: String,
    /// Docs' own id for the object, which also works as `image` and which
    /// stays the same when a picture is added above this one.
    pub object_id: String,
    /// The alt text, when the document carries any.
    pub alt_title: Option<String>,
    pub alt_text: Option<String>,
    /// How large the picture is in the document, in points. Docs says nothing
    /// about how many bytes it is.
    pub width_pt: Option<i64>,
    pub height_pt: Option<i64>,
    /// A drawing or a chart has no picture of its own, so there is nothing to
    /// look at or to download; open the document to see it.
    pub fetchable: bool,
}

impl From<docs::InlineImage> for DocImageOut {
    fn from(i: docs::InlineImage) -> Self {
        Self {
            image: i.label,
            object_id: i.object_id,
            alt_title: i.alt_title,
            alt_text: i.alt_text,
            width_pt: i.width_pt,
            height_pt: i.height_pt,
            fetchable: i.content_uri.is_some(),
        }
    }
}

/// A document as numbered paragraphs: the read every careful write starts
/// from.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocParagraphsOut {
    pub account: String,
    pub doc_id: String,
    pub title: String,
    pub url: String,
    /// What the document is at right now. Every write takes it as
    /// `revision_id`, and one write makes it stale.
    pub revision_id: String,
    /// How many paragraphs the document has, whatever this answer shows.
    pub count: usize,
    /// The range shown, 1-based and inclusive.
    pub from: usize,
    pub to: usize,
    pub paragraphs: Vec<DocParagraphOut>,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocParagraphOut {
    /// What to pass as `paragraph`, `after_paragraph` or `before_paragraph`.
    pub paragraph: usize,
    /// Docs' own name for the style: NORMAL_TEXT, HEADING_2, TITLE.
    pub style: String,
    /// How many characters the paragraph holds, before any cut below.
    pub chars: usize,
    /// True when the paragraph sits in a table cell.
    pub in_table: bool,
    pub text: String,
    /// True when `text` was cut to keep the answer small; ask for this
    /// paragraph again with full=true to read all of it.
    pub truncated: bool,
}

/// A document as the runs it is made of: what `docs_read` cannot say,
/// because the markdown export throws every colour, font and weight away.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocFormattingOut {
    pub account: String,
    pub doc_id: String,
    pub title: String,
    pub url: String,
    /// The same revision id docs_list_paragraphs answers with, so a write can
    /// be planned straight off this read.
    pub revision_id: String,
    /// How many paragraphs the document has, whatever this answer shows.
    pub count: usize,
    /// The range shown, 1-based and inclusive.
    pub from: usize,
    pub to: usize,
    pub paragraphs: Vec<DocRunsOut>,
    pub note: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocRunsOut {
    /// What to pass as `paragraph` to a write.
    pub paragraph: usize,
    /// Docs' own name for the style: NORMAL_TEXT, HEADING_2, TITLE.
    pub style: String,
    /// How many characters the paragraph holds.
    pub chars: usize,
    pub in_table: bool,
    /// The runs, in order and covering the paragraph end to end.
    pub runs: Vec<DocRunOut>,
}

/// One run of a paragraph. Only what the document sets is here: a run with no
/// colour, no weight and no font of its own answers `start`, `end` and `text`
/// and nothing else, so a plain paragraph is one bare run.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocRunOut {
    /// The first character of the run, counting from 0 at the start of the
    /// paragraph. These are the units docs_insert_code takes for its spans.
    pub start: usize,
    /// One past the last character of the run.
    pub end: usize,
    pub text: String,
    /// The foreground colour, as #rrggbb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// The size in points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
}

impl From<docs::StyledRun> for DocRunOut {
    fn from(run: docs::StyledRun) -> Self {
        Self {
            start: run.start,
            end: run.end,
            text: run.text,
            colour: run.style.colour,
            bold: run.style.bold,
            italic: run.style.italic,
            font: run.style.font,
            size: run.style.size,
        }
    }
}

/// What one paragraph write did, and what it invalidated by doing it.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DocEditOut {
    pub account: String,
    pub doc_id: String,
    pub url: String,
    /// The paragraph the write was aimed at, as it was numbered when the
    /// write was planned.
    pub paragraph: usize,
    /// What was written, in one line the model can repeat to the person.
    pub written: String,
    /// The text this write leaves behind: the changed paragraph, the
    /// inserted one, or the name of the picture that was put in.
    pub text: String,
    /// Always the same sentence: read the document again before writing to it
    /// again.
    pub next: String,
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
    /// How the cells were read: `formatted`, `formula` or `unformatted`. A
    /// model that asked for one and reads another would draw the wrong
    /// conclusion from the same digits.
    pub render: String,
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
