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
    // No folder means no `parents` at all, which is what puts a file in the
    // root of My Drive.
    assert!(!body.contains("\"parents\""), "{body}");
}

/// An upload is stored as it is: the metadata names the type the content
/// carries, so Drive has nothing to convert it into, and the bytes arrive
/// unchanged whether or not they are text.
#[tokio::test]
async fn a_file_is_stored_as_it_is_and_never_converted() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/upload/drive/v3/files",
        fixture("drive_file_uploaded.json"),
    )
    .await;
    // Not UTF-8, and holding a CRLF and a boundary-like line, so a body that
    // was ever turned into text on the way would not come out the same.
    let bytes = b"%PDF-1.7\r\n--gmcp\r\n\xff\xfe\x00\x01".to_vec();

    let stored = drive::upload(
        &h.client,
        CONNECTION,
        "Faktura 04-2026.pdf",
        "application/pdf",
        &bytes,
        Some("1FaKtUrYfOlDeRiDeXaMpLe0123456789"),
    )
    .await
    .unwrap();
    assert_eq!(stored.id, "1UpLoAdEdFiLeIdExAmPlE0123456789abcd");
    assert_eq!(stored.size, Some(16));

    let request = h.last("POST", "/upload/drive/v3/files").await;
    let body = &request.body;
    let text = String::from_utf8_lossy(body);
    assert!(
        text.contains("\"mimeType\":\"application/pdf\""),
        "the metadata names the file's own type: {text}"
    );
    assert!(
        text.contains("Content-Type: application/pdf\r\n\r\n"),
        "the content part carries no charset: {text}"
    );
    assert!(
        body.windows(bytes.len()).any(|w| w == bytes.as_slice()),
        "the bytes arrive exactly as they were given"
    );

    // A Google type is how a conversion is asked for, so it is refused before
    // anything is sent.
    let before = h.requests().await.len();
    let refused = drive::upload(
        &h.client,
        CONNECTION,
        "notes",
        drive::DOCUMENT_MIME,
        b"# notes",
        None,
    )
    .await
    .unwrap_err();
    assert!(refused.to_string().contains("stored as it is"), "{refused}");
    assert_eq!(h.requests().await.len(), before);
}

/// A folder is read with the one field that decides whether a file may go
/// into it at all.
#[tokio::test]
async fn a_folder_is_read_with_whether_it_is_in_the_bin() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/drive/v3/files/1FaKtUrYfOlDeRiDeXaMpLe0123456789",
        fixture("drive_folder.json"),
    )
    .await;
    let folder = drive::folder(&h.client, CONNECTION, "1FaKtUrYfOlDeRiDeXaMpLe0123456789")
        .await
        .unwrap();
    assert_eq!(folder.name, "Faktury 2026");
    assert_eq!(folder.mime_type, drive::FOLDER_MIME);
    assert!(!folder.trashed);
    let request = h
        .last("GET", "/drive/v3/files/1FaKtUrYfOlDeRiDeXaMpLe0123456789")
        .await;
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert!(query["fields"].contains("trashed"), "{query:?}");
    assert!(!query.contains_key("supportsAllDrives"), "{query:?}");
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

