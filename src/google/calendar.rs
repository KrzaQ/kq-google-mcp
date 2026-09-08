//! Calendar: the calendars an account can see, events in a window, and the
//! three mutations.
//!
//! Two rules are structural here rather than checked at the edge. **Every
//! mutation passes `sendUpdates=none`**, so nothing this server does ever
//! puts a notification in somebody's inbox. And the request types have no
//! `attendees` field at all — not an ignored one, not an empty one — so a
//! draft agenda cannot turn into an invitation by any route, including a
//! future caller that means well. The response type reports how many
//! attendees an event has, which is what lets a tool refuse to touch a
//! meeting that has them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::client::{Client, Result};

/// The calendar a tool uses when the caller does not name one.
pub const PRIMARY: &str = "primary";
/// Never, on any mutation. The one place this string appears.
const SEND_UPDATES: (&str, &str) = ("sendUpdates", "none");

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Calendar {
    pub id: String,
    pub summary: String,
    pub description: Option<String>,
    pub primary: bool,
    /// `owner`, `writer`, `reader`, `freeBusyReader`.
    pub access_role: Option<String>,
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub id: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: When,
    pub end: When,
    pub status: Option<String>,
    pub html_link: Option<String>,
    pub organizer: Option<String>,
    pub updated: Option<DateTime<Utc>>,
    /// How many people are on the invitation. Anything above zero is a
    /// meeting, and this release refuses to change one.
    pub attendee_count: usize,
}

impl Event {
    pub fn has_attendees(&self) -> bool {
        self.attendee_count > 0
    }
}

/// An instant, or a whole day. Calendar uses one of the two and never both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct When {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_time: Option<DateTime<Utc>>,
    /// `YYYY-MM-DD` for an all-day event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

impl When {
    pub fn at(instant: DateTime<Utc>) -> Self {
        When {
            date_time: Some(instant),
            date: None,
            time_zone: Some("UTC".to_string()),
        }
    }

    /// An all-day event. Calendar's end date is exclusive, which the caller
    /// has to know; the tool description says so.
    pub fn all_day(date: impl Into<String>) -> Self {
        When {
            date_time: None,
            date: Some(date.into()),
            time_zone: None,
        }
    }
}

/// What an event is made of when this server writes one. There is no
/// `attendees` field, and adding one is a decision, not a patch.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventDraft {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<When>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<When>,
}

/// The window and filter of an events query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventQuery {
    pub time_min: Option<DateTime<Utc>>,
    pub time_max: Option<DateTime<Utc>>,
    pub query: Option<String>,
    pub max: Option<u32>,
}

/// `calendarList.list`.
pub async fn list_calendars(client: &Client, connection_id: i64) -> Result<Vec<Calendar>> {
    let request = client
        .get("calendar/v3/users/me/calendarList")
        .query(&[("minAccessRole", "reader")]);
    let wire: WireCalendarList = client.json(connection_id, request).await?;
    Ok(wire.items.into_iter().map(Into::into).collect())
}

/// `events.list`. Recurring events are expanded (`singleEvents=true`) and
/// ordered by start, which is the only ordering Calendar allows once they are.
pub async fn list_events(
    client: &Client,
    connection_id: i64,
    calendar_id: &str,
    query: &EventQuery,
) -> Result<Vec<Event>> {
    let mut request = client
        .get(&format!(
            "calendar/v3/calendars/{}/events",
            urlencode(calendar_id)
        ))
        .query(&[("singleEvents", "true"), ("orderBy", "startTime")]);
    if let Some(min) = query.time_min {
        request = request.query(&[("timeMin", rfc3339(min))]);
    }
    if let Some(max) = query.time_max {
        request = request.query(&[("timeMax", rfc3339(max))]);
    }
    if let Some(q) = query.query.as_deref().map(str::trim)
        && !q.is_empty()
    {
        request = request.query(&[("q", q)]);
    }
    if let Some(max) = query.max {
        request = request.query(&[("maxResults", max.to_string())]);
    }
    let wire: WireEventList = client.json(connection_id, request).await?;
    Ok(wire.items.into_iter().map(Into::into).collect())
}

/// `events.get`.
pub async fn get_event(
    client: &Client,
    connection_id: i64,
    calendar_id: &str,
    event_id: &str,
) -> Result<Event> {
    let request = client.get(&format!(
        "calendar/v3/calendars/{}/events/{}",
        urlencode(calendar_id),
        urlencode(event_id)
    ));
    let wire: WireEvent = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `events.insert`, with `sendUpdates=none`.
pub async fn insert_event(
    client: &Client,
    connection_id: i64,
    calendar_id: &str,
    draft: &EventDraft,
) -> Result<Event> {
    let request = client
        .post(&format!(
            "calendar/v3/calendars/{}/events",
            urlencode(calendar_id)
        ))
        .query(&[SEND_UPDATES])
        .json(draft);
    let wire: WireEvent = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `events.patch`, with `sendUpdates=none`. Only the fields the draft sets
/// are sent, so an unmentioned field keeps its value.
pub async fn patch_event(
    client: &Client,
    connection_id: i64,
    calendar_id: &str,
    event_id: &str,
    draft: &EventDraft,
) -> Result<Event> {
    let request = client
        .patch(&format!(
            "calendar/v3/calendars/{}/events/{}",
            urlencode(calendar_id),
            urlencode(event_id)
        ))
        .query(&[SEND_UPDATES])
        .json(draft);
    let wire: WireEvent = client.json(connection_id, request).await?;
    Ok(wire.into())
}

/// `events.delete`, with `sendUpdates=none`. The one deletion outside drafts,
/// and the tool asks for `confirmed=true` before it happens.
pub async fn delete_event(
    client: &Client,
    connection_id: i64,
    calendar_id: &str,
    event_id: &str,
) -> Result<()> {
    let request = client
        .delete(&format!(
            "calendar/v3/calendars/{}/events/{}",
            urlencode(calendar_id),
            urlencode(event_id)
        ))
        .query(&[SEND_UPDATES]);
    client.drain(connection_id, request).await
}

fn rfc3339(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A calendar id is an email address and an event id can hold anything; both
/// go in the path.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireCalendarList {
    items: Vec<WireCalendar>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireCalendar {
    id: String,
    summary: String,
    description: Option<String>,
    primary: bool,
    access_role: Option<String>,
    time_zone: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireEventList {
    items: Vec<WireEvent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireEvent {
    id: String,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: When,
    end: When,
    status: Option<String>,
    html_link: Option<String>,
    organizer: Option<WirePerson>,
    updated: Option<DateTime<Utc>>,
    /// Read, never written: this is how a tool knows the event is a meeting.
    attendees: Vec<WirePerson>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WirePerson {
    email: Option<String>,
    display_name: Option<String>,
}

impl From<WireCalendar> for Calendar {
    fn from(w: WireCalendar) -> Self {
        Calendar {
            id: w.id,
            summary: w.summary,
            description: w.description,
            primary: w.primary,
            access_role: w.access_role,
            time_zone: w.time_zone,
        }
    }
}

impl From<WireEvent> for Event {
    fn from(w: WireEvent) -> Self {
        Event {
            id: w.id,
            summary: w.summary,
            description: w.description,
            location: w.location,
            start: w.start,
            end: w.end,
            status: w.status,
            html_link: w.html_link,
            organizer: w.organizer.and_then(|p| p.email.or(p.display_name)),
            updated: w.updated,
            attendee_count: w.attendees.len(),
        }
    }
}
