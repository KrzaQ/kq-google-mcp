//! Calendar, and the two rules that are structural: nothing notifies
//! anybody, and nothing this server writes can name an attendee.

use super::*;
use crate::google::calendar;

#[tokio::test]
async fn calendars_and_events_are_listed_in_a_window() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/calendar/v3/users/me/calendarList",
        fixture("calendar_list.json"),
    )
    .await;
    let calendars = calendar::list_calendars(&h.client, CONNECTION)
        .await
        .unwrap();
    assert_eq!(calendars.len(), 2);
    assert!(calendars[0].primary);
    assert_eq!(calendars[1].access_role.as_deref(), Some("writer"));

    h.mount_json(
        "GET",
        "/calendar/v3/calendars/primary/events",
        fixture("calendar_events.json"),
    )
    .await;
    let events = calendar::list_events(
        &h.client,
        CONNECTION,
        calendar::PRIMARY,
        &calendar::EventQuery {
            time_min: Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-09T00:00:00Z")
                    .unwrap()
                    .into(),
            ),
            time_max: Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z")
                    .unwrap()
                    .into(),
            ),
            query: Some("phoenix".into()),
            max: Some(50),
        },
    )
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].summary.as_deref(), Some("Write the Q3 summary"));
    assert!(!events[0].has_attendees());
    // The second is a meeting, which is what the write tools refuse to touch.
    assert_eq!(events[1].attendee_count, 2);
    assert!(events[1].has_attendees());

    let query: std::collections::HashMap<_, _> = h
        .last("GET", "/calendar/v3/calendars/primary/events")
        .await
        .url
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(query["singleEvents"], "true");
    assert_eq!(query["orderBy"], "startTime");
    assert_eq!(query["timeMin"], "2026-09-09T00:00:00Z");
    assert_eq!(query["timeMax"], "2026-09-16T00:00:00Z");
    assert_eq!(query["q"], "phoenix");
    assert_eq!(query["maxResults"], "50");
}

#[tokio::test]
async fn every_event_mutation_notifies_nobody_and_names_no_attendees() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/calendar/v3/calendars/primary/events",
        fixture("calendar_event_created.json"),
    )
    .await;
    h.mount_json(
        "PATCH",
        "/calendar/v3/calendars/primary/events/9n8m7l6k5j4h3g2f1d0s",
        fixture("calendar_event_created.json"),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path(
            "/calendar/v3/calendars/primary/events/9n8m7l6k5j4h3g2f1d0s",
        ))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let draft = calendar::EventDraft {
        summary: Some("Draft the invoice run".into()),
        description: Some("Nobody is invited to this".into()),
        start: Some(calendar::When::at(
            chrono::DateTime::parse_from_rfc3339("2026-09-11T08:00:00Z")
                .unwrap()
                .into(),
        )),
        end: Some(calendar::When::at(
            chrono::DateTime::parse_from_rfc3339("2026-09-11T09:00:00Z")
                .unwrap()
                .into(),
        )),
        ..Default::default()
    };
    let created = calendar::insert_event(&h.client, CONNECTION, calendar::PRIMARY, &draft)
        .await
        .unwrap();
    assert_eq!(created.id, "9n8m7l6k5j4h3g2f1d0s");
    assert_eq!(created.attendee_count, 0);

    calendar::patch_event(
        &h.client,
        CONNECTION,
        calendar::PRIMARY,
        "9n8m7l6k5j4h3g2f1d0s",
        &calendar::EventDraft {
            location: Some("Desk".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    calendar::delete_event(
        &h.client,
        CONNECTION,
        calendar::PRIMARY,
        "9n8m7l6k5j4h3g2f1d0s",
    )
    .await
    .unwrap();

    for request in h.requests().await {
        if !request.url.path().starts_with("/calendar/") {
            continue;
        }
        if matches!(request.method.as_str(), "POST" | "PATCH" | "DELETE") {
            let query: std::collections::HashMap<_, _> =
                request.url.query_pairs().into_owned().collect();
            assert_eq!(
                query.get("sendUpdates").map(String::as_str),
                Some("none"),
                "{} {}",
                request.method,
                request.url
            );
            let body = String::from_utf8_lossy(&request.body);
            assert!(!body.contains("attendees"), "{body}");
        }
    }

    // A patch sends only what it names, so nothing else is overwritten.
    let patch = h
        .last_body(
            "PATCH",
            "/calendar/v3/calendars/primary/events/9n8m7l6k5j4h3g2f1d0s",
        )
        .await;
    assert_eq!(patch, json!({"location": "Desk"}));
}

#[test]
fn an_event_this_server_writes_cannot_carry_attendees() {
    // The type has no such field, so no caller can add one by mistake and no
    // future edit can smuggle one in without failing this.
    let draft = calendar::EventDraft {
        summary: Some("Solo work".into()),
        description: Some("attendees: nobody, deliberately".into()),
        location: Some("Desk".into()),
        start: Some(calendar::When::all_day("2026-09-11")),
        end: Some(calendar::When::all_day("2026-09-12")),
    };
    let json = serde_json::to_string(&draft).unwrap();
    assert!(json.contains("\"summary\":\"Solo work\""), "{json}");
    assert!(json.contains("\"date\":\"2026-09-11\""), "{json}");
    // The word only survives where the author of the description put it.
    assert_eq!(json.matches("attendees").count(), 1);
    assert!(!json.contains("\"attendees\""), "{json}");
    assert_eq!(
        serde_json::to_string(&calendar::EventDraft::default()).unwrap(),
        "{}"
    );
}
