//! Drive: search, metadata, download, export and the two imports.

use super::*;
use crate::google::drive;

#[tokio::test]
async fn a_drive_search_builds_one_query_and_never_lists_the_bin() {
    let h = harness().await;
    h.mount_json("GET", "/drive/v3/files", fixture("drive_files_list.json"))
        .await;

    let files = drive::list(
        &h.client,
        CONNECTION,
        &drive::Search {
            name_contains: Some("q3".into()),
            mime_type: Some(drive::DOCUMENT_MIME.into()),
            modified_after: Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                    .unwrap()
                    .into(),
            ),
            query: Some("'me' in owners".into()),
            max: Some(5),
        },
    )
    .await
    .unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].name, "Q3 report");
    assert!(files[0].is_google_doc());
    assert!(files[0].size.is_none());
    assert_eq!(files[1].size, Some(26112));
    assert_eq!(files[1].owners, ["marta@example.test"]);

    let query: std::collections::HashMap<_, _> = h
        .last("GET", "/drive/v3/files")
        .await
        .url
        .query_pairs()
        .into_owned()
        .collect();
    let q = &query["q"];
    assert!(q.starts_with("trashed = false"), "{q}");
    assert!(q.contains("name contains 'q3'"), "{q}");
    assert!(q.contains("modifiedTime > '2026-09-01T00:00:00Z'"), "{q}");
    assert!(q.contains("('me' in owners)"), "{q}");
    assert_eq!(query["pageSize"], "5");
    // Shared drives stay out of this release.
    assert!(!query.contains_key("corpora"));
    assert!(!query.contains_key("supportsAllDrives"));
}

#[test]
fn a_drive_query_escapes_what_a_file_name_may_contain() {
    let search = drive::Search {
        name_contains: Some("Bob's \\ notes".into()),
        ..Default::default()
    };
    assert_eq!(
        search.to_query(),
        r"trashed = false and name contains 'Bob\'s \\ notes'"
    );
}

#[tokio::test]
async fn a_file_is_read_downloaded_and_exported() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/drive/v3/files/1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf",
        fixture("drive_file.json"),
    )
    .await;
    let file = drive::get(
        &h.client,
        CONNECTION,
        "1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf",
    )
    .await
    .unwrap();
    assert_eq!(file.mime_type, "application/pdf");
    assert!(!file.is_google_native());

    // The bytes route: same path, alt=media, and the response is streamed.
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(
            "/drive/v3/files/1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf",
        ))
        .and(query_param("alt", "media"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("%PDF-1.7 bytes".as_bytes().to_vec(), "application/pdf"),
        )
        .mount(&h.server)
        .await;
    let download = drive::download(
        &h.client,
        CONNECTION,
        "1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf",
    )
    .await
    .unwrap();
    assert_eq!(download.mime_type(), Some("application/pdf"));
    assert_eq!(download.size(), Some(14));
    assert_eq!(
        String::from_utf8(download.collect().await.unwrap()).unwrap(),
        "%PDF-1.7 bytes"
    );

    // The export route, for a file that has no bytes of its own.
    Mock::given(method("GET"))
        .and(path(
            "/drive/v3/files/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc/export",
        ))
        .and(query_param("mimeType", "text/markdown"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "# Q3 report\n\nRevenue.".as_bytes().to_vec(),
            "text/markdown",
        ))
        .mount(&h.server)
        .await;
    let exported = drive::export(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
        drive::ExportFormat::Markdown,
    )
    .await
    .unwrap()
    .collect()
    .await
    .unwrap();
    assert_eq!(
        String::from_utf8(exported).unwrap(),
        "# Q3 report\n\nRevenue."
    );
}

#[test]
fn export_formats_are_refused_where_they_make_no_sense() {
    let doc = drive::FileMeta {
        id: "d".into(),
        name: "Q3 report".into(),
        mime_type: drive::DOCUMENT_MIME.into(),
        modified_time: None,
        size: None,
        owners: vec![],
        web_view_link: None,
        parents: vec![],
    };
    let pdf = drive::FileMeta {
        mime_type: "application/pdf".into(),
        name: "q3.pdf".into(),
        ..doc.clone()
    };
    assert!(drive::check_export_format(&doc, drive::ExportFormat::Markdown).is_ok());
    assert!(drive::check_export_format(&doc, drive::ExportFormat::Docx).is_ok());
    let error = drive::check_export_format(&doc, drive::ExportFormat::Xlsx).unwrap_err();
    assert!(error.to_string().contains("markdown, pdf, docx"), "{error}");
    let error = drive::check_export_format(&pdf, drive::ExportFormat::Pdf).unwrap_err();
    assert!(error.to_string().contains("downloaded"), "{error}");
    assert_eq!(
        "MARKDOWN".parse::<drive::ExportFormat>().unwrap(),
        drive::ExportFormat::Markdown
    );
    assert!("odt".parse::<drive::ExportFormat>().is_err());
}

