//! Gmail: search, reading, labels, and the drafts that are the only
//! thing this server ever writes into a mailbox.

use super::*;
use crate::domain::limits::DOWNLOAD_MAX_BYTES;
use crate::google::gmail;

#[tokio::test]
async fn a_search_returns_rows_with_senders_subjects_and_attachment_counts() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/messages",
        fixture("gmail_messages_list.json"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/gmail/v1/users/me/messages/\w+$"))
        .and(query_param("format", "metadata"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_metadata.json")),
        )
        .mount(&h.server)
        .await;

    let rows = gmail::search(&h.client, CONNECTION, "has:attachment from:marta", 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    let first = &rows[0];
    assert_eq!(first.id, "18f0a1b2c3d4e5f6");
    assert_eq!(first.thread_id, "18f0a1b2c3d4e5f0");
    assert_eq!(
        first.from.as_deref(),
        Some("Marta Nowak <marta@example.test>")
    );
    assert_eq!(
        first.to,
        ["Anna Kowalska <anna@example.test>", "team@example.test"]
    );
    // The encoded word in the header is decoded for the model.
    assert_eq!(first.subject.as_deref(), Some("Q3 figures for Kraków"));
    assert_eq!(first.attachment_count, 1);
    assert_eq!(first.labels, ["INBOX", "IMPORTANT", "UNREAD"]);
    assert_eq!(
        first.date.unwrap().to_rfc3339(),
        "2026-09-04T13:33:20+00:00"
    );

    let request = h.last("GET", "/gmail/v1/users/me/messages").await;
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert_eq!(query["q"], "has:attachment from:marta");
    assert_eq!(query["maxResults"], "10");
}

#[tokio::test]
async fn a_message_is_read_with_its_text_attachments_and_inline_images() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .and(query_param("format", "full"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&h.server)
        .await;

    let message = gmail::get_message(&h.client, CONNECTION, "18f0a1b2c3d4e5f6")
        .await
        .unwrap();
    assert_eq!(message.subject.as_deref(), Some("Q3 figures for Kraków"));
    assert!(message.text.starts_with("Hi Anna,"), "{}", message.text);
    assert!(!message.text_from_html);
    assert_eq!(message.cc, ["team@example.test"]);

    // The three headers the plan names, kept as the reply tools need them.
    assert_eq!(
        message.message_id.as_deref(),
        Some("<CAF7n2sabc123@mail.example.test>")
    );
    assert_eq!(
        message.in_reply_to.as_deref(),
        Some("<20260901T090000.1@example.test>")
    );
    assert_eq!(
        message.references,
        [
            "<20260901T090000.0@example.test>",
            "<20260901T090000.1@example.test>"
        ]
    );

    // The PDF is an attachment with the id the attachments endpoint needs.
    assert_eq!(message.attachments.len(), 1);
    let pdf = &message.attachments[0];
    assert_eq!(pdf.id, "ANGjdJ8pdfQ3");
    assert_eq!(pdf.filename, "q3-figures.pdf");
    assert_eq!(pdf.mime_type, "application/pdf");
    assert_eq!(pdf.size, 26112);

    // The picture the HTML refers to is listed by its Content-ID, with the
    // angle brackets stripped, because `cid:` in the body has none.
    assert_eq!(message.inline_images.len(), 1);
    let chart = &message.inline_images[0];
    assert_eq!(chart.content_id, "chart-q3@example.test");
    assert_eq!(chart.attachment_id.as_deref(), Some("ANGjdJ8chartPNG"));
    assert_eq!(chart.mime_type, "image/png");
    assert_eq!(chart.filename, "chart.png");
    assert!(!message.text.contains("cid:"), "{}", message.text);
}

#[tokio::test]
async fn an_html_only_message_comes_back_as_readable_text() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_html_only.json")),
        )
        .mount(&h.server)
        .await;

    let message = gmail::get_message(&h.client, CONNECTION, "18f0a1b2c3d4e5f7")
        .await
        .unwrap();
    assert!(message.text_from_html);
    let text = &message.text;
    assert!(text.contains("Invoice 3391"), "{text}");
    assert!(text.contains("August 2026"), "{text}");
    // Entities are decoded and the markup, styles and scripts are gone.
    assert!(text.contains("Thank you & goodbye."), "{text}");
    assert!(!text.contains('<'), "{text}");
    assert!(!text.to_lowercase().contains("font-family"), "{text}");
    assert!(!text.contains("track("), "{text}");
    // No run of blank lines survives the conversion.
    assert!(!text.contains("\n\n\n"), "{text}");
}

