//! Docs: the end index, the append and the replacement.

use super::*;
use crate::google::docs;

#[tokio::test]
async fn a_document_reports_its_end_index_and_its_tabs() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        fixture("docs_document.json"),
    )
    .await;
    let document = docs::get(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
    )
    .await
    .unwrap();
    assert_eq!(document.end_index, 32);
    assert_eq!(document.append_index(), 31);
    assert_eq!(
        document
            .tabs
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["Summary", "Appendix"]
    );
    assert!(document.url().ends_with("/edit"));
    let request = h
        .last(
            "GET",
            "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        )
        .await;
    assert!(
        request
            .url
            .query()
            .unwrap()
            .contains("includeTabsContent=false")
    );
}

#[tokio::test]
async fn text_is_appended_at_the_end_of_the_document() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        fixture("docs_document.json"),
    )
    .await;
    h.mount_json(
        "POST",
        "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc:batchUpdate",
        fixture("docs_batch_update.json"),
    )
    .await;

    let index = docs::append_text(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        "\nOctober was quieter.\n",
    )
    .await
    .unwrap();
    assert_eq!(index, 31);
    let body = h
        .last_body(
            "POST",
            "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc:batchUpdate",
        )
        .await;
    assert_eq!(
        body,
        json!({"requests": [{"insertText": {
            "text": "\nOctober was quieter.\n",
            "location": {"index": 31}
        }}]})
    );

    let empty = docs::append_text(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        "",
    )
    .await
    .unwrap_err();
    assert!(matches!(empty, Error::Unsupported(_)), "{empty:?}");
}

#[tokio::test]
async fn a_replacement_reports_how_many_it_changed() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(
            "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc:batchUpdate",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_replace_reply.json")))
        .mount(&h.server)
        .await;

    let changed = docs::replace_all_text(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        "Q3",
        "Q4",
        true,
    )
    .await
    .unwrap();
    assert_eq!(changed, 3);
    let body = h
        .last_body(
            "POST",
            "/v1/documents/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc:batchUpdate",
        )
        .await;
    assert_eq!(
        body,
        json!({"requests": [{"replaceAllText": {
            "containsText": {"text": "Q3", "matchCase": true},
            "replaceText": "Q4"
        }}]})
    );

    let empty = docs::replace_all_text(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        "",
        "x",
        false,
    )
    .await
    .unwrap_err();
    assert!(matches!(empty, Error::Unsupported(_)), "{empty:?}");
}
