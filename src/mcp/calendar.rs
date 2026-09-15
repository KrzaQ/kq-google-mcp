//! The Calendar tools. Reading is calendars and events in a window; writing
//! is one event created, changed or removed, always with `sendUpdates=none`
//! and always after `confirmed=true`.
//!
//! Two refusals are the point of this module. There is no `attendees`
//! argument anywhere — not optional, not empty, absent from the schema — so a
//! model cannot turn a draft agenda into an invitation; and an event that
//! already has attendees is read-only here, because patching or deleting it
//! would change something on other people's calendars.

use chrono::{Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::drive::{Moment, instant};
use super::dto::{self, Confirmable, PreviewOut};
use super::{Call, Gmcp, bad, capped, refuse};
use crate::domain::scope::Service;
use crate::google::calendar::{self, EventDraft, EventQuery, PRIMARY, When};

/// How far ahead `calendar_list_events` looks when no window is given.
const DEFAULT_DAYS: i64 = 7;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListEventsParam {
    /// The label of a connected account, as list_accounts reports it
    pub account: String,
    /// Start of the window; now by default. RFC 3339 with an offset, or a
    /// plain 2026-09-11T09:00 or 2026-09-11 on the person's own clock
    pub from: Option<String>,
    /// End of the window; seven days after `from` by default. Same shapes as
    /// `from`
    pub to: Option<String>,
    /// The calendar's id, as calendar_list reports it; the primary one by default
    pub calendar_id: Option<String>,
    /// Free-text search across the events in the window
    pub query: Option<String>,
    /// How many events to return; default 50, at most 250
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventParam {
    pub account: String,
    pub event_id: String,
    /// The calendar's id; the primary one by default
    pub calendar_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEventParam {
    pub account: String,
    /// What the event is called
    pub title: String,
    /// When it starts, on the person's own clock: 2026-09-11T15:00, or RFC
    /// 3339 with an offset to say exactly which instant. YYYY-MM-DD when
    /// all_day is true
    pub start: String,
    /// When it ends, in the same shapes as `start`. With all_day it is a date,
    /// and then it is the day *after* the last one, as Calendar counts it
    pub end: String,
    /// The calendar's id; the primary one by default
    pub calendar_id: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// True for a whole-day event, where start and end are dates
    pub all_day: Option<bool>,
    /// Must be true to write. Call with false first, show the person the
    /// title, times and calendar, and write only after they agree.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateEventParam {
    pub account: String,
    pub event_id: String,
    pub calendar_id: Option<String>,
    /// Only the fields given are changed; the rest keep their values
    pub title: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// True when the start and end given are dates rather than instants
    pub all_day: Option<bool>,
    /// Must be true to write, after the person has seen what changes.
    pub confirmed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteEventParam {
    pub account: String,
    pub event_id: String,
    pub calendar_id: Option<String>,
    /// Must be true to delete. Call with false first and show the person which
    /// event would go; this cannot be undone from here.
    pub confirmed: bool,
}

#[tool_router(router = calendar_router, vis = "pub(crate)")]
impl Gmcp {
    #[tool(
        description = "The calendars this account can see, with their ids, names and access. \
                       Pass one of these ids as calendar_id to the other calendar tools; leaving \
                       it out means the primary calendar."
    )]
    async fn calendar_list(
        &self,
        Parameters(p): Parameters<super::AccountParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::CalendarsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let calendars = calendar::list_calendars(&self.google()?.client, connection.id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::CalendarsOut {
            account: connection.label,
            calendars: calendars.into_iter().map(Into::into).collect(),
        }))
    }

    #[tool(
        description = "Events in a window, earliest first, with recurring ones expanded into \
                       their occurrences. Times come back on the person's own clock, with the \
                       offset in force that day, and a window given without an offset is read on \
                       that same clock; leaving the window out means the next seven days on the \
                       primary calendar."
    )]
    async fn calendar_list_events(
        &self,
        Parameters(p): Parameters<ListEventsParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::EventsOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let mut notes = Vec::new();
        let from = match p.from.as_deref() {
            Some(f) => noted(instant(f, call.tz)?, &mut notes),
            None => Utc::now(),
        };
        let to = match p.to.as_deref() {
            Some(t) => noted(instant(t, call.tz)?, &mut notes),
            None => from + Duration::days(DEFAULT_DAYS),
        };
        if to < from {
            return Err(bad("the window ends before it starts"));
        }
        let calendar_id = calendar_id(p.calendar_id.as_deref());
        let events = calendar::list_events(
            &self.google()?.client,
            connection.id,
            &calendar_id,
            &EventQuery {
                time_min: Some(from),
                time_max: Some(to),
                query: p.query,
                max: Some(capped(p.max, 50, 250)),
            },
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::EventsOut {
            account: connection.label,
            calendar_id,
            from: dto::at_zone(from, call.tz),
            to: dto::at_zone(to, call.tz),
            count: events.len(),
            events: events
                .into_iter()
                .map(|e| dto::EventOut::new(e, call.tz))
                .collect(),
            note: note(notes),
        }))
    }

    #[tool(description = "One event in full, including how many people are invited to it.")]
    async fn calendar_get_event(
        &self,
        Parameters(p): Parameters<EventParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<dto::EventOut>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let calendar_id = calendar_id(p.calendar_id.as_deref());
        let event = calendar::get_event(
            &self.google()?.client,
            connection.id,
            &calendar_id,
            p.event_id.trim(),
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(dto::EventOut::new(event, call.tz)))
    }

    #[tool(
        description = "Put an event on a calendar. Nobody is invited and nobody is notified: \
                       there is no attendees argument here on purpose, so an agenda cannot turn \
                       into an invitation by accident. Needs confirmed=true: call once with \
                       confirmed=false, show the person the title, times and calendar, and write \
                       only after they say yes."
    )]
    async fn calendar_create_event(
        &self,
        Parameters(p): Parameters<CreateEventParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::EventWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let calendar_id = calendar_id(p.calendar_id.as_deref());
        let title = p.title.trim().to_string();
        if title.is_empty() {
            return Err(bad("an event needs a title"));
        }
        let all_day = p.all_day.unwrap_or(false);
        let mut notes = Vec::new();
        let start = moment(&p.start, all_day, "start", call.tz, &mut notes)?;
        let end = moment(&p.end, all_day, "end", call.tz, &mut notes)?;
        if !p.confirmed {
            let mut details = vec![
                format!("title: {title}"),
                format!("from {} to {}", said(&start, &p.start), said(&end, &p.end)),
                format!("calendar: {calendar_id}"),
            ];
            details.extend(notes.iter().cloned());
            if all_day {
                details
                    .push("all day; Calendar treats the end date as the day after the last".into());
            }
            if let Some(l) = p.location.as_deref().filter(|l| !l.trim().is_empty()) {
                details.push(format!("location: {l}"));
            }
            if let Some(d) = p.description.as_deref().filter(|d| !d.trim().is_empty()) {
                details.push(format!("description: {d}"));
            }
            details.push("nobody is invited and nobody is notified".into());
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!("create the event {title:?} in `{}`", connection.label),
                details,
            ))));
        }
        let event = calendar::insert_event(
            &self.google()?.client,
            connection.id,
            &calendar_id,
            &EventDraft {
                summary: Some(title),
                description: clean(p.description),
                location: clean(p.location),
                start: Some(start),
                end: Some(end),
            },
        )
        .await
        .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(written(
            connection.label,
            calendar_id,
            "created",
            Some(event),
            call.tz,
            notes,
        ))))
    }

    #[tool(
        description = "Change an event's title, times, description or location; fields left out \
                       keep their values, and no attendee can be added because there is no such \
                       argument. An event that already has attendees is refused: changing it \
                       would move something on other people's calendars. Needs confirmed=true \
                       after the person has seen what changes."
    )]
    async fn calendar_update_event(
        &self,
        Parameters(p): Parameters<UpdateEventParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::EventWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let client = &self.google()?.client;
        let calendar_id = calendar_id(p.calendar_id.as_deref());
        let event_id = p.event_id.trim().to_string();
        let existing = calendar::get_event(client, connection.id, &calendar_id, &event_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        refuse_attendees(&existing, "change")?;
        let all_day = p.all_day.unwrap_or(existing.start.date.is_some());
        let mut notes = Vec::new();
        let draft = EventDraft {
            summary: clean(p.title.clone()),
            description: clean(p.description.clone()),
            location: clean(p.location.clone()),
            start: p
                .start
                .as_deref()
                .map(|s| moment(s, all_day, "start", call.tz, &mut notes))
                .transpose()?,
            end: p
                .end
                .as_deref()
                .map(|s| moment(s, all_day, "end", call.tz, &mut notes))
                .transpose()?,
        };
        if draft == EventDraft::default() {
            return Err(bad(
                "nothing to change: pass title, start, end, description or location",
            ));
        }
        if !p.confirmed {
            let mut details = vec![format!(
                "event: {} ({event_id})",
                existing
                    .summary
                    .clone()
                    .unwrap_or_else(|| "untitled".into())
            )];
            if let Some(t) = &draft.summary {
                details.push(format!("title becomes {t:?}"));
            }
            if let Some(s) = p.start.as_deref() {
                details.push(format!(
                    "start becomes {}",
                    said_opt(draft.start.as_ref(), s)
                ));
            }
            if let Some(e) = p.end.as_deref() {
                details.push(format!("end becomes {}", said_opt(draft.end.as_ref(), e)));
            }
            details.extend(notes.iter().cloned());
            if let Some(l) = &draft.location {
                details.push(format!("location becomes {l:?}"));
            }
            if draft.description.is_some() {
                details.push("the description is replaced".into());
            }
            details.push("nobody is notified".into());
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!("change an event in `{}`", connection.label),
                details,
            ))));
        }
        let event = calendar::patch_event(client, connection.id, &calendar_id, &event_id, &draft)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(written(
            connection.label,
            calendar_id,
            "changed",
            Some(event),
            call.tz,
            notes,
        ))))
    }

    #[tool(
        description = "Remove an event from a calendar. An event that has attendees is refused, \
                       because deleting it would take it off other people's calendars. Needs \
                       confirmed=true and cannot be undone from here."
    )]
    async fn calendar_delete_event(
        &self,
        Parameters(p): Parameters<DeleteEventParam>,
        Extension(call): Extension<Call>,
    ) -> Result<Json<Confirmable<dto::EventWriteOut>>, ErrorData> {
        let connection = self.account(&call, &p.account, Service::Calendar).await?;
        let client = &self.google()?.client;
        let calendar_id = calendar_id(p.calendar_id.as_deref());
        let event_id = p.event_id.trim().to_string();
        let existing = calendar::get_event(client, connection.id, &calendar_id, &event_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        refuse_attendees(&existing, "delete")?;
        if !p.confirmed {
            return Ok(Json(Confirmable::Preview(PreviewOut::new(
                format!(
                    "delete the event {:?} from `{}`",
                    existing
                        .summary
                        .clone()
                        .unwrap_or_else(|| "untitled".into()),
                    connection.label
                ),
                vec![
                    format!("event id: {event_id}"),
                    format!("calendar: {calendar_id}"),
                    "this cannot be undone from here".into(),
                ],
            ))));
        }
        calendar::delete_event(client, connection.id, &calendar_id, &event_id)
            .await
            .map_err(|e| self.google_err_for(&connection, e))?;
        Ok(Json(Confirmable::Done(written(
            connection.label,
            calendar_id,
            "deleted",
            None,
            call.tz,
            Vec::new(),
        ))))
    }
}

