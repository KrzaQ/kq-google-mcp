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

// ----- the document as numbered paragraphs -----------------------------------

const ARTICLE: &str = "1ArTiClEdOcIdExAmPlE0123456789abcdef";
const ARTICLE_AT: &str = "/v1/documents/1ArTiClEdOcIdExAmPlE0123456789abcdef";
const ARTICLE_BATCH: &str = "/v1/documents/1ArTiClEdOcIdExAmPlE0123456789abcdef:batchUpdate";
/// What `docs_article.json` says the document is at.
const REVISION: &str = "ALm37BW0Article1";
/// The one paragraph every index in these tests is measured against. It holds
/// Polish letters, which are one UTF-16 unit and two UTF-8 bytes each, and an
/// emoji, which is two units and four bytes.
const POLISH: &str = "Zażółć gęślą jaźń 😀 już i już";

/// The article, read the way every write reads it.
async fn article(h: &Harness) -> docs::Outline {
    h.mount_json("GET", ARTICLE_AT, fixture("docs_article.json"))
        .await;
    docs::outline(&h.client, CONNECTION, ARTICLE).await.unwrap()
}

async fn mount_batch(h: &Harness) {
    h.mount_json("POST", ARTICLE_BATCH, fixture("docs_batch_update.json"))
        .await;
}

#[tokio::test]
async fn paragraphs_are_numbered_in_body_order_through_table_cells() {
    let h = harness().await;
    let outline = article(&h).await;

    assert_eq!(outline.revision_id, REVISION);
    assert_eq!(outline.title, "Wywiad z Anną");
    assert_eq!(outline.end_index, 87);
    assert_eq!(
        outline
            .paragraphs
            .iter()
            .map(|p| (
                p.ordinal,
                p.style.as_str(),
                p.text.as_str(),
                p.in_table,
                p.start_index,
                p.end_index
            ))
            .collect::<Vec<_>>(),
        [
            (1, "TITLE", "Wywiad z Anną", false, 1, 15),
            (2, "HEADING_2", "Część pierwsza", false, 15, 30),
            (3, "NORMAL_TEXT", POLISH, false, 30, 61),
            // The cell's paragraph is numbered where the body meets it:
            // after the paragraph before the table and before the one after.
            (4, "NORMAL_TEXT", "Komórka tabeli", true, 63, 78),
            (5, "NORMAL_TEXT", "Koniec.", false, 79, 87),
        ]
    );
    // The count is of characters, not of bytes and not of UTF-16 units.
    assert_eq!(outline.paragraph(3).unwrap().chars(), 29);

    for (ordinal, wanted) in [(6, "5 paragraphs"), (0, "numbered from 1")] {
        let error = outline.paragraph(ordinal).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains(wanted), "{error}");
    }
}

/// The arithmetic every index in this module rests on, in both directions and
/// over text that makes the three counts disagree.
#[test]
fn indexes_are_utf16_code_units_and_convert_both_ways() {
    use docs::index;

    let text = "Zażółć gęślą jaźń 😀 już";
    assert_eq!(text.chars().count(), 23);
    assert_eq!(text.len(), 36, "bytes");
    assert_eq!(index::len(text), 24, "UTF-16 code units");
    assert_eq!(index::len(""), 0);
    assert_eq!(index::len("😀😀"), 4);

    // `już` starts at character 20, byte 32 and code unit 21. A tool that
    // counted characters or bytes would edit the middle of a word.
    assert_eq!(text.char_indices().nth(20).unwrap().1, 'j');
    assert_eq!(index::from_chars(text, 20), Some(21));
    assert_eq!(index::from_bytes(text, 32), 21);
    assert_eq!(index::to_chars(text, 21), Some(20));

    // The ends of the text both ways, and nothing past them.
    assert_eq!(index::from_chars(text, 23), Some(24));
    assert_eq!(index::from_chars(text, 24), None);
    assert_eq!(index::to_chars(text, 24), Some(23));
    assert_eq!(index::to_chars(text, 25), None);

    // The emoji takes units 18 and 19, and 19 is half of a character.
    assert_eq!(index::from_chars(text, 18), Some(18));
    assert_eq!(index::to_chars(text, 18), Some(18));
    assert_eq!(index::to_chars(text, 19), None);
    assert_eq!(index::from_chars(text, 19), Some(20));

    // Every character offset goes out and comes back.
    for chars in 0..=text.chars().count() {
        let units = index::from_chars(text, chars).unwrap();
        assert_eq!(index::to_chars(text, units), Some(chars), "{chars}");
    }
}

