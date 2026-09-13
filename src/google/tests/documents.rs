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

// ----- the pictures a document holds -----------------------------------------

const IMAGES_DOC: &str = "1PiCtUrEsDoCiDeXaMpLe0123456789abcdefgh";
const IMAGES_AT: &str = "/v1/documents/1PiCtUrEsDoCiDeXaMpLe0123456789abcdefgh";

/// The document as Google answers it, with one picture pointed at `uri`.
fn images_document(uri: &str) -> Value {
    let mut document = fixture("docs_document_images.json");
    document["inlineObjects"]["kix.chart"]["inlineObjectProperties"]["embeddedObject"]["imageProperties"]
        ["contentUri"] = json!(uri);
    document
}

#[tokio::test]
async fn pictures_are_labelled_in_the_order_the_body_meets_them() {
    let h = harness().await;
    h.mount_json("GET", IMAGES_AT, fixture("docs_document_images.json"))
        .await;

    let held = docs::images(&h.client, CONNECTION, IMAGES_DOC)
        .await
        .unwrap();
    assert_eq!(held.title, "Five screenshots");
    // The fixture keys `inlineObjects` drawing, screenshot, chart — the
    // reverse of the body — because the map's order means nothing and the
    // labels have to mean what the markdown export means by them.
    assert_eq!(
        held.images
            .iter()
            .map(|i| (i.label.as_str(), i.object_id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("image1", "kix.chart"),
            ("image2", "kix.screenshot"),
            ("image3", "kix.drawing"),
        ],
        "image2 sits inside a table cell, so the walk goes through tables"
    );

    let chart = &held.images[0];
    assert_eq!(chart.alt_title.as_deref(), Some("Revenue"));
    assert_eq!(
        chart.alt_text.as_deref(),
        Some("Revenue by quarter, in thousands")
    );
    assert_eq!((chart.width_pt, chart.height_pt), (Some(320), Some(180)));
    assert!(
        chart
            .content_uri
            .as_deref()
            .unwrap()
            .starts_with("https://lh7-us.googleusercontent.com/"),
    );

    // A picture with no alt text says so rather than inventing one, and a
    // fraction of a point is rounded.
    let screenshot = &held.images[1];
    assert_eq!(screenshot.alt_title, None);
    assert_eq!(screenshot.alt_text, None);
    assert_eq!(screenshot.height_pt, Some(263));

    // A drawing has no picture of its own to fetch.
    assert_eq!(held.images[2].content_uri, None);
    assert_eq!(held.images[2].alt_title.as_deref(), Some("Sketch"));
}

#[tokio::test]
async fn a_picture_is_named_by_its_label_or_its_object_id_and_nothing_else() {
    let h = harness().await;
    h.mount_json("GET", IMAGES_AT, fixture("docs_document_images.json"))
        .await;
    let held = docs::images(&h.client, CONNECTION, IMAGES_DOC)
        .await
        .unwrap();

    assert_eq!(held.find("image2").unwrap().object_id, "kix.screenshot");
    assert_eq!(held.find(" IMAGE2 ").unwrap().object_id, "kix.screenshot");
    assert_eq!(held.find("kix.screenshot").unwrap().label, "image2");

    for wanted in ["image9", "kix.nothing", "2", ""] {
        let error = held.find(wanted).unwrap_err();
        assert!(
            matches!(error, Error::Unsupported(_)),
            "{wanted}: {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("Five screenshots"), "{message}");
        assert!(message.contains("image1, image2, image3"), "{message}");
    }
}

#[tokio::test]
async fn a_picture_comes_from_the_content_uri_the_document_just_gave() {
    let h = harness().await;
    let uri = format!("{}/docs-image/chart", h.server.uri());
    h.mount_json("GET", IMAGES_AT, images_document(&uri)).await;
    Mock::given(method("GET"))
        .and(path("/docs-image/chart"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"\x89PNG\r\n\x1a\nthe chart".to_vec())
                .insert_header("content-type", "image/png"),
        )
        .mount(&h.server)
        .await;

    let held = docs::images(&h.client, CONNECTION, IMAGES_DOC)
        .await
        .unwrap();
    let download = docs::open_image(&h.client, held.find("image1").unwrap())
        .await
        .unwrap();
    assert_eq!(download.mime_type(), Some("image/png"));
    assert_eq!(
        download.collect().await.unwrap(),
        b"\x89PNG\r\n\x1a\nthe chart"
    );

    // The picture's host is not the API host, and Google's URL is its own
    // capability, so no access token travels with the fetch.
    let request = h.last("GET", "/docs-image/chart").await;
    assert!(
        !request.headers.contains_key("authorization"),
        "{:?}",
        request.headers
    );
}

/// `contentUri` is an absolute URL out of a response body, and this is the one
/// place this server follows one. A body that names somewhere else is a body
/// that must be refused rather than fetched.
#[tokio::test]
async fn a_content_uri_that_is_not_googles_is_refused_and_never_fetched() {
    let h = harness().await;
    h.mount_json(
        "GET",
        IMAGES_AT,
        images_document("https://pictures.evil.example/collect?doc=1"),
    )
    .await;
    let held = docs::images(&h.client, CONNECTION, IMAGES_DOC)
        .await
        .unwrap();

    let Err(error) = docs::open_image(&h.client, held.find("image1").unwrap()).await else {
        panic!("a picture on somebody else's host was fetched");
    };
    assert!(matches!(error, Error::Untrusted(_)), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("pictures.evil.example"), "{message}");

    // A drawing is not a picture at all, and says which one it was.
    let Err(drawing) = docs::open_image(&h.client, held.find("image3").unwrap()).await else {
        panic!("a drawing answered bytes it does not have");
    };
    assert!(matches!(drawing, Error::Unsupported(_)), "{drawing:?}");
    assert!(drawing.to_string().contains("image3"), "{drawing}");

    // Nothing was fetched but the document itself.
    let paths: Vec<String> = h
        .requests()
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(paths, ["/token", IMAGES_AT]);
}