#[tokio::test]
async fn a_thread_comes_back_in_order_and_can_be_trimmed() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/threads/18f0a1b2c3d4e5f0",
        fixture("gmail_thread.json"),
    )
    .await;

    let thread = gmail::get_thread(&h.client, CONNECTION, "18f0a1b2c3d4e5f0", None)
        .await
        .unwrap();
    assert_eq!(thread.id, "18f0a1b2c3d4e5f0");
    assert_eq!(thread.messages.len(), 2);
    assert_eq!(
        thread.messages[1].from.as_deref(),
        Some("Anna Kowalska <anna@example.test>")
    );

    // A long thread is cut from the front: what was said last is what matters.
    let trimmed = gmail::get_thread(&h.client, CONNECTION, "18f0a1b2c3d4e5f0", Some(1))
        .await
        .unwrap();
    assert_eq!(trimmed.messages.len(), 1);
    assert_eq!(trimmed.messages[0].id, "18f0a1b2c3d4e5fa");
}

#[tokio::test]
async fn labels_are_listed_with_their_counts() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/labels",
        fixture("gmail_labels.json"),
    )
    .await;
    let labels = gmail::list_labels(&h.client, CONNECTION).await.unwrap();
    assert_eq!(labels.len(), 5);
    assert_eq!(labels[0].id, "INBOX");
    assert_eq!(labels[0].kind.as_deref(), Some("system"));
    assert_eq!(labels[0].messages_unread, Some(12));
    assert_eq!(labels[3].name, "Invoices");
}

#[tokio::test]
async fn an_attachment_comes_back_as_bytes() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/attachments/ANGjdJ8pdfQ3",
        fixture("gmail_attachment.json"),
    )
    .await;
    let bytes = gmail::get_attachment(&h.client, CONNECTION, "18f0a1b2c3d4e5f6", "ANGjdJ8pdfQ3")
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes), "%PDF-1.7 not really a pdf");
}

/// Gmail hands attachments back base64 inside JSON, which the download cap
/// has to cover as well: without it a mail with a 200 MB attachment is read
/// whole into this process before anything looks at its size.
#[tokio::test]
async fn an_attachment_over_the_download_cap_is_refused_before_it_is_read() {
    let h = harness().await;
    let at = "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/attachments/ANGjdJ8pdfQ3";
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![b'x'; DOWNLOAD_MAX_BYTES as usize + 1])
                .insert_header("content-type", "application/json"),
        )
        .mount(&h.server)
        .await;

    let error = gmail::get_attachment(&h.client, CONNECTION, "18f0a1b2c3d4e5f6", "ANGjdJ8pdfQ3")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::TooLarge), "{error:?}");
    assert!(error.to_string().contains("50 MB"), "{error}");
}