#[tokio::test]
async fn a_folder_is_created_with_the_folder_type_and_its_parent_and_nothing_else() {
    let h = harness().await;
    h.mount_json(
        "POST",
        "/drive/v3/files",
        fixture("drive_subfolder_created.json"),
    )
    .await;

    let folder = drive::create_folder(
        &h.client,
        CONNECTION,
        "notatki",
        Some("1ToPoLoGiAnEwFoLdErIdExAmPlE012345"),
    )
    .await
    .unwrap();
    assert_eq!(folder.id, "1NoTaTkInEwFoLdErIdExAmPlE01234567");
    assert_eq!(folder.mime_type, drive::FOLDER_MIME);
    assert_eq!(folder.parents, ["1ToPoLoGiAnEwFoLdErIdExAmPlE012345"]);
    assert!(folder.size.is_none(), "a folder has no bytes");

    // The metadata endpoint, with a JSON body and no media part: the three
    // fields a folder needs, and nothing else.
    let request = h.last("POST", "/drive/v3/files").await;
    assert!(
        request.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "{:?}",
        request.headers
    );
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert!(!query.contains_key("uploadType"), "{query:?}");
    assert!(!query.contains_key("supportsAllDrives"), "{query:?}");
    assert_eq!(
        h.last_body("POST", "/drive/v3/files").await,
        json!({
            "name": "notatki",
            "mimeType": "application/vnd.google-apps.folder",
            "parents": ["1ToPoLoGiAnEwFoLdErIdExAmPlE012345"],
        })
    );

    // In the root, the parent is left out rather than sent empty.
    drive::create_folder(&h.client, CONNECTION, "topologia", None)
        .await
        .unwrap();
    assert_eq!(
        h.last_body("POST", "/drive/v3/files").await,
        json!({"name": "topologia", "mimeType": "application/vnd.google-apps.folder"})
    );
    assert!(
        h.requests()
            .await
            .iter()
            .all(|r| !r.url.path().starts_with("/upload/")),
        "a folder has no media to upload"
    );
}

#[test]
fn a_folder_name_with_an_apostrophe_cannot_end_the_query() {
    assert_eq!(
        drive::named_folder_query(Some("1PaReNt"), r"Bob's \ notes"),
        r"name = 'Bob\'s \\ notes' and mimeType = 'application/vnd.google-apps.folder' and '1PaReNt' in parents and trashed = false"
    );
    assert_eq!(
        drive::named_folder_query(None, "topologia"),
        "name = 'topologia' and mimeType = 'application/vnd.google-apps.folder' and 'root' in parents and trashed = false"
    );
}

#[tokio::test]
async fn folders_of_one_name_are_all_found_and_only_that_name_is_kept() {
    let h = harness().await;
    let mut two = fixture("drive_folders_two.json");
    // A folder whose name differs only in case, should Drive's `=` match it.
    two["files"].as_array_mut().unwrap().push(json!({
        "id": "1ToPoLoGiAcApItAlIdExAmPlE01234567",
        "name": "Topologia",
        "mimeType": "application/vnd.google-apps.folder",
        "trashed": false
    }));
    h.mount_json("GET", "/drive/v3/files", two).await;

    let found = drive::folders_named(&h.client, CONNECTION, None, "topologia")
        .await
        .unwrap();
    assert_eq!(
        found
            .folders
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>(),
        [
            "1ToPoLoGiAhAnDmAdEiDeXaMpLe0123456",
            "1ToPoLoGiAsEcOnDiDeXaMpLe012345678"
        ]
    );
    assert!(!found.more);

    let query: std::collections::HashMap<_, _> = h
        .last("GET", "/drive/v3/files")
        .await
        .url
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        query["q"],
        "name = 'topologia' and mimeType = 'application/vnd.google-apps.folder' and 'root' in parents and trashed = false"
    );
    assert!(query["fields"].contains("trashed"), "{query:?}");
    assert!(query["fields"].contains("nextPageToken"), "{query:?}");
    assert!(!query.contains_key("corpora"));

    // An empty answer is no folder at all.
    let h = harness().await;
    h.mount_json("GET", "/drive/v3/files", fixture("drive_folders_none.json"))
        .await;
    let found = drive::folders_named(&h.client, CONNECTION, Some("1PaReNt"), "notatki")
        .await
        .unwrap();
    assert!(found.folders.is_empty());
}

const REVISABLE: &str = "1ReViSaBlEfIlEiDeXaMpLe0123456789ab";
const HEAD: &str = "0B3hEaDrEvIsIoNiDeXaMpLe0123456789";