#[tokio::test]
async fn a_doc_is_made_out_of_markdown_in_one_multipart_upload() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/upload/drive/v3/files",
        fixture("drive_file_created.json"),
    )
    .await;

    let created = drive::create_doc_from_markdown(
        &h.client,
        CONNECTION,
        "Meeting notes",
        "# Notes\n\n- one\n- two\n",
        Some("0AFolderIdExample01234"),
    )
    .await
    .unwrap();
    assert_eq!(created.name, "Meeting notes");
    assert!(created.is_google_doc());

    let request = h.last("POST", "/upload/drive/v3/files").await;
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert_eq!(query["uploadType"], "multipart");
    let content_type = request.headers["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("multipart/related; boundary="),
        "{content_type}"
    );
    let body = String::from_utf8_lossy(&request.body);
    assert!(
        body.contains("\"mimeType\":\"application/vnd.google-apps.document\""),
        "{body}"
    );
    assert!(body.contains("\"name\":\"Meeting notes\""), "{body}");
    assert!(
        body.contains("\"parents\":[\"0AFolderIdExample01234\"]"),
        "{body}"
    );
    assert!(body.contains("Content-Type: text/markdown"), "{body}");
    assert!(body.contains("# Notes"), "{body}");

    // The same route makes a Sheet out of CSV.
    drive::create_sheet_from_csv(&h.client, CONNECTION, "Hours", "a,b\n1,2\n", None)
        .await
        .unwrap();
    let body =
        String::from_utf8_lossy(&h.last("POST", "/upload/drive/v3/files").await.body).into_owned();
    assert!(
        body.contains("application/vnd.google-apps.spreadsheet"),
        "{body}"
    );
    assert!(body.contains("Content-Type: text/csv"), "{body}");
    assert!(body.contains("\"parents\":[]"), "{body}");
}

#[tokio::test]
async fn comments_are_asked_for_with_the_replies_and_the_quoted_text() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/drive/v3/files/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc/comments",
        fixture("drive_comments.json"),
    )
    .await;

    let read = drive::comments(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
    )
    .await
    .unwrap();
    assert!(!read.more, "the fixture is one whole page");
    assert_eq!(read.threads.len(), 3);

    let first = &read.threads[0];
    assert_eq!(first.author.as_deref(), Some("marta@example.test"));
    assert_eq!(first.text, "Is this the number before or after the refund?");
    assert_eq!(
        first.quoted_text.as_deref(),
        Some("Revenue held up in September.")
    );
    assert!(!first.resolved);
    // The replies come back in the order the thread reads.
    let replies: Vec<&str> = first.replies.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(replies, ["After.", "Then say so in the sentence."]);
    assert_eq!(
        first.replies[0].author.as_deref(),
        Some("anna@example.test")
    );
    assert_eq!(
        first.replies[1].created_time.unwrap().to_rfc3339(),
        "2026-09-03T10:05:00+00:00"
    );

    assert!(read.threads[1].resolved);
    // A comment on the whole file is anchored to nothing, and an author Google
    // gives no address for is named the way a person is.
    assert_eq!(read.threads[2].quoted_text, None);
    assert_eq!(read.threads[2].author.as_deref(), Some("Redakcja"));

    // Drive's default projection carries neither the replies nor the text a
    // comment is anchored to, so the call names the fields it needs.
    let query: std::collections::HashMap<_, _> = h
        .last(
            "GET",
            "/drive/v3/files/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc/comments",
        )
        .await
        .url
        .query_pairs()
        .into_owned()
        .collect();
    let fields = &query["fields"];
    assert!(fields.contains("replies("), "{fields}");
    assert!(fields.contains("quotedFileContent"), "{fields}");
    assert!(fields.contains("resolved"), "{fields}");
    assert!(
        fields.contains("author(displayName,emailAddress)"),
        "{fields}"
    );
    assert!(fields.contains("nextPageToken"), "{fields}");
    assert_eq!(query["pageSize"], "100");
    // Nothing here asks for the deleted comments, whose content Google strips.
    assert!(!query.contains_key("includeDeleted"));
}