#[tokio::test]
async fn labels_are_added_and_removed_but_never_trash_or_spam() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/modify",
        fixture("gmail_message_modified.json"),
    )
    .await;

    let message = gmail::modify_labels(
        &h.client,
        CONNECTION,
        "18f0a1b2c3d4e5f6",
        &["Label_18".to_string()],
        &["UNREAD".to_string(), "INBOX".to_string()],
    )
    .await
    .unwrap();
    assert_eq!(message.labels, ["IMPORTANT", "Label_18"]);
    let body = h
        .last_body(
            "POST",
            "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/modify",
        )
        .await;
    assert_eq!(body["addLabelIds"], json!(["Label_18"]));
    assert_eq!(body["removeLabelIds"], json!(["UNREAD", "INBOX"]));

    // The two labels this server refuses, however they are spelled, on
    // whichever side they are named, and before any request goes out. Taking a
    // message out of the bin is not a thing this server does either: it would
    // be an untrash, and the rule is that the bin is not touched at all.
    for (label, on_add) in [
        ("TRASH", true),
        ("spam", true),
        (" Trash ", true),
        ("TRASH", false),
        ("spam", false),
        (" Trash ", false),
    ] {
        let named = [label.to_string()];
        let (add, remove): (&[String], &[String]) =
            if on_add { (&named, &[]) } else { (&[], &named) };
        let error = gmail::modify_labels(&h.client, CONNECTION, "18f0a1b2c3d4e5f6", add, remove)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("never"), "{error}");
    }
    let modifies = h
        .requests()
        .await
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/modify"))
        .count();
    assert_eq!(modifies, 1);
}

/// Ids come from a model. `Url` would read `../` and `?` in one as
/// structure, so `message_id = "../drafts/send?"` would turn this modify POST
/// into a call on the drafts endpoint — the one thing this server never makes.
/// Encoded, the id is one path segment and the call lands where it was aimed;
/// the send guard every harness mounts sees nothing.
#[tokio::test]
async fn a_message_id_that_tries_to_climb_out_of_the_path_stays_in_it() {
    let h = harness().await;
    let encoded = "/gmail/v1/users/me/messages/..%2Fdrafts%2Fsend%3F/modify";
    h.mount_json("POST", encoded, fixture("gmail_message_modified.json"))
        .await;

    gmail::modify_labels(
        &h.client,
        CONNECTION,
        "../drafts/send?",
        &["Label_18".to_string()],
        &[],
    )
    .await
    .unwrap();

    let paths: Vec<String> = h
        .requests()
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(paths.iter().any(|p| p == encoded), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.ends_with("/drafts/send")),
        "{paths:?}"
    );
}

/// The RFC 2822 message inside a draft request, decoded.
fn draft_raw(body: &Value) -> String {
    use base64::Engine;
    let raw = body["message"]["raw"].as_str().expect("a raw message");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .expect("base64url");
    String::from_utf8(bytes).expect("the message is UTF-8")
}

#[tokio::test]
async fn a_reply_draft_carries_in_reply_to_references_and_the_thread_id() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_message_full.json")))
        .mount(&h.server)
        .await;
    h.mount_json(
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;

    let message = gmail::get_message(&h.client, CONNECTION, "18f0a1b2c3d4e5f6")
        .await
        .unwrap();
    let reply = gmail::DraftContent::reply_to(
        &message,
        "anna@example.test",
        "Thanks, I will read it tonight.",
        true,
    );
    let created = gmail::create_draft(&h.client, CONNECTION, &reply)
        .await
        .unwrap();
    assert_eq!(created.id, "r-8812345678901234567");
    assert_eq!(created.thread_id, "18f0a1b2c3d4e5f0");
    assert!(created.url().contains("r-8812345678901234567"));

    let body = h.last_body("POST", "/gmail/v1/users/me/drafts").await;
    // Gmail files the draft in the conversation it answers.
    assert_eq!(body["message"]["threadId"], "18f0a1b2c3d4e5f0");

    let mime = draft_raw(&body);
    assert!(
        mime.contains("In-Reply-To: <CAF7n2sabc123@mail.example.test>"),
        "{mime}"
    );
    // References is the original chain with the replied message appended.
    assert!(
        mime.contains("<20260901T090000.0@example.test>")
            && mime.contains("<20260901T090000.1@example.test>")
            && mime.contains("References:"),
        "{mime}"
    );
    let references = mime
        .lines()
        .find(|l| l.starts_with("References:"))
        .unwrap_or_default();
    assert!(
        references.ends_with("<CAF7n2sabc123@mail.example.test>")
            || mime.contains("\t<CAF7n2sabc123@mail.example.test>")
            || mime.contains(" <CAF7n2sabc123@mail.example.test>"),
        "{mime}"
    );
    // The reply goes to the sender, with everyone else copied, and one Re:.
    assert!(
        mime.contains("To: \"Marta Nowak\" <marta@example.test>"),
        "{mime}"
    );
    assert!(mime.contains("team@example.test"), "{mime}");
    assert!(mime.contains("Subject: Re: Q3 figures for"), "{mime}");
    assert!(!mime.contains("Re: Re:"), "{mime}");
}