/// The four calls behind a replacement: the read names the head revision,
/// the list keeps only what is kept forever, the pin asks for keep forever
/// and nothing else, and the new content goes to the upload endpoint of the
/// same file with a name only when one is given.
#[tokio::test]
async fn a_file_is_read_with_its_head_revision_pinned_and_replaced_in_place() {
    let h = harness().await;
    h.mount_json(
        "GET",
        &format!("/drive/v3/files/{REVISABLE}"),
        fixture("drive_file_revisable.json"),
    )
    .await;
    h.mount_json(
        "GET",
        &format!("/drive/v3/files/{REVISABLE}/revisions"),
        fixture("drive_revisions.json"),
    )
    .await;
    h.mount_json(
        "PATCH",
        &format!("/drive/v3/files/{REVISABLE}/revisions/{HEAD}"),
        fixture("drive_revision_kept.json"),
    )
    .await;
    h.mount_json(
        "PATCH",
        &format!("/upload/drive/v3/files/{REVISABLE}"),
        fixture("drive_file_replaced.json"),
    )
    .await;

    let read = drive::revisable(&h.client, CONNECTION, REVISABLE)
        .await
        .unwrap();
    assert_eq!(read.head_revision_id.as_deref(), Some(HEAD));
    assert_eq!(read.file.size, Some(26112));
    let query: std::collections::HashMap<_, _> = h
        .last("GET", &format!("/drive/v3/files/{REVISABLE}"))
        .await
        .url
        .query_pairs()
        .into_owned()
        .collect();
    assert!(query["fields"].contains("headRevisionId"), "{query:?}");

    let kept = drive::kept_forever(&h.client, CONNECTION, REVISABLE)
        .await
        .unwrap();
    assert_eq!(kept.ids, ["0B1oLdErReViSiOnIdExAmPlE012345678"]);
    assert!(!kept.more);

    let pinned = drive::keep_forever(&h.client, CONNECTION, REVISABLE, HEAD)
        .await
        .unwrap();
    assert_eq!(pinned.id, HEAD);
    assert!(pinned.keep_forever);
    assert_eq!(
        h.last_body(
            "PATCH",
            &format!("/drive/v3/files/{REVISABLE}/revisions/{HEAD}")
        )
        .await,
        serde_json::json!({"keepForever": true})
    );

    let bytes = b"%PDF-1.7\r\n\xff\xfe new".to_vec();
    let at = format!("/upload/drive/v3/files/{REVISABLE}");
    let replaced = drive::replace(
        &h.client,
        CONNECTION,
        REVISABLE,
        None,
        "application/pdf",
        &bytes,
    )
    .await
    .unwrap();
    assert_eq!(replaced.id, REVISABLE);
    assert_eq!(replaced.size, Some(16));
    let request = h.last("PATCH", &at).await;
    let query: std::collections::HashMap<_, _> = request.url.query_pairs().into_owned().collect();
    assert_eq!(query["uploadType"], "multipart");
    assert!(!query.contains_key("addParents"), "{query:?}");
    assert!(!query.contains_key("removeParents"), "{query:?}");
    let text = String::from_utf8_lossy(&request.body);
    assert!(
        text.contains("\r\n\r\n{}\r\n"),
        "no name, no metadata: {text}"
    );
    assert!(
        request
            .body
            .windows(bytes.len())
            .any(|w| w == bytes.as_slice()),
        "the bytes arrive exactly as they were given"
    );

    drive::replace(
        &h.client,
        CONNECTION,
        REVISABLE,
        Some("Faktura 03-2026 (korekta).pdf"),
        "application/pdf",
        &bytes,
    )
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&h.last("PATCH", &at).await.body).into_owned();
    assert!(
        text.contains("{\"name\":\"Faktura 03-2026 (korekta).pdf\"}"),
        "{text}"
    );

    // A Google type asks for a conversion, so it is refused before anything
    // is sent.
    let before = h.requests().await.len();
    let refused = drive::replace(
        &h.client,
        CONNECTION,
        REVISABLE,
        None,
        drive::DOCUMENT_MIME,
        b"# notes",
    )
    .await
    .unwrap_err();
    assert!(refused.to_string().contains("stored as it is"), "{refused}");
    assert_eq!(h.requests().await.len(), before);
}
