//! Writing to the log. Every portal action that changes something records a
//! row: who did it, to which connection or token, and how it ended. The log is
//! the answer to "why did the model draft that", and the page a person opens
//! when something fails in chat.
//!
//! Recording is best effort on purpose. A connection that was made and a log
//! row that could not be written is a worse outcome than a connection that was
//! made and a warning in the process log, so nothing here can fail a request.

use chrono::Utc;
use serde_json::Value;

use crate::db::{AuditKind, AuditOutcome, Db, NewAuditEntry, User};
use crate::domain::audit::strip_args;

/// The skeleton of a row: now, by this person, with everything else left to
/// the caller. `kind` and `outcome` are the two things every row has.
pub fn by(user: &User, kind: AuditKind, outcome: AuditOutcome) -> NewAuditEntry {
    NewAuditEntry {
        user_id: Some(user.id),
        ..NewAuditEntry::new(Utc::now(), kind, outcome)
    }
}

/// Arguments as the log stores them: content fields summarised, the whole
/// thing cut at 4 KB. The cut can land inside a value and leave text that is
/// no longer JSON, so what could not be parsed back is stored as the string it
/// became rather than thrown away.
pub fn args(value: Value) -> Option<Value> {
    let stripped = strip_args(value);
    Some(serde_json::from_str(&stripped).unwrap_or(Value::String(stripped)))
}

/// Write the row. A failure is logged and swallowed; see the module note.
pub async fn record(db: &Db, entry: NewAuditEntry) {
    let kind = entry.kind;
    if let Err(e) = db.insert_audit(entry).await {
        tracing::error!("could not record a {kind} audit row: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_row_carries_the_person_and_the_stripped_arguments() {
        let db = Db::open_memory().await.unwrap();
        let user = db
            .upsert_user("subject", Some("anna@example.test"), None)
            .await
            .unwrap();
        record(
            &db,
            NewAuditEntry {
                args: args(json!({"label": "work", "body": "the whole mail"})),
                detail: Some("work".into()),
                ..by(&user, AuditKind::Connect, AuditOutcome::Ok)
            },
        )
        .await;
        let rows = db.list_audit(Default::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, AuditKind::Connect);
        assert_eq!(rows[0].user_id, Some(user.id));
        let args = rows[0].args.clone().unwrap();
        assert_eq!(args["label"], "work");
        assert_eq!(args["body"], "<14 chars>");
    }

    #[tokio::test]
    async fn arguments_too_long_to_stay_json_are_kept_as_text() {
        let stored = args(json!({ "query": "x".repeat(10_000) })).unwrap();
        assert!(stored.is_string(), "{stored}");
        assert!(stored.as_str().unwrap().ends_with('…'));
    }
}