/// This release does not touch an event other people are on. The message says
/// why rather than only that, because the person can still do it in Calendar.
fn refuse_attendees(event: &calendar::Event, what: &str) -> Result<(), ErrorData> {
    if !event.has_attendees() {
        return Ok(());
    }
    Err(refuse(format!(
        "{:?} has {} attendees, and this server does not {what} events that other people are \
         invited to: it would change what is on their calendars. Ask the person to do it in \
         Google Calendar",
        event.summary.clone().unwrap_or_else(|| "this event".into()),
        event.attendee_count
    )))
}

fn calendar_id(given: Option<&str>) -> String {
    given
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(PRIMARY)
        .to_string()
}

/// A start or an end, as the two shapes Calendar has: an instant on the
/// person's clock, or a date for a whole-day event. Anything the reading of a
/// time had to decide is added to `notes`, which the reply carries back.
fn moment(
    value: &str,
    all_day: bool,
    field: &str,
    tz: Tz,
    notes: &mut Vec<String>,
) -> Result<When, ErrorData> {
    let value = value.trim();
    if all_day {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
            bad(format!(
                "{field} {value:?} is not a date; an all-day event takes YYYY-MM-DD"
            ))
        })?;
        return Ok(When::all_day(value));
    }
    Ok(When::at(noted(instant(value, tz)?, notes), tz))
}

