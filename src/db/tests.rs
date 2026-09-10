//! Every test migrates a fresh in-memory database: no file, no environment,
//! no server. `Db::open_memory()` is the fake; SQLite is fast enough that the
//! real thing is cheaper than pretending.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use super::*;

/// The house zone every test user starts in, as `GMCP_TIMEZONE` gives it.
const HOUSE: Tz = chrono_tz::Europe::Warsaw;

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn sealed(s: &str) -> Vec<u8> {
    // Opaque bytes at this layer; step 2 owns what is actually in them.
    s.as_bytes().to_vec()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Two people, three connections, a personal token with an allowlist and a
/// delegate token: the smallest world every rule in the plan can be seen in.
struct World {
    db: Db,
    alice: User,
    bob: User,
    /// Alice's, reachable through the gateway.
    work: Connection,
    /// Alice's, not reachable through the gateway and not on the allowlist.
    personal: Connection,
    /// Bob's.
    bob_work: Connection,
    /// Alice's personal token, allowlisted to `work` only.
    token: ApiToken,
    /// Belongs to nobody, acts for whoever `X-Gmcp-User` names.
    delegate: ApiToken,
}

async fn world() -> World {
    let db = Db::open_memory().await.unwrap();
    let alice = db
        .upsert_user("sub-alice", Some("alice@example.com"), Some("Alice"), HOUSE)
        .await
        .unwrap();
    let bob = db
        .upsert_user("sub-bob", Some("bob@example.com"), Some("Bob"), HOUSE)
        .await
        .unwrap();

    let work = db
        .create_connection(NewConnection {
            user_id: alice.id,
            label: "work".into(),
            google_email: "alice@work.example".into(),
            services: strings(&["gmail", "drive"]),
            granted_scopes: strings(&["gmail.modify", "drive.readonly"]),
            refresh_token_sealed: sealed("alice-work"),
            delegate_ok: true,
        })
        .await
        .unwrap();
    let personal = db
        .create_connection(NewConnection {
            user_id: alice.id,
            label: "personal".into(),
            google_email: "alice@personal.example".into(),
            services: strings(&["gmail"]),
            granted_scopes: strings(&["gmail.modify"]),
            refresh_token_sealed: sealed("alice-personal"),
            delegate_ok: false,
        })
        .await
        .unwrap();
    let bob_work = db
        .create_connection(NewConnection {
            user_id: bob.id,
            label: "work".into(),
            google_email: "bob@work.example".into(),
            services: strings(&["calendar"]),
            granted_scopes: strings(&["calendar.events"]),
            refresh_token_sealed: sealed("bob-work"),
            delegate_ok: true,
        })
        .await
        .unwrap();

    let token = db
        .create_token(NewToken {
            name: "alice claude-code".into(),
            token_hash: "hash-personal".into(),
            scopes: strings(&["gmail:read", "drive:read"]),
            client: ClientProfile::ClaudeCode,
            user_id: Some(alice.id),
            all_connections: false,
            created_by: alice.id,
        })
        .await
        .unwrap();
    db.set_token_connections(token.id, &[work.id])
        .await
        .unwrap();
    let delegate = db
        .create_token(NewToken {
            name: "open webui".into(),
            token_hash: "hash-delegate".into(),
            scopes: strings(&["gmail:read", "delegate"]),
            client: ClientProfile::OpenWebUi,
            user_id: None,
            all_connections: false,
            created_by: alice.id,
        })
        .await
        .unwrap();

    World {
        db,
        alice,
        bob,
        work,
        personal,
        bob_work,
        token,
        delegate,
    }
}

fn labels(cs: &[Connection]) -> Vec<&str> {
    cs.iter().map(|c| c.label.as_str()).collect()
}

#[tokio::test]
async fn migrations_apply_to_a_fresh_in_memory_database() {
    let db = Db::open_memory().await.unwrap();
    let status = db.migration_status().await.unwrap();
    assert!(!status.is_empty());
    assert!(status.iter().all(|(_, _, applied)| *applied));
    // Idempotent: running them again changes nothing.
    db.migrate().await.unwrap();
}

#[tokio::test]
async fn instants_are_rfc_3339_text_that_sorts_chronologically() {
    let w = world().await;
    let db = &w.db;

    // Foreign keys are what make an allowlist entry follow its connection out.
    let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(on, 1);

    // Out of order on purpose, and one instant with a fraction of a second.
    let instants = [
        "2026-09-08T10:00:00Z",
        "2026-01-01T00:00:00Z",
        "2026-09-08T10:00:00.5Z",
        "2026-12-31T23:59:59Z",
    ];
    for s in instants {
        db.insert_audit(NewAuditEntry::new(
            utc(s),
            AuditKind::LinkUsed,
            AuditOutcome::Ok,
        ))
        .await
        .unwrap();
    }

    // The cursor compares the stored TEXT, so the format has to sort the way
    // the instants do, and has to survive a round trip unchanged.
    let stored: Vec<String> = sqlx::query_scalar("SELECT at FROM audit_log ORDER BY at")
        .fetch_all(db.pool())
        .await
        .unwrap();
    for s in &stored {
        let parsed = DateTime::parse_from_rfc3339(s).unwrap();
        assert_eq!(parsed.offset().local_minus_utc(), 0, "{s} is not UTC");
    }
    let mut expected = instants.map(utc);
    expected.sort();
    assert_eq!(stored.iter().map(|s| utc(s)).collect::<Vec<_>>(), expected);
}

#[tokio::test]
async fn users_are_keyed_by_subject_and_found_by_email() {
    let w = world().await;
    let db = &w.db;

    // A second login refreshes what the provider knows and keeps the rest.
    let again = db
        .upsert_user("sub-alice", None, None, HOUSE)
        .await
        .unwrap();
    assert_eq!(again.id, w.alice.id);
    assert_eq!(again.email.as_deref(), Some("alice@example.com"));
    assert_eq!(again.name.as_deref(), Some("Alice"));
    assert!(again.last_login_at.unwrap() >= w.alice.last_login_at.unwrap());

    assert_eq!(
        db.find_user_by_email("ALICE@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
        w.alice.id
    );
    assert!(
        db.find_user_by_email("nobody@example.com")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.list_users()
            .await
            .unwrap()
            .iter()
            .map(|u| u.id)
            .collect::<Vec<_>>(),
        [w.alice.id, w.bob.id]
    );

    db.touch_last_login(w.alice.id).await.unwrap();
    assert!(matches!(
        db.touch_last_login(9999).await,
        Err(DbError::NotFound)
    ));
    assert_eq!(db.get_user(w.alice.id).await.unwrap().display(), "Alice");
    assert!(matches!(db.get_user(9999).await, Err(DbError::NotFound)));
}

#[tokio::test]
async fn a_label_and_a_google_account_are_unique_per_person() {
    let w = world().await;
    let db = &w.db;

    // Same label, different Google account.
    let clash = db
        .create_connection(NewConnection {
            user_id: w.alice.id,
            label: "work".into(),
            google_email: "alice@other.example".into(),
            services: strings(&["gmail"]),
            granted_scopes: strings(&["gmail.modify"]),
            refresh_token_sealed: sealed("x"),
            delegate_ok: false,
        })
        .await;
    assert!(matches!(clash, Err(DbError::Conflict(_))), "{clash:?}");

    // Same Google account, different label: one grant per account per person.
    let clash = db
        .create_connection(NewConnection {
            user_id: w.alice.id,
            label: "work-again".into(),
            google_email: "alice@work.example".into(),
            services: strings(&["gmail"]),
            granted_scopes: strings(&["gmail.modify"]),
            refresh_token_sealed: sealed("x"),
            delegate_ok: false,
        })
        .await;
    assert!(matches!(clash, Err(DbError::Conflict(_))), "{clash:?}");

    // Bob may use both, they are only unique within a person.
    db.create_connection(NewConnection {
        user_id: w.bob.id,
        label: "personal".into(),
        google_email: "alice@work.example".into(),
        services: strings(&["gmail"]),
        granted_scopes: strings(&["gmail.modify"]),
        refresh_token_sealed: sealed("x"),
        delegate_ok: false,
    })
    .await
    .unwrap();

    // Renaming onto a taken label is the same conflict.
    let clash = db
        .update_connection(
            w.personal.id,
            ConnectionPatch {
                label: Some("work".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(clash, Err(DbError::Conflict(_))), "{clash:?}");

    // And case is not what tells two labels apart: every lookup folds it, so
    // "Work" would be a second row that answers to "work".
    let clash = db
        .create_connection(NewConnection {
            user_id: w.alice.id,
            label: "Work".into(),
            google_email: "alice@third.example".into(),
            services: strings(&["gmail"]),
            granted_scopes: strings(&["gmail.modify"]),
            refresh_token_sealed: sealed("x"),
            delegate_ok: false,
        })
        .await;
    assert!(matches!(clash, Err(DbError::Conflict(_))), "{clash:?}");
    let clash = db
        .update_connection(
            w.personal.id,
            ConnectionPatch {
                label: Some("WORK".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(clash, Err(DbError::Conflict(_))), "{clash:?}");
}

#[tokio::test]
async fn a_token_hash_is_unique_and_the_delegate_rule_is_enforced() {
    let w = world().await;
    let db = &w.db;

    let same_hash = db
        .create_token(NewToken {
            name: "copy".into(),
            token_hash: "hash-personal".into(),
            scopes: strings(&["gmail:read"]),
            client: ClientProfile::Generic,
            user_id: Some(w.bob.id),
            all_connections: true,
            created_by: w.bob.id,
        })
        .await;
    assert!(
        matches!(same_hash, Err(DbError::Conflict(_))),
        "{same_hash:?}"
    );

    // A delegate token belongs to nobody...
    let delegate_with_user = db
        .create_token(NewToken {
            name: "confused delegate".into(),
            token_hash: "hash-a".into(),
            scopes: strings(&["gmail:read", "delegate"]),
            client: ClientProfile::OpenWebUi,
            user_id: Some(w.alice.id),
            all_connections: false,
            created_by: w.alice.id,
        })
        .await;
    assert!(
        matches!(delegate_with_user, Err(DbError::Conflict(_))),
        "{delegate_with_user:?}"
    );

    // ...and every other token belongs to someone.
    let personal_without_user = db
        .create_token(NewToken {
            name: "orphan".into(),
            token_hash: "hash-b".into(),
            scopes: strings(&["gmail:read"]),
            client: ClientProfile::Generic,
            user_id: None,
            all_connections: false,
            created_by: w.alice.id,
        })
        .await;
    assert!(
        matches!(personal_without_user, Err(DbError::Conflict(_))),
        "{personal_without_user:?}"
    );
}

#[tokio::test]
async fn tokens_are_found_by_hash_until_they_are_revoked() {
    let w = world().await;
    let db = &w.db;

    let found = db
        .find_active_token("hash-personal")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, w.token.id);
    assert_eq!(found.client, ClientProfile::ClaudeCode);
    assert_eq!(found.scopes, strings(&["gmail:read", "drive:read"]));
    assert!(!found.is_delegate());
    assert!(w.delegate.is_delegate());
    assert!(found.last_used_at.is_none());

    db.touch_token_used(&found).await.unwrap();
    let used = db.get_token(found.id).await.unwrap();
    assert!(used.last_used_at.is_some());
    // Bumped at most once a minute, so a second call within one leaves it.
    db.touch_token_used(&used).await.unwrap();
    assert_eq!(
        db.get_token(found.id).await.unwrap().last_used_at,
        used.last_used_at
    );

    assert_eq!(
        db.list_tokens(Some(w.alice.id))
            .await
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [w.token.id]
    );
    assert_eq!(db.list_tokens(None).await.unwrap().len(), 2);
    assert_eq!(db.token_connections(w.token.id).await.unwrap(), [w.work.id]);

    db.revoke_token(w.token.id).await.unwrap();
    assert!(
        db.find_active_token("hash-personal")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        db.revoke_token(w.token.id).await,
        Err(DbError::NotFound)
    ));
}

#[tokio::test]
async fn each_principal_kind_sees_its_own_connections() {
    let w = world().await;
    let db = &w.db;

    // A session sees all of its own, in label order.
    let session = Reach::Session {
        user_id: w.alice.id,
    };
    assert_eq!(
        labels(&db.visible_connections(session).await.unwrap()),
        ["personal", "work"]
    );
    // ...and nobody else's.
    assert_eq!(
        labels(
            &db.visible_connections(Reach::Session { user_id: w.bob.id })
                .await
                .unwrap()
        ),
        ["work"]
    );

    // A personal token sees its allowlist.
    let by_token = Reach::of_token(&w.token, w.alice.id);
    assert_eq!(
        by_token,
        Reach::Token {
            user_id: w.alice.id,
            token_id: w.token.id,
            all_connections: false,
        }
    );
    assert_eq!(
        labels(&db.visible_connections(by_token).await.unwrap()),
        ["work"]
    );

    // ...and everything of its user once all_connections is set, including a
    // connection made after the token was.
    let all = db
        .create_token(NewToken {
            name: "all of alice".into(),
            token_hash: "hash-all".into(),
            scopes: strings(&["gmail:read"]),
            client: ClientProfile::Generic,
            user_id: Some(w.alice.id),
            all_connections: true,
            created_by: w.alice.id,
        })
        .await
        .unwrap();
    assert_eq!(
        labels(
            &db.visible_connections(Reach::of_token(&all, w.alice.id))
                .await
                .unwrap()
        ),
        ["personal", "work"]
    );

    // A delegate token reaches only what is flagged for the gateway, and only
    // for the person it acts as.
    let delegate = Reach::of_token(&w.delegate, w.alice.id);
    assert_eq!(
        delegate,
        Reach::Delegate {
            user_id: w.alice.id
        }
    );
    assert_eq!(
        labels(&db.visible_connections(delegate).await.unwrap()),
        ["work"]
    );
    assert_eq!(
        labels(
            &db.visible_connections(Reach::of_token(&w.delegate, w.bob.id))
                .await
                .unwrap()
        ),
        ["work"]
    );
    assert_eq!(
        db.visible_connections(Reach::of_token(&w.delegate, w.bob.id))
            .await
            .unwrap()[0]
            .id,
        w.bob_work.id
    );

    // The label a tool's `account` argument names resolves against that list.
    assert_eq!(
        db.find_visible_connection(by_token, "WORK")
            .await
            .unwrap()
            .unwrap()
            .id,
        w.work.id
    );
    assert!(
        db.find_visible_connection(by_token, "personal")
            .await
            .unwrap()
            .is_none()
    );

    // A connection that needs re-auth is still listed, with the flag; a
    // revoked one is gone from every principal's view.
    let personal = db
        .set_connection_status(
            w.personal.id,
            ConnectionStatus::NeedsReauth,
            Some("invalid_grant"),
        )
        .await
        .unwrap();
    assert_eq!(personal.status, ConnectionStatus::NeedsReauth);
    assert_eq!(personal.status_detail.as_deref(), Some("invalid_grant"));
    assert_eq!(
        labels(&db.visible_connections(session).await.unwrap()),
        ["personal", "work"]
    );

    db.set_connection_status(w.personal.id, ConnectionStatus::Revoked, None)
        .await
        .unwrap();
    assert_eq!(
        labels(&db.visible_connections(session).await.unwrap()),
        ["work"]
    );
    // ...but the portal still lists it, which is how it gets noticed.
    assert_eq!(
        labels(&db.list_connections(w.alice.id).await.unwrap()),
        ["personal", "work"]
    );
}

#[tokio::test]
async fn a_connection_is_renamed_reconnected_used_and_removed() {
    let w = world().await;
    let db = &w.db;

    let renamed = db
        .update_connection(
            w.work.id,
            ConnectionPatch {
                label: Some("  job  ".into()),
                delegate_ok: Some(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.label, "job");
    assert!(!renamed.delegate_ok);
    assert!(renamed.updated_at >= w.work.updated_at);
    assert!(matches!(
        db.update_connection(9999, ConnectionPatch::default()).await,
        Err(DbError::NotFound)
    ));

    // A reconnect replaces the sealed token and what Google granted, and
    // clears the health complaint.
    db.set_connection_status(w.work.id, ConnectionStatus::NeedsReauth, Some("expired"))
        .await
        .unwrap();
    let back = db
        .set_connection_grant(
            w.work.id,
            &strings(&["gmail", "drive", "calendar"]),
            &strings(&["gmail.modify", "drive.readonly", "calendar.events"]),
            &sealed("alice-work-2"),
        )
        .await
        .unwrap();
    assert_eq!(back.status, ConnectionStatus::Ok);
    assert!(back.status_detail.is_none());
    assert_eq!(back.services, strings(&["gmail", "drive", "calendar"]));
    assert_eq!(back.refresh_token_sealed, sealed("alice-work-2"));

    assert!(
        db.get_connection(w.work.id)
            .await
            .unwrap()
            .last_used_at
            .is_none()
    );
    db.touch_connection_used(w.work.id).await.unwrap();
    assert!(
        db.get_connection(w.work.id)
            .await
            .unwrap()
            .last_used_at
            .is_some()
    );

    assert_eq!(
        db.find_connection(w.alice.id, "JOB")
            .await
            .unwrap()
            .unwrap()
            .id,
        w.work.id
    );
    assert!(
        db.find_connection(w.alice.id, "work")
            .await
            .unwrap()
            .is_none()
    );

    // Removing it takes the allowlist entry with it and leaves the rest.
    db.delete_connection(w.work.id).await.unwrap();
    assert!(db.token_connections(w.token.id).await.unwrap().is_empty());
    assert!(matches!(
        db.get_connection(w.work.id).await,
        Err(DbError::NotFound)
    ));
    assert!(matches!(
        db.delete_connection(w.work.id).await,
        Err(DbError::NotFound)
    ));
    assert_eq!(
        labels(&db.list_connections(w.alice.id).await.unwrap()),
        ["personal"]
    );
}

#[tokio::test]
async fn a_link_is_spent_three_times_and_then_refused() {
    let w = world().await;
    let db = &w.db;
    let now = utc("2026-09-08T12:00:00Z");

    let link = db
        .create_link(NewLink {
            id: "aaaaaaaaaaaaaaaaaaaaaa".into(),
            connection_id: w.work.id,
            token_id: w.token.id,
            kind: LinkKind::GmailAttachment,
            target: serde_json::json!({"message_id": "m1", "attachment_id": "a1"}),
            filename: "photo.jpg".into(),
            mime_type: "image/jpeg".into(),
            size: Some(1234),
            expires_at: now + Duration::minutes(15),
            uses_left: 3,
        })
        .await
        .unwrap();
    assert_eq!(link.uses_left, 3);
    assert_eq!(link.kind, LinkKind::GmailAttachment);
    assert_eq!(link.target["attachment_id"], "a1");

    for left in [2, 1, 0] {
        let taken = db.take_link(&link.id, now).await.unwrap().unwrap();
        assert_eq!(taken.uses_left, left);
        assert_eq!(taken.filename, "photo.jpg");
    }
    let exhausted = db.take_link(&link.id, now).await.unwrap();
    assert_eq!(exhausted, Err(LinkRefusal::Exhausted));

    // A live link that the clock has passed.
    let stale = db
        .create_link(NewLink {
            id: "bbbbbbbbbbbbbbbbbbbbbb".into(),
            connection_id: w.work.id,
            token_id: w.token.id,
            kind: LinkKind::DriveExport,
            target: serde_json::json!({"file_id": "f1", "mime": "application/pdf"}),
            filename: "notes.pdf".into(),
            mime_type: "application/pdf".into(),
            size: None,
            expires_at: now - Duration::seconds(1),
            uses_left: 3,
        })
        .await
        .unwrap();
    let expired = db.take_link(&stale.id, now).await.unwrap();
    assert_eq!(expired, Err(LinkRefusal::Expired));
    // Refused, not spent.
    assert_eq!(
        db.take_link(&stale.id, now).await.unwrap(),
        Err(LinkRefusal::Expired)
    );

    let unknown = db.take_link("nonsense", now).await.unwrap();
    assert_eq!(unknown, Err(LinkRefusal::Unknown));

    // The route answers 404 to all three, but the log says which.
    assert_ne!(unknown, expired);
    assert_ne!(unknown, exhausted);
    assert_ne!(expired, exhausted);

    // A duplicate id is a conflict rather than a silently overwritten link.
    let dup = db
        .create_link(NewLink {
            id: link.id.clone(),
            connection_id: w.work.id,
            token_id: w.token.id,
            kind: LinkKind::DriveDownload,
            target: serde_json::json!({"file_id": "f2"}),
            filename: "other.bin".into(),
            mime_type: "application/octet-stream".into(),
            size: None,
            expires_at: now + Duration::minutes(15),
            uses_left: 3,
        })
        .await;
    assert!(matches!(dup, Err(DbError::Conflict(_))), "{dup:?}");
}

/// One audit row per minute, half of them Alice's.
async fn seed_audit(db: &Db, w: &World, count: i64) -> Vec<AuditEntry> {
    let base = utc("2026-09-08T10:00:00Z");
    let mut out = Vec::new();
    for i in 0..count {
        let mine = i % 2 == 0;
        out.push(
            db.insert_audit(NewAuditEntry {
                at: base + Duration::minutes(i),
                kind: AuditKind::ToolCall,
                user_id: Some(if mine { w.alice.id } else { w.bob.id }),
                token_id: Some(w.token.id),
                connection_id: Some(if mine { w.work.id } else { w.bob_work.id }),
                tool: Some(
                    if mine {
                        "gmail_search"
                    } else {
                        "calendar_list"
                    }
                    .into(),
                ),
                args: Some(serde_json::json!({"query": format!("call {i}")})),
                outcome: AuditOutcome::Ok,
                detail: None,
                duration_ms: Some(12),
                ip: None,
            })
            .await
            .unwrap(),
        );
    }
    out
}

#[tokio::test]
async fn the_log_pages_newest_first_across_a_boundary() {
    let w = world().await;
    let db = &w.db;
    let rows = seed_audit(db, &w, 12).await;

    // Two rows sharing an instant: the id breaks the tie, so a page boundary
    // between them must not skip or repeat one.
    let at = utc("2026-09-08T09:30:00Z");
    for detail in ["first", "second"] {
        db.insert_audit(NewAuditEntry {
            user_id: Some(w.alice.id),
            connection_id: Some(w.work.id),
            tool: Some("gmail_search".into()),
            detail: Some(detail.into()),
            ..NewAuditEntry::new(at, AuditKind::ToolCall, AuditOutcome::Ok)
        })
        .await
        .unwrap();
    }

    let mine = AuditFilter {
        user_id: Some(w.alice.id),
        limit: Some(3),
        ..Default::default()
    };
    let expected: Vec<i64> = {
        let mut ids: Vec<i64> = rows
            .iter()
            .filter(|r| r.user_id == Some(w.alice.id))
            .map(|r| r.id)
            .collect();
        // The two same-instant rows are the oldest, newest id first.
        let extra = db
            .list_audit(AuditFilter {
                user_id: Some(w.alice.id),
                tool: Some("gmail_search".into()),
                to: Some(at),
                ..Default::default()
            })
            .await
            .unwrap();
        ids.reverse();
        ids.extend(extra.iter().map(|r| r.id));
        ids
    };
    assert_eq!(expected.len(), 8);

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = db
            .list_audit(AuditFilter {
                before: cursor,
                ..mine.clone()
            })
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 3);
        assert!(page.iter().all(|r| r.user_id == Some(w.alice.id)));
        cursor = Some(page.last().unwrap().cursor());
        seen.extend(page.iter().map(|r| r.id));
    }
    assert_eq!(seen, expected, "pages must not skip or repeat a row");

    // A cursor survives the round trip through a query string.
    let c = rows[5].cursor();
    assert_eq!(c.to_string().parse::<AuditCursor>().unwrap(), c);

    // The other filters.
    let by_tool = db
        .list_audit(AuditFilter {
            tool: Some("calendar_list".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(by_tool.iter().all(|r| r.user_id == Some(w.bob.id)));
    assert_eq!(by_tool.len(), 6);

    let window = db
        .list_audit(AuditFilter {
            from: Some(utc("2026-09-08T10:03:00Z")),
            to: Some(utc("2026-09-08T10:05:00Z")),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(window.len(), 3);
    assert_eq!(window[0].at, utc("2026-09-08T10:05:00Z"));

    assert_eq!(
        db.list_audit(AuditFilter {
            connection_id: Some(w.bob_work.id),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        6
    );
    assert_eq!(
        db.list_audit(AuditFilter {
            kind: Some(AuditKind::LinkUsed),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        db.list_audit(AuditFilter {
            token_id: Some(w.token.id),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        12
    );

    // Arguments come back as the JSON they went in as.
    let newest = db.list_audit(AuditFilter::default()).await.unwrap();
    assert_eq!(newest[0].args.as_ref().unwrap()["query"], "call 11");
    assert_eq!(newest[0].outcome, AuditOutcome::Ok);
}

#[tokio::test]
async fn prune_removes_expired_links_and_old_log_rows_and_nothing_else() {
    let w = world().await;
    let db = &w.db;
    let now = utc("2026-09-08T12:00:00Z");

    for (id, expires_at) in [
        ("expired-one-aaaaaaaaaa", now - Duration::minutes(1)),
        ("expired-two-bbbbbbbbbb", now - Duration::days(2)),
        ("live-cccccccccccccccccc", now + Duration::minutes(15)),
    ] {
        db.create_link(NewLink {
            id: id.into(),
            connection_id: w.work.id,
            token_id: w.token.id,
            kind: LinkKind::DriveDownload,
            target: serde_json::json!({"file_id": "f1"}),
            filename: "f".into(),
            mime_type: "application/octet-stream".into(),
            size: None,
            expires_at,
            uses_left: 3,
        })
        .await
        .unwrap();
    }

    let old = db
        .insert_audit(NewAuditEntry::new(
            now - Duration::days(200),
            AuditKind::LinkUsed,
            AuditOutcome::Ok,
        ))
        .await
        .unwrap();
    let recent = db
        .insert_audit(NewAuditEntry::new(
            now - Duration::days(10),
            AuditKind::LinkUsed,
            AuditOutcome::Ok,
        ))
        .await
        .unwrap();

    let pruned = db.prune(now, Duration::days(180)).await.unwrap();
    assert_eq!(pruned, Pruned { links: 2, audit: 1 });

    assert_eq!(
        db.take_link("live-cccccccccccccccccc", now)
            .await
            .unwrap()
            .unwrap()
            .uses_left,
        2
    );
    assert_eq!(
        db.take_link("expired-one-aaaaaaaaaa", now).await.unwrap(),
        Err(LinkRefusal::Unknown)
    );

    let left = db.list_audit(AuditFilter::default()).await.unwrap();
    assert_eq!(left.iter().map(|r| r.id).collect::<Vec<_>>(), [recent.id]);
    assert!(!left.iter().any(|r| r.id == old.id));

    // Nothing else moved.
    assert_eq!(db.list_users().await.unwrap().len(), 2);
    assert_eq!(db.list_connections(w.alice.id).await.unwrap().len(), 2);
    assert_eq!(db.list_tokens(None).await.unwrap().len(), 2);

    // A second prune finds nothing to do.
    assert_eq!(
        db.prune(now, Duration::days(180)).await.unwrap(),
        Pruned::default()
    );
}

#[tokio::test]
async fn a_person_starts_in_the_house_zone_and_keeps_the_one_they_chose() {
    let w = world().await;
    let db = &w.db;
    assert_eq!(w.alice.timezone, "Europe/Warsaw");
    assert_eq!(w.alice.zone(HOUSE), HOUSE);

    let moved = db
        .set_user_timezone(w.alice.id, chrono_tz::America::New_York)
        .await
        .unwrap();
    assert_eq!(moved.timezone, "America/New_York");

    // Logging in again must not undo the choice, whatever the house zone is,
    // or every login would drag the person back to Warsaw.
    let again = db
        .upsert_user("sub-alice", None, None, chrono_tz::Europe::Lisbon)
        .await
        .unwrap();
    assert_eq!(again.timezone, "America/New_York");
    assert_eq!(again.zone(HOUSE), chrono_tz::America::New_York);

    // Somebody new gets whatever the house zone is at the time.
    let fresh = db
        .upsert_user("sub-carol", Some("carol@example.com"), None, chrono_tz::UTC)
        .await
        .unwrap();
    assert_eq!(fresh.timezone, "UTC");

    // A row whose zone the tz database no longer knows falls back rather than
    // failing every call that person makes.
    let stale = User {
        timezone: "Mars/Olympus".into(),
        ..fresh
    };
    assert_eq!(stale.zone(HOUSE), HOUSE);
}