#[tokio::test]
async fn an_edit_lands_on_the_utf16_index_of_the_occurrence_it_was_given() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    let plan = docs::plan_edit(&outline, REVISION, 3, "już", 2, "jutro", "Zażółć").unwrap();
    assert_eq!(plan.paragraph, 3);
    assert_eq!(plan.before, POLISH);
    assert_eq!(plan.after, "Zażółć gęślą jaźń 😀 już i jutro");
    docs::apply(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();

    // The second `już` starts at character 26 of the paragraph, which is unit
    // 27, which is index 57 in the document. The deletion names the old word
    // where the insertion before it has just pushed it: 57 + 5 for `jutro`.
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await,
        json!({
            "requests": [
                {"insertText": {"text": "jutro", "location": {"index": 57}}},
                {"deleteContentRange": {"range": {"startIndex": 62, "endIndex": 65}}}
            ],
            "writeControl": {"requiredRevisionId": REVISION}
        })
    );

    // The first occurrence is a different place in the same paragraph, and
    // the paragraph's two text runs are stitched into one string to find it.
    let first = docs::plan_edit(&outline, REVISION, 3, "już", 1, "jutro", "Zażółć").unwrap();
    assert_eq!(first.after, "Zażółć gęślą jaźń 😀 jutro i już");
    docs::apply(&h.client, CONNECTION, ARTICLE, first)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([
            {"insertText": {"text": "jutro", "location": {"index": 51}}},
            {"deleteContentRange": {"range": {"startIndex": 56, "endIndex": 59}}}
        ])
    );

    // An empty replacement is a deletion and nothing else.
    let cut = docs::plan_edit(&outline, REVISION, 3, " i już", 1, "", "Zażółć").unwrap();
    assert_eq!(cut.after, "Zażółć gęślą jaźń 😀 już");
    docs::apply(&h.client, CONNECTION, ARTICLE, cut)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([{"deleteContentRange": {"range": {"startIndex": 54, "endIndex": 60}}}])
    );

    // A third occurrence of a word the paragraph holds twice is not written.
    let error = docs::plan_edit(&outline, REVISION, 3, "już", 3, "jutro", "Zażółć").unwrap_err();
    assert!(error.to_string().contains("holds 2"), "{error}");
}

#[tokio::test]
async fn a_stale_revision_id_is_refused_and_nothing_is_sent() {
    let h = harness().await;
    let outline = article(&h).await;
    // Every write is guarded, and the guard fires before a request is built.
    let refusals = [
        docs::plan_edit(&outline, "ALm37BW0Older", 3, "już", 1, "jutro", "Zażółć").unwrap_err(),
        docs::plan_style(&outline, "ALm37BW0Older", 2, "HEADING_3", "Część").unwrap_err(),
        docs::plan_insert(
            &outline,
            "ALm37BW0Older",
            docs::At::After(2),
            "Nowy akapit",
            None,
            None,
        )
        .unwrap_err(),
        docs::plan_code(
            &outline,
            "ALm37BW0Older",
            2,
            "let x = 1;",
            docs::CodeStyle::default(),
            &[],
            None,
        )
        .unwrap_err(),
        // A write with no revision id at all is the same refusal.
        docs::plan_style(&outline, "  ", 2, "HEADING_3", "Część").unwrap_err(),
    ];
    for error in &refusals {
        assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("docs_list_paragraphs"),
            "{error}"
        );
    }
    assert!(
        refusals[0].to_string().contains("ALm37BW0Older"),
        "{}",
        refusals[0]
    );
    assert!(
        refusals[0].to_string().contains(REVISION),
        "{}",
        refusals[0]
    );

    // The document was read and nothing else happened.
    let posts: Vec<String> = h
        .requests()
        .await
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() != "/token")
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(posts.is_empty(), "{posts:?}");
}

/// The document may also move between the read and the write, which is the
/// gap `writeControl` closes. Google's refusal is answered in the same words
/// as the guard above, and never retried.
#[tokio::test]
async fn google_refusing_the_revision_asks_for_a_fresh_read_and_does_not_retry() {
    let h = harness().await;
    let outline = article(&h).await;
    Mock::given(method("POST"))
        .and(path(ARTICLE_BATCH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": 400,
                "message": "Revision ID does not match the document's current revision ID.",
                "status": "FAILED_PRECONDITION"
            }
        })))
        .expect(1)
        .named("one attempt, and no retry of a write")
        .mount(&h.server)
        .await;

    let plan = docs::plan_style(&outline, REVISION, 2, "HEADING_3", "Część").unwrap();
    let error = docs::apply(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("call docs_list_paragraphs again"),
        "{error}"
    );
}