/// The instant a time argument named, with whatever the reading had to decide
/// kept for the reply.
fn noted(moment: Moment, notes: &mut Vec<String>) -> chrono::DateTime<Utc> {
    if let Some(n) = moment.note {
        notes.push(n);
    }
    moment.at
}

fn note(notes: Vec<String>) -> Option<String> {
    (!notes.is_empty()).then(|| notes.join(" "))
}

/// What a preview calls a time: the instant the write will use, spelled out
/// with its offset, so the person confirms the time the server understood
/// rather than the string the model typed.
fn said(when: &When, given: &str) -> String {
    match &when.date_time {
        Some(at) => at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        None => given.trim().to_string(),
    }
}

fn said_opt(when: Option<&When>, given: &str) -> String {
    match when {
        Some(w) => said(w, given),
        None => given.trim().to_string(),
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn written(
    account: String,
    calendar_id: String,
    what: &str,
    event: Option<calendar::Event>,
    tz: Tz,
    mut notes: Vec<String>,
) -> dto::EventWriteOut {
    notes.push("nobody was invited and nobody was notified".into());
    dto::EventWriteOut {
        account,
        calendar_id,
        written: format!("the event was {what}"),
        event: event.map(|e| dto::EventOut::new(e, tz)),
        note: notes.join(" "),
    }
}