#[tokio::test]
async fn a_draft_with_html_is_sent_as_alternatives() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_draft.json"),
    )
    .await;
    let content = gmail::DraftContent {
        from: "anna@example.test".into(),
        to: vec!["Marta Nowak <marta@example.test>".into()],
        cc: vec!["team@example.test".into()],
        subject: "Figures".into(),
        text: "Plain words.".into(),
        html: Some("<p>Rich <b>words</b>.</p>".into()),
        ..Default::default()
    };
    gmail::create_draft(&h.client, CONNECTION, &content)
        .await
        .unwrap();

    let mime = draft_raw(&h.last_body("POST", "/gmail/v1/users/me/drafts").await);
    assert!(mime.contains("multipart/alternative"), "{mime}");
    assert!(mime.contains("text/plain"), "{mime}");
    assert!(mime.contains("text/html"), "{mime}");
    assert!(mime.contains("Cc: team@example.test"), "{mime}");
    // No thread id on a fresh draft.
    let body = h.last_body("POST", "/gmail/v1/users/me/drafts").await;
    assert!(body["message"].get("threadId").is_none());
}

#[tokio::test]
async fn drafts_are_listed_updated_and_deleted_and_nothing_is_ever_sent() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/drafts",
        fixture("gmail_drafts_list.json"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/gmail/v1/users/me/messages/\w+$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_metadata.json")),
        )
        .mount(&h.server)
        .await;
    h.mount_json(
        "PUT",
        "/gmail/v1/users/me/drafts/r-8812345678901234567",
        fixture("gmail_draft.json"),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path("/gmail/v1/users/me/drafts/r-8812345678901234567"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let drafts = gmail::list_drafts(&h.client, CONNECTION, 20).await.unwrap();
    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].id, "r-8812345678901234567");
    assert_eq!(
        drafts[0].message.subject.as_deref(),
        Some("Q3 figures for Kraków")
    );

    let content = gmail::DraftContent {
        from: "anna@example.test".into(),
        to: vec!["marta@example.test".into()],
        subject: "Figures, corrected".into(),
        text: "Second thoughts.".into(),
        ..Default::default()
    };
    let updated = gmail::update_draft(&h.client, CONNECTION, "r-8812345678901234567", &content)
        .await
        .unwrap();
    assert_eq!(updated.id, "r-8812345678901234567");

    gmail::delete_draft(&h.client, CONNECTION, "r-8812345678901234567")
        .await
        .unwrap();

    // The guard mounted by every harness expects zero hits on a send path;
    // this asserts the same thing from the other side, over what was sent.
    for request in h.requests().await {
        assert!(
            !request.url.path().to_lowercase().contains("send"),
            "a request reached {}",
            request.url
        );
    }
}