#[tokio::test]
async fn an_expect_that_does_not_match_writes_nothing_and_says_what_it_found() {
    let h = harness().await;
    let outline = article(&h).await;

    let error = docs::plan_edit(&outline, REVISION, 3, "już", 1, "jutro", "Koniec").unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("paragraph 3"), "{message}");
    assert!(message.contains("\"Koniec\""), "{message}");
    assert!(message.contains("Zażółć gęślą"), "{message}");

    // The same lock on a restyle, and an empty `expect` is no lock at all.
    assert!(docs::plan_style(&outline, REVISION, 2, "TITLE", "Koniec").is_err());
    let empty = docs::plan_style(&outline, REVISION, 2, "TITLE", " ").unwrap_err();
    assert!(empty.to_string().contains("`expect` is empty"), "{empty}");

    // What it does match is the paragraph's own first words.
    assert!(docs::plan_style(&outline, REVISION, 2, "TITLE", "Część pierwsza").is_ok());

    let posts = h
        .requests()
        .await
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() != "/token")
        .count();
    assert_eq!(posts, 0);
}

#[tokio::test]
async fn a_restyle_writes_the_named_style_and_nothing_else() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    let plan = docs::plan_style(&outline, REVISION, 2, "heading 3", "Część").unwrap();
    assert_eq!(
        (plan.before.as_str(), plan.after.as_str()),
        ("HEADING_2", "HEADING_3")
    );
    docs::apply(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await,
        json!({
            "requests": [{"updateParagraphStyle": {
                "range": {"startIndex": 15, "endIndex": 30},
                "paragraphStyle": {"namedStyleType": "HEADING_3"},
                "fields": "namedStyleType"
            }}],
            "writeControl": {"requiredRevisionId": REVISION}
        })
    );

    // The last paragraph's range stops one short of the body's final newline,
    // which is Docs' own and may not be written over.
    let last = docs::plan_style(&outline, REVISION, 5, "SUBTITLE", "Koniec").unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, last)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"][0]["updateParagraphStyle"]["range"],
        json!({"startIndex": 79, "endIndex": 86})
    );

    // A style Docs does not have is refused with the ones it does.
    let error = docs::plan_style(&outline, REVISION, 2, "BODY_TEXT", "Część").unwrap_err();
    assert!(error.to_string().contains("NORMAL_TEXT"), "{error}");
    assert!(error.to_string().contains("HEADING_6"), "{error}");
}

#[tokio::test]
async fn an_insert_carries_its_own_paragraph_break() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    // Before a paragraph: the text goes in at that paragraph's own index and
    // ends with the break, so what was there stays a paragraph of its own.
    let before = docs::plan_insert(
        &outline,
        REVISION,
        docs::At::Before(1),
        "Lead\n",
        Some("SUBTITLE"),
        Some("Wywiad"),
    )
    .unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, before)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([
            {"insertText": {"text": "Lead\n", "location": {"index": 1}}},
            {"updateParagraphStyle": {
                "range": {"startIndex": 1, "endIndex": 5},
                "paragraphStyle": {"namedStyleType": "SUBTITLE"},
                "fields": "namedStyleType"
            }}
        ])
    );

    // After a paragraph in the middle: at the index the next paragraph starts.
    let middle =
        docs::plan_insert(&outline, REVISION, docs::At::After(2), "Akapit", None, None).unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, middle)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([{"insertText": {"text": "Akapit\n", "location": {"index": 30}}}])
    );

    // After the last paragraph the break comes first, because nothing may be
    // written after the body's final newline.
    let last = docs::plan_insert(
        &outline,
        REVISION,
        docs::At::After(5),
        "Nowy akapit",
        Some("HEADING_2"),
        None,
    )
    .unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, last)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([
            {"insertText": {"text": "\nNowy akapit", "location": {"index": 86}}},
            {"updateParagraphStyle": {
                "range": {"startIndex": 87, "endIndex": 98},
                "paragraphStyle": {"namedStyleType": "HEADING_2"},
                "fields": "namedStyleType"
            }}
        ])
    );

    // Nothing to insert, and a paragraph that is not there.
    assert!(docs::plan_insert(&outline, REVISION, docs::At::After(2), "\n\n", None, None).is_err());
    assert!(
        docs::plan_insert(&outline, REVISION, docs::At::After(9), "Akapit", None, None).is_err()
    );
}

// ----- a code listing --------------------------------------------------------

/// Twelve characters and thirteen UTF-16 units: the emoji is the difference,
/// and the span over the string literal is where that shows.
const CODE: &str = "let ż = \"😀\";";

