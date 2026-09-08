//! What `gmcp token create` leaves behind. The rules themselves are tested
//! in `domain::token`; these run the subcommand against an in-memory database
//! and look at the rows, because the allowlist and the author are what the
//! CLI decides on top of them.

use super::token::{Created, TokenCommand};
use crate::db::{ClientProfile, Connection, Db, NewConnection, User};
use crate::domain::seal;

const SECRET: &[u8] = b"a development secret of thirty-two bytes";

async fn user(db: &Db, subject: &str, email: &str) -> User {
    db.upsert_user(subject, Some(email), Some(subject))
        .await
        .unwrap()
}

async fn connection(db: &Db, owner: &User, label: &str) -> Connection {
    db.create_connection(NewConnection {
        user_id: owner.id,
        label: label.into(),
        google_email: format!("{}.{label}@example.test", owner.subject),
        services: vec!["gmail".into()],
        granted_scopes: vec![],
        refresh_token_sealed: seal::seal(SECRET, "1//a-refresh-token"),
        delegate_ok: label == "work",
    })
    .await
    .unwrap()
}

/// The arguments of `gmcp token create x --scopes … --client generic`, with
/// everything else left for the caller to fill in.
fn creating(name: &str, scopes: &[&str]) -> Created {
    Created {
        name: name.into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        client: "generic".into(),
        user: None,
        all_connections: false,
        connections: vec![],
        delegate: false,
    }
}

async fn create(db: &Db, args: Created) -> anyhow::Result<()> {
    super::token::run(
        db,
        Some("https://gmcp.example.test".into()),
        TokenCommand::Create {
            name: args.name,
            scopes: args.scopes,
            client: args.client,
            user: args.user,
            all_connections: args.all_connections,
            connections: args.connections,
            delegate: args.delegate,
        },
    )
    .await
}

#[tokio::test]
async fn a_personal_token_is_stored_with_the_labels_it_was_given() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    let work = connection(&db, &anna, "work").await;
    connection(&db, &anna, "personal").await;

    create(
        &db,
        Created {
            user: Some("anna@example.test".into()),
            connections: vec!["work".into()],
            client: "claude-code".into(),
            ..creating("claude", &["gmail:read", "gmail:draft"])
        },
    )
    .await
    .unwrap();

    let tokens = db.list_tokens(None).await.unwrap();
    assert_eq!(tokens.len(), 1);
    let t = &tokens[0];
    assert_eq!(t.name, "claude");
    assert_eq!(t.client, ClientProfile::ClaudeCode);
    // Canonical order, whatever order the flag listed them in.
    assert_eq!(t.scopes, ["gmail:read", "gmail:draft"]);
    assert_eq!(t.user_id, Some(anna.id));
    assert_eq!(t.created_by, anna.id);
    assert!(!t.all_connections);
    assert!(t.revoked_at.is_none());
    // The label became the allowlist, and the one not named stayed off it.
    assert_eq!(db.token_connections(t.id).await.unwrap(), [work.id]);

    // The secret is not in the row: only its hash is stored at all.
    assert_eq!(t.token_hash.len(), 64);
    // And the log says where the token came from.
    let log = db.list_audit(Default::default()).await.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].token_id, Some(t.id));
    assert_eq!(log[0].user_id, Some(anna.id));
    assert_eq!(log[0].kind.as_str(), "token_created");
}

#[tokio::test]
async fn all_connections_keeps_no_allowlist() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    connection(&db, &anna, "work").await;
    create(
        &db,
        Created {
            user: Some("anna@example.test".into()),
            all_connections: true,
            ..creating("everything", &["gmail:read"])
        },
    )
    .await
    .unwrap();
    let t = &db.list_tokens(None).await.unwrap()[0];
    assert!(t.all_connections);
    assert!(db.token_connections(t.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_delegate_token_belongs_to_nobody_and_is_authored_by_the_first_person() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    user(&db, "bob", "bob@example.test").await;

    // Nothing names a person, so the row's author is the first on record.
    create(
        &db,
        Created {
            delegate: true,
            client: "openwebui".into(),
            ..creating("gateway", &["gmail:read"])
        },
    )
    .await
    .unwrap();
    let t = &db.list_tokens(None).await.unwrap()[0];
    assert!(t.is_delegate());
    assert_eq!(t.user_id, None);
    assert_eq!(t.created_by, anna.id);
    assert_eq!(t.scopes, ["gmail:read", "delegate"]);
    assert!(!t.all_connections);
    assert!(db.token_connections(t.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn nothing_is_written_when_the_request_is_refused() {
    let db = Db::open_memory().await.unwrap();
    let anna = user(&db, "anna", "anna@example.test").await;
    connection(&db, &anna, "work").await;
    let bob = user(&db, "bob", "bob@example.test").await;
    connection(&db, &bob, "shared").await;

    let refused = |args| async {
        let error = create(&db, args).await.unwrap_err().to_string();
        assert!(db.list_tokens(None).await.unwrap().is_empty(), "{error}");
        error
    };

    // A label of somebody else's is not a label of theirs.
    let error = refused(Created {
        user: Some("anna@example.test".into()),
        connections: vec!["shared".into()],
        ..creating("x", &["gmail:read"])
    })
    .await;
    assert!(
        error.contains("no connection labelled \"shared\""),
        "{error}"
    );
    assert!(error.contains("theirs are work"), "{error}");

    // A person who has never logged in cannot be acted as.
    let error = refused(Created {
        user: Some("nobody@example.test".into()),
        all_connections: true,
        ..creating("x", &["gmail:read"])
    })
    .await;
    assert!(error.contains("has never logged in"), "{error}");

    // The shared rules, with the flag that fixes each one.
    let error = refused(Created {
        user: Some("anna@example.test".into()),
        delegate: true,
        ..creating("x", &["gmail:read"])
    })
    .await;
    assert!(error.contains("drop --user"), "{error}");

    let error = refused(Created {
        user: Some("anna@example.test".into()),
        ..creating("x", &["gmail:read"])
    })
    .await;
    assert!(error.contains("--all-connections"), "{error}");

    let error = refused(Created {
        user: Some("anna@example.test".into()),
        all_connections: true,
        ..creating("x", &["docs:write"])
    })
    .await;
    assert!(error.contains("docs:write is useless without docs:read"));

    let error = refused(Created {
        user: Some("anna@example.test".into()),
        all_connections: true,
        client: "emacs".into(),
        ..creating("x", &["gmail:read"])
    })
    .await;
    assert!(error.contains("emacs"), "{error}");
    assert!(error.contains("claude-code"), "the valid list: {error}");
}