#[tokio::test]
async fn the_send_as_list_says_which_addresses_a_draft_may_be_written_as() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/settings/sendAs",
        fixture("gmail_send_as.json"),
    )
    .await;

    let addresses = gmail::list_send_as(&h.client, CONNECTION).await.unwrap();
    assert_eq!(addresses.len(), 3);

    let primary = &addresses[0];
    assert_eq!(primary.email, "anna@example.test");
    assert!(primary.is_primary && primary.is_default);
    // The primary needs no verification and is always usable.
    assert_eq!(primary.verification_status, None);
    assert!(primary.usable());
    assert_eq!(primary.header(), "\"Anna Kowalska\" <anna@example.test>");

    let alias = &addresses[1];
    assert_eq!(alias.email, "sales@example.test");
    assert!(!alias.is_default);
    assert_eq!(alias.verification_status.as_deref(), Some("accepted"));
    assert_eq!(alias.reply_to.as_deref(), Some("sales@example.test"));
    assert!(alias.usable());
    assert_eq!(alias.header(), "\"Anna at Sales\" <sales@example.test>");

    // Gmail would rewrite a From Google has not verified, so this one is
    // listed and refused rather than written.
    let pending = &addresses[2];
    assert_eq!(pending.email, "old@example.test");
    assert_eq!(pending.verification_status.as_deref(), Some("pending"));
    assert!(!pending.usable());

    assert_eq!(
        gmail::default_send_as(&addresses).map(|a| a.email.as_str()),
        Some("anna@example.test")
    );
    // An argument finds its alias by address alone, whatever the case and
    // whether or not it carries a display name.
    for wanted in [
        "sales@example.test",
        "SALES@Example.Test",
        "Whoever <sales@example.test>",
    ] {
        assert_eq!(
            gmail::find_send_as(&addresses, wanted).map(|a| a.email.as_str()),
            Some("sales@example.test"),
            "{wanted}"
        );
    }
    assert!(gmail::find_send_as(&addresses, "nobody@example.test").is_none());
}

/// Drafting three replies in a row must not ask Gmail for the same settings
/// three times, so the list is cached per connection.
#[tokio::test]
async fn the_send_as_list_is_read_once_per_connection() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/settings/sendAs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("gmail_send_as.json")))
        .expect(1)
        .named("the send-as list is read once")
        .mount(&h.server)
        .await;

    for _ in 0..3 {
        let addresses = gmail::send_as(&h.client, CONNECTION).await.unwrap();
        assert_eq!(addresses.len(), 3);
    }
    // Disconnecting an account forgets what was held for it.
    h.client.forget(CONNECTION);
}

/// A reply is written as the address the mail arrived at, so the header that
/// says which one that was has to survive the parse.
#[tokio::test]
async fn a_message_carries_the_address_it_was_delivered_to() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/18f0a1b2c3d4e5f7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture("gmail_message_to_alias.json")),
        )
        .mount(&h.server)
        .await;

    let message = gmail::get_message(&h.client, CONNECTION, "18f0a1b2c3d4e5f7")
        .await
        .unwrap();
    assert_eq!(message.delivered_to, ["sales@example.test"]);
    assert_eq!(message.to, ["Anna at Sales <sales@example.test>"]);
}

#[test]
fn the_gmail_module_has_no_send_no_trash_and_no_message_delete() {
    let source = include_str!("../gmail.rs");
    let endpoints: Vec<&str> = source
        .lines()
        .filter(|line| line.contains("gmail/v1/"))
        .collect();
    assert!(endpoints.len() >= 10, "{endpoints:?}");
    for endpoint in &endpoints {
        // `send` as a path segment is Gmail's send: `messages/send` and
        // `drafts/send`, and with an upload it carries a query string
        // (`/send?uploadType=multipart`), so matching a trailing quote is not
        // enough. The only thing allowed to follow `/send` is `As`, which is
        // `settings/sendAs`, a read of which addresses the account may write
        // as. It sends nothing.
        let mut rest = *endpoint;
        while let Some(i) = rest.find("/send") {
            let after = &rest[i + "/send".len()..];
            assert!(after.starts_with("As"), "{endpoint}");
            rest = after;
        }
        assert!(!endpoint.contains("trash"), "{endpoint}");
        assert!(!endpoint.contains("batchDelete"), "{endpoint}");
        // The one delete in the whole server is the undo for a draft.
        if endpoint.contains("delete") || endpoint.contains("DELETE") {
            assert!(endpoint.contains("drafts"), "{endpoint}");
        }
    }
    // The draft endpoints are still there, with the id percent-encoded into
    // exactly one path segment rather than pasted in raw.
    assert!(source.contains("/drafts/{}"));
    assert!(source.contains("urlencode(draft_id)"));
}