fn span(start: usize, end: usize) -> docs::Span {
    docs::Span {
        start,
        end,
        colour: Some("#ff0000".into()),
        bold: None,
        italic: None,
    }
}

#[tokio::test]
async fn a_code_listing_is_written_and_coloured_in_one_batch() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    let spans = vec![
        docs::Span {
            start: 0,
            end: 3,
            colour: Some("#ff0000".into()),
            bold: Some(true),
            italic: None,
        },
        docs::Span {
            start: 8,
            end: 11,
            colour: Some("#00ff00".into()),
            bold: None,
            italic: Some(true),
        },
    ];
    let plan =
        docs::plan_code(&outline, REVISION, 2, CODE, pt(8.0), &spans, Some("Część")).unwrap();
    assert_eq!(plan.requests(), 4, "the text, the font and one per span");
    docs::apply(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();

    // The listing goes in at 30, so it covers units 30 to 43. The second span
    // is characters 8 to 11 of the code — the quoted emoji — which is units 8
    // to 12, because the emoji is two of them.
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await,
        json!({
            "requests": [
                {"insertText": {"text": "let ż = \"😀\";\n", "location": {"index": 30}}},
                {"updateTextStyle": {
                    "range": {"startIndex": 30, "endIndex": 43},
                    "textStyle": {
                        "weightedFontFamily": {"fontFamily": "Courier New"},
                        "fontSize": {"magnitude": 8.0, "unit": "PT"}
                    },
                    "fields": "weightedFontFamily,fontSize"
                }},
                {"updateTextStyle": {
                    "range": {"startIndex": 30, "endIndex": 33},
                    "textStyle": {
                        "foregroundColor": {"color": {"rgbColor": {"red": 1.0, "green": 0.0, "blue": 0.0}}},
                        "bold": true
                    },
                    "fields": "foregroundColor,bold"
                }},
                {"updateTextStyle": {
                    "range": {"startIndex": 38, "endIndex": 42},
                    "textStyle": {
                        "foregroundColor": {"color": {"rgbColor": {"red": 0.0, "green": 1.0, "blue": 0.0}}},
                        "italic": true
                    },
                    "fields": "foregroundColor,italic"
                }}
            ],
            "writeControl": {"requiredRevisionId": REVISION}
        })
    );
    // One batch for the whole listing, and no second call to finish it off.
    let batches = h
        .requests()
        .await
        .iter()
        .filter(|r| r.url.path() == ARTICLE_BATCH)
        .count();
    assert_eq!(batches, 1);
}

#[tokio::test]
async fn a_listing_that_cannot_be_coloured_completely_is_refused() {
    let h = harness().await;
    let outline = article(&h).await;
    let plan = |spans: Vec<docs::Span>| {
        docs::plan_code(
            &outline,
            REVISION,
            2,
            CODE,
            docs::CodeStyle::default(),
            &spans,
            None,
        )
        .unwrap_err()
    };

    let overlap = plan(vec![span(0, 5), span(3, 8)]);
    assert!(overlap.to_string().contains("overlap"), "{overlap}");
    // Overlapping the other way round is the same refusal.
    assert!(
        plan(vec![span(3, 8), span(0, 5)])
            .to_string()
            .contains("overlap")
    );

    let past = plan(vec![span(8, 13)]);
    assert!(past.to_string().contains("past the end"), "{past}");
    assert!(past.to_string().contains("12 characters"), "{past}");

    let empty = plan(vec![span(4, 4)]);
    assert!(empty.to_string().contains("empty"), "{empty}");

    let colour = plan(vec![docs::Span {
        colour: Some("red".into()),
        ..span(0, 3)
    }]);
    assert!(colour.to_string().contains("#rrggbb"), "{colour}");
    for bad in ["#ff000", "#gggggg", "ff0000", "#ff0000ff", ""] {
        let error = plan(vec![docs::Span {
            colour: Some(bad.into()),
            ..span(0, 3)
        }]);
        assert!(error.to_string().contains("not a colour"), "{bad}: {error}");
    }

    let nothing = plan(vec![docs::Span {
        colour: None,
        ..span(0, 3)
    }]);
    assert!(nothing.to_string().contains("says nothing"), "{nothing}");

    // Every one of them refused before a request was built.
    assert_eq!(
        h.requests()
            .await
            .iter()
            .filter(|r| r.url.path() == ARTICLE_BATCH)
            .count(),
        0
    );

    // Spans touching end to end are not overlapping, and a listing with no
    // spans at all is just monospace text.
    assert!(
        docs::plan_code(
            &outline,
            REVISION,
            2,
            CODE,
            docs::CodeStyle::default(),
            &[span(0, 3), span(3, 8)],
            None
        )
        .is_ok()
    );
    // A size Docs would render but no page could hold is refused here, and
    // so is one that is not a number at all.
    for bad in [0.0, 400.5, f64::NAN] {
        assert!(
            docs::plan_code(&outline, REVISION, 2, CODE, pt(bad), &[], None).is_err(),
            "{bad} was accepted"
        );
    }
    let plain = docs::plan_code(
        &outline,
        REVISION,
        2,
        CODE,
        docs::CodeStyle {
            font: Some("Roboto Mono"),
            size_pt: Some(8.0),
        },
        &[],
        None,
    )
    .unwrap();
    assert_eq!(plain.requests(), 2);
}

/// A listing set at a size and left to the default font.
fn pt(size: f64) -> docs::CodeStyle<'static> {
    docs::CodeStyle {
        font: None,
        size_pt: Some(size),
    }
}

// ----- reading the formatting back -------------------------------------------

const LISTING: &str = "1LiStInGdOcIdExAmPlE0123456789abcdef";
const LISTING_AT: &str = "/v1/documents/1LiStInGdOcIdExAmPlE0123456789abcdef";

/// Character offsets out, UTF-16 indexes back in. This is the whole contract
/// of the read: what comes out of a run can be written back as a span, and
/// the text it is measured over is Polish with an emoji in it, where the
/// three counts all disagree.
#[tokio::test]
async fn runs_are_reported_in_the_characters_a_span_is_written_in() {
    let h = harness().await;
    let outline = article(&h).await;
    let paragraph = outline.paragraph(3).unwrap();
    let runs = paragraph.formatting();

    assert_eq!(
        runs.iter()
            .map(|r| (r.start, r.end, r.text.as_str()))
            .collect::<Vec<_>>(),
        [(0, 20, "Zażółć gęślą jaźń 😀 "), (20, 29, "już i już")]
    );
    // Only what the document sets. The first run says nothing at all, and the
    // second says bold and nothing else — not that it is black, not that it
    // is upright.
    assert_eq!(runs[0].style, docs::RunStyle::default());
    assert_eq!(runs[1].style.bold, Some(true));
    assert_eq!(runs[1].style.colour, None);
    assert_eq!(runs[1].style.font, None);
    // The offsets convert back to the indexes Docs itself gave the runs,
    // which is what makes them usable as a span.
    for (run, index) in runs.iter().zip([30, 51]) {
        assert_eq!(
            paragraph.start_index + docs::index::from_chars(&paragraph.text, run.start).unwrap(),
            index,
            "{:?}",
            run.text
        );
    }
    // The newline that ends the paragraph is not a character of it.
    assert_eq!(runs.last().unwrap().end, paragraph.chars());
}

/// A paragraph nobody styled is one bare run, and a listing comes back with
/// the colours it was written with — in fewer runs than the spans that wrote
/// it, because Docs merges neighbours that share a style.
#[tokio::test]
async fn a_listing_reads_back_with_its_colours_and_its_font() {
    let h = harness().await;
    h.mount_json("GET", LISTING_AT, fixture("docs_listing.json"))
        .await;
    let outline = docs::outline(&h.client, CONNECTION, LISTING).await.unwrap();

    let plain = outline.paragraph(1).unwrap().formatting();
    assert_eq!(plain.len(), 1);
    assert_eq!((plain[0].start, plain[0].end), (0, 8));
    assert_eq!(plain[0].text, "Przykład");
    assert_eq!(plain[0].style, docs::RunStyle::default());

    let listing = outline.paragraph(2).unwrap().formatting();
    assert_eq!(
        listing
            .iter()
            .map(|r| (r.start, r.end, r.text.as_str(), r.style.colour.as_deref()))
            .collect::<Vec<_>>(),
        [
            (0, 4, "let ", Some("#ff0000")),
            (4, 8, "ż = ", None),
            // The quoted emoji is three characters and four code units, so a
            // colour read at character 8 is a colour written at character 8.
            (8, 11, "\"😀\"", Some("#00ff00")),
            (11, 12, ";", None),
        ]
    );
    // Docs leaves a zero channel out of the JSON, so #ff0000 arrives as red
    // alone and must not come back as #ff0000 minus its black.
    assert_eq!(listing[0].style.bold, Some(true));
    assert_eq!(listing[2].style.italic, Some(true));
    assert_eq!(listing[0].style.italic, None);
    // The font covered the whole listing, so every run carries it.
    for run in &listing {
        assert_eq!(run.style.font.as_deref(), Some(docs::CODE_FONT));
        assert_eq!(run.style.size, Some(10.0));
    }
}