#[test]
fn a_reply_keeps_one_re_and_drops_the_replying_account_from_the_recipients() {
    let message = gmail::Message {
        id: "m1".into(),
        thread_id: "t1".into(),
        date: None,
        from: Some("Marta Nowak <marta@example.test>".into()),
        to: vec![
            "Anna Kowalska <anna@example.test>".into(),
            "bob@example.test".into(),
        ],
        cc: vec!["team@example.test".into()],
        delivered_to: vec!["anna@example.test".into()],
        subject: Some("RE: budget".into()),
        snippet: None,
        labels: vec![],
        message_id: Some("<abc@example.test>".into()),
        in_reply_to: None,
        references: vec!["<older@example.test>".into()],
        text: String::new(),
        text_from_html: false,
        attachments: vec![],
        inline_images: vec![],
    };
    let reply = gmail::DraftContent::reply_to(&message, "anna@example.test", "yes", true);
    assert_eq!(reply.subject, "RE: budget");
    assert_eq!(reply.to, ["Marta Nowak <marta@example.test>"]);
    assert_eq!(reply.cc, ["bob@example.test", "team@example.test"]);
    assert_eq!(reply.in_reply_to.as_deref(), Some("<abc@example.test>"));
    assert_eq!(
        reply.references,
        ["<older@example.test>", "<abc@example.test>"]
    );
    assert_eq!(reply.thread_id.as_deref(), Some("t1"));

    let alone = gmail::DraftContent::reply_to(&message, "anna@example.test", "yes", false);
    assert!(alone.cc.is_empty());

    // A subject whose first three bytes are not three characters: the `Re:`
    // check must look at characters, or replying to a mail that opens with an
    // emoji panics inside a tool call.
    let subject_of = |subject: &str| {
        let mut m = message.clone();
        m.subject = Some(subject.to_string());
        gmail::DraftContent::reply_to(&m, "anna@example.test", "yes", false).subject
    };
    assert_eq!(subject_of("ab😀 report"), "Re: ab😀 report");
    assert_eq!(subject_of("Ре: тема"), "Re: Ре: тема");
    assert_eq!(subject_of("re: budget"), "re: budget");
    assert_eq!(subject_of(""), "Re:");
}

#[test]
fn a_reply_to_a_message_the_account_sent_goes_to_that_message_s_recipients() {
    let message = gmail::Message {
        id: "m2".into(),
        thread_id: "t2".into(),
        date: None,
        from: Some("Anna Kowalska <anna@example.test>".into()),
        to: vec![
            "Marta Nowak <marta@example.test>".into(),
            "anna@example.test".into(),
        ],
        cc: vec!["team@example.test".into()],
        delivered_to: vec![],
        subject: Some("budget".into()),
        snippet: None,
        labels: vec!["SENT".into()],
        message_id: Some("<sent@example.test>".into()),
        in_reply_to: None,
        references: vec![],
        text: String::new(),
        text_from_html: false,
        attachments: vec![],
        inline_images: vec![],
    };
    // The account wrote the original, so answering its From would write to the
    // person replying. The reply goes where the original went, and the copy
    // the account kept to itself stays a recipient.
    let reply =
        gmail::DraftContent::reply_to(&message, "anna@example.test", "one more thing", true);
    assert_eq!(
        reply.to,
        ["Marta Nowak <marta@example.test>", "anna@example.test"]
    );
    assert_eq!(reply.cc, ["team@example.test"]);
    assert_eq!(reply.subject, "Re: budget");
    assert_eq!(reply.in_reply_to.as_deref(), Some("<sent@example.test>"));
    assert_eq!(reply.references, ["<sent@example.test>"]);
    assert_eq!(reply.thread_id.as_deref(), Some("t2"));

    // Without reply_all the copies are left off, as on any other reply.
    let alone =
        gmail::DraftContent::reply_to(&message, "anna@example.test", "one more thing", false);
    assert_eq!(
        alone.to,
        ["Marta Nowak <marta@example.test>", "anna@example.test"]
    );
    assert!(alone.cc.is_empty());
}