// ----- a picture ---------------------------------------------------------------

/// The URL Google is handed. In production this server mints it and Google
/// fetches it while the batch is in flight; the plan only carries it.
const PICTURE_URL: &str = "https://gmcp.example/dl/heldUpload0000000000";

fn picture(width_pt: Option<f64>, height_pt: Option<f64>) -> docs::NewImage<'static> {
    docs::NewImage {
        uri: PICTURE_URL,
        width_pt,
        height_pt,
        label: "wykres.png",
    }
}

#[tokio::test]
async fn a_picture_is_inserted_into_a_paragraph_of_its_own() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    // After a paragraph in the middle: the break goes in at the index the
    // next paragraph starts, and the picture goes inside the paragraph the
    // break has just made.
    let middle = docs::plan_image(
        &outline,
        REVISION,
        2,
        &picture(Some(300.0), None),
        Some("Część"),
    )
    .unwrap();
    assert_eq!(middle.after, "wykres.png");
    docs::apply(&h.client, CONNECTION, ARTICLE, middle)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([
            {"insertText": {"text": "\n", "location": {"index": 30}}},
            {"insertInlineImage": {
                "uri": PICTURE_URL,
                "location": {"index": 30},
                "objectSize": {"width": {"magnitude": 300.0, "unit": "PT"}}
            }}
        ])
    );

    // After the last paragraph the break comes first, because nothing may be
    // written after the body's final newline.
    let last = docs::plan_image(
        &outline,
        REVISION,
        5,
        &picture(Some(300.0), Some(200.0)),
        None,
    )
    .unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, last)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"],
        json!([
            {"insertText": {"text": "\n", "location": {"index": 86}}},
            {"insertInlineImage": {
                "uri": PICTURE_URL,
                "location": {"index": 87},
                "objectSize": {
                    "width": {"magnitude": 300.0, "unit": "PT"},
                    "height": {"magnitude": 200.0, "unit": "PT"}
                }
            }}
        ])
    );

    // Neither side given is no objectSize at all, which is Docs' own way of
    // saying the picture keeps the size it is.
    let own_size = docs::plan_image(&outline, REVISION, 2, &picture(None, None), None).unwrap();
    docs::apply(&h.client, CONNECTION, ARTICLE, own_size)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", ARTICLE_BATCH).await["requests"][1]["insertInlineImage"],
        json!({"uri": PICTURE_URL, "location": {"index": 30}})
    );

    // The two locks and a measurement Docs would refuse, each of them before
    // anything is sent.
    let sent = h
        .requests()
        .await
        .iter()
        .filter(|r| r.url.path() == ARTICLE_BATCH)
        .count();
    for refused in [
        docs::plan_image(&outline, "ALm37BW0Older", 2, &picture(None, None), None),
        docs::plan_image(&outline, REVISION, 2, &picture(None, None), Some("Koniec")),
        docs::plan_image(&outline, REVISION, 9, &picture(None, None), None),
        docs::plan_image(&outline, REVISION, 2, &picture(Some(0.0), None), None),
        docs::plan_image(&outline, REVISION, 2, &picture(Some(-10.0), None), None),
        docs::plan_image(&outline, REVISION, 2, &picture(None, Some(f64::NAN)), None),
    ] {
        assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
    }
    assert_eq!(
        h.requests()
            .await
            .iter()
            .filter(|r| r.url.path() == ARTICLE_BATCH)
            .count(),
        sent
    );
}

// ----- a table ---------------------------------------------------------------

/// What `docs_article_table.json` says the document is at: the article with
/// the empty two-by-two table in it, which is what the second batch of a
/// table write is planned against.
const TABLE_REVISION: &str = "ALm37BW0Article2";

/// The two reads a table write makes, in the order it makes them: the article
/// as it is, and then the article with the empty table Google has just put in
/// it. The second fixture is a table as the Docs API describes one — rows,
/// cells, and a paragraph of its own in every cell — so the indexes the code
/// fills at are Google's and not this test's arithmetic.
async fn article_then_table(h: &Harness) -> docs::Outline {
    Mock::given(method("GET"))
        .and(path(ARTICLE_AT))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_article.json")))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    let outline = docs::outline(&h.client, CONNECTION, ARTICLE).await.unwrap();
    h.mount_json("GET", ARTICLE_AT, fixture("docs_article_table.json"))
        .await;
    outline
}

/// Every `batchUpdate` the server saw, oldest first.
async fn batches(h: &Harness) -> Vec<Value> {
    h.requests()
        .await
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == ARTICLE_BATCH)
        .map(|r| serde_json::from_slice(&r.body).expect("a batch body is JSON"))
        .collect()
}

fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|c| c.to_string()).collect())
        .collect()
}

#[tokio::test]
async fn a_table_is_filled_at_the_indexes_the_re_read_answered_with() {
    let h = harness().await;
    let outline = article_then_table(&h).await;
    mount_batch(&h).await;

    let rows = grid(&[&["Model", "Parametry"], &["Mistral-7B", "7 mld"]]);
    let plan = docs::plan_table(&outline, REVISION, 2, &rows, false, Some("Część")).unwrap();
    assert_eq!(plan.rows, 2);
    assert_eq!(plan.columns, 2);
    assert_eq!(plan.lines(), ["Model | Parametry", "Mistral-7B | 7 mld"]);
    let written = docs::apply_table(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();

    // The cells are paragraphs 4 to 7 of the document the re-read described:
    // the table's four cells, numbered where the body meets them.
    assert_eq!(
        written,
        docs::InsertedTable {
            rows: 2,
            columns: 2,
            first_paragraph: 4,
            last_paragraph: 7,
            index: 31,
        }
    );

    let sent = batches(&h).await;
    assert_eq!(sent.len(), 2, "the empty grid, and then its cells");
    // The empty grid goes in where a new paragraph would: Docs writes the
    // newline before it itself.
    assert_eq!(
        sent[0],
        json!({
            "requests": [{"insertTable": {
                "rows": 2, "columns": 2, "location": {"index": 30}
            }}],
            "writeControl": {"requiredRevisionId": REVISION}
        })
    );
    // The cells are filled from the last to the first, so that no insert
    // moves an index that is still to be used, and every index here is one
    // the fixture says Google gave for a cell's own paragraph.
    assert_eq!(
        sent[1],
        json!({
            "requests": [
                {"insertText": {"text": "7 mld", "location": {"index": 40}}},
                {"insertText": {"text": "Mistral-7B", "location": {"index": 38}}},
                {"insertText": {"text": "Parametry", "location": {"index": 35}}},
                {"insertText": {"text": "Model", "location": {"index": 33}}}
            ],
            // The revision of this server's own re-read, and not the
            // caller's: the document has moved on by one write, its own.
            "writeControl": {"requiredRevisionId": TABLE_REVISION}
        })
    );
    assert_ne!(TABLE_REVISION, REVISION);
}

#[tokio::test]
async fn a_header_row_is_bolded_where_its_own_text_was_just_written() {
    let h = harness().await;
    let outline = article_then_table(&h).await;
    mount_batch(&h).await;

    // Polish letters are one UTF-16 unit and two UTF-8 bytes; the emoji is
    // two units and four bytes. "Zażółć 😀" is 8 characters, 13 bytes and 9
    // units, and only the units make the bold cover the whole cell.
    let rows = grid(&[&["Zażółć 😀", "Kolumna"], &["już", ""]]);
    let plan = docs::plan_table(&outline, REVISION, 2, &rows, true, None).unwrap();
    assert!(plan.header);
    docs::apply_table(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();

    let sent = batches(&h).await;
    assert_eq!(
        sent[1]["requests"],
        json!([
            // The empty cell is skipped: Docs refuses an insert of no text,
            // and an empty cell is what it already is.
            {"insertText": {"text": "już", "location": {"index": 38}}},
            {"insertText": {"text": "Kolumna", "location": {"index": 35}}},
            {"updateTextStyle": {
                "range": {"startIndex": 35, "endIndex": 42},
                "textStyle": {"bold": true},
                "fields": "bold"
            }},
            {"insertText": {"text": "Zażółć 😀", "location": {"index": 33}}},
            {"updateTextStyle": {
                "range": {"startIndex": 33, "endIndex": 42},
                "textStyle": {"bold": true},
                "fields": "bold"
            }}
        ]),
        "each header cell is bolded straight after its own insert, before an \
         insert at a lower index moves it"
    );
    // Only the first row is bold, whatever the rest of the table says.
    assert_eq!(
        sent[1]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r.get("updateTextStyle").is_some())
            .count(),
        2
    );

    // Without the header the same grid carries no styling at all.
    let h = harness().await;
    let outline = article_then_table(&h).await;
    mount_batch(&h).await;
    let plan = docs::plan_table(&outline, REVISION, 2, &rows, false, None).unwrap();
    docs::apply_table(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap();
    let plain = batches(&h).await;
    assert!(
        plain[1]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("updateTextStyle").is_none()),
        "{}",
        plain[1]
    );
}

#[tokio::test]
async fn a_grid_that_is_not_a_table_is_refused_before_any_call() {
    let h = harness().await;
    let outline = article(&h).await;
    mount_batch(&h).await;

    let wide = grid(&[&["x"; 21]]);
    let tall: Vec<Vec<String>> = (0..101).map(|_| vec!["x".to_string()]).collect();
    for (rows, wanted) in [
        (
            grid(&[&["a", "b"], &["c"], &["d", "e"]]),
            "row 2 holds 1 cell",
        ),
        (grid(&[&["a"], &["b", "c"]]), "row 2 holds 2 cells"),
        (wide, "21 columns"),
        (tall, "101 rows"),
        (Vec::new(), "no rows"),
        (vec![Vec::new()], "no cells"),
        (grid(&[&["a\nb"]]), "line break"),
    ] {
        let refused = docs::plan_table(&outline, REVISION, 2, &rows, false, None).unwrap_err();
        assert!(matches!(refused, Error::Unsupported(_)), "{refused:?}");
        assert!(refused.to_string().contains(wanted), "{refused}");
        assert!(
            refused.to_string().contains("Nothing was written")
                || refused.to_string().contains("nothing to insert"),
            "{refused}"
        );
    }
    // The two locks are checked here as well, and none of these reached
    // Google: a refused grid costs no call at all.
    let rows = grid(&[&["a"]]);
    for refused in [
        docs::plan_table(&outline, "ALm37BW0Older", 2, &rows, false, None),
        docs::plan_table(&outline, REVISION, 2, &rows, false, Some("Koniec")),
        docs::plan_table(&outline, REVISION, 9, &rows, false, None),
    ] {
        assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
    }
    assert!(batches(&h).await.is_empty());
}

#[tokio::test]
async fn a_second_batch_google_refuses_says_the_table_is_there_and_empty() {
    let h = harness().await;
    let outline = article_then_table(&h).await;
    // The first batch goes through and the second is refused, which is the
    // one moment this tool can leave a document changed and unfinished.
    Mock::given(method("POST"))
        .and(path(ARTICLE_BATCH))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("docs_batch_update.json")))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(ARTICLE_BATCH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"code": 400, "message": "Revision ALm37BW0Article2 is not the latest"}
        })))
        .mount(&h.server)
        .await;

    let rows = grid(&[&["Model", "Parametry"], &["Mistral-7B", "7 mld"]]);
    let plan = docs::plan_table(&outline, REVISION, 2, &rows, true, None).unwrap();
    let refused = docs::apply_table(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap_err();

    assert!(matches!(refused, Error::Unsupported(_)), "{refused:?}");
    let said = refused.to_string();
    // What is true of the document now, where it is, and what to do about it.
    assert!(
        said.contains("2 by 2 table was created and is empty"),
        "{said}"
    );
    assert!(said.contains("paragraphs 4 to 7"), "{said}");
    assert!(said.contains("docs_insert_text"), "{said}");
    assert!(said.contains("docs_edit_paragraph"), "{said}");
    assert!(said.contains("docs_list_paragraphs"), "{said}");
    assert!(said.contains("Revision ALm37BW0Article2"), "{said}");
    assert_eq!(batches(&h).await.len(), 2, "the second batch was attempted");
}

#[tokio::test]
async fn a_table_that_cannot_be_found_again_is_not_guessed_at() {
    let h = harness().await;
    // The re-read answers the article as it was, which holds no empty
    // two-by-two table. Rather than write into the one-by-one table that is
    // there, the write stops and says where to look.
    h.mount_json("GET", ARTICLE_AT, fixture("docs_article.json"))
        .await;
    let outline = docs::outline(&h.client, CONNECTION, ARTICLE).await.unwrap();
    mount_batch(&h).await;

    let rows = grid(&[&["Model", "Parametry"], &["Mistral-7B", "7 mld"]]);
    let plan = docs::plan_table(&outline, REVISION, 2, &rows, false, None).unwrap();
    let refused = docs::apply_table(&h.client, CONNECTION, ARTICLE, plan)
        .await
        .unwrap_err();
    let said = refused.to_string();
    assert!(said.contains("created and is empty"), "{said}");
    assert!(said.contains("could not find it"), "{said}");
    // One batch, and nothing written into anybody else's table.
    assert_eq!(batches(&h).await.len(), 1);
}
