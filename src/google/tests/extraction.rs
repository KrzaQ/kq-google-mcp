//! Text extraction from every source the plan names.

use super::*;
use crate::google::{drive, gmail, text};

#[tokio::test]
async fn plain_text_and_csv_come_back_as_they_are() {
    let extractor = text::Extractor::new();
    let extracted = extractor
        .bytes(
            "text/plain; charset=utf-8",
            "notes.txt",
            b"Two lines\nof text\n",
        )
        .await
        .unwrap();
    assert_eq!(extracted.text, "Two lines\nof text\n");
    assert_eq!(extracted.truncated_chars, 0);
    assert_eq!(extracted.source, "text");

    let csv = extractor
        .bytes("text/csv", "hours.csv", b"a,b\n1,2\n")
        .await
        .unwrap();
    assert_eq!(csv.text, "a,b\n1,2\n");

    // Bytes that are not UTF-8 still produce something readable.
    let lossy = extractor
        .bytes("text/plain", "latin.txt", &[0x41, 0xff, 0x42])
        .await
        .unwrap();
    assert!(lossy.text.starts_with('A') && lossy.text.ends_with('B'));

    // A mail client that says octet-stream is believed about the extension.
    let guessed = extractor
        .bytes("application/octet-stream", "notes.txt", b"hello")
        .await
        .unwrap();
    assert_eq!(guessed.text, "hello");

    let refused = extractor
        .bytes("image/png", "chart.png", b"\x89PNG")
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("download link"), "{refused}");
}

#[tokio::test]
async fn a_pdf_says_clearly_that_poppler_is_missing() {
    // The absence is simulated by looking for a program that is not there,
    // so the test does not depend on what the machine has installed.
    let extractor = text::Extractor::with_program("gmcp-no-such-pdftotext");
    assert!(!extractor.pdftotext_available());
    let error = extractor
        .bytes("application/pdf", "q3.pdf", b"%PDF-1.7")
        .await
        .unwrap_err();
    assert!(matches!(error, Error::PdftotextMissing), "{error:?}");
    assert!(error.to_string().contains("pdftotext"), "{error}");
    assert!(error.to_string().contains("not installed"), "{error}");
}

/// A minimal .docx: a zip with the one part the extractor reads.
fn docx(document_xml: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buffer = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("[Content_Types].xml", options).unwrap();
        zip.write_all(b"<Types/>").unwrap();
        zip.start_file("word/document.xml", options).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buffer
}

#[tokio::test]
async fn a_docx_gives_up_its_paragraphs() {
    let bytes = docx(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Minutes of the meeting</w:t></w:r></w:p>
    <w:p><w:r><w:t xml:space="preserve">Present: </w:t></w:r><w:r><w:t>Anna &amp; Marta</w:t></w:r></w:p>
    <w:p><w:r><w:t>Line one</w:t><w:br/><w:t>line two</w:t></w:r></w:p>
    <w:p/>
    <w:p><w:r><w:t>End</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
    );
    let extracted = text::Extractor::new()
        .bytes(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "minutes.docx",
            &bytes,
        )
        .await
        .unwrap();
    assert_eq!(
        extracted.text,
        "Minutes of the meeting\nPresent: Anna & Marta\nLine one\nline two\n\nEnd"
    );
    assert_eq!(extracted.source, "docx");

    let not_a_docx = text::docx(b"this is not a zip").unwrap_err();
    assert!(not_a_docx.to_string().contains(".docx"), "{not_a_docx}");
    let wrong_zip = text::docx(&{
        use std::io::Write;
        let mut buffer = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        zip.start_file("other.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        buffer
    })
    .unwrap_err();
    assert!(
        wrong_zip.to_string().contains("word/document.xml"),
        "{wrong_zip}"
    );
}

#[tokio::test]
async fn a_doc_full_of_screenshots_comes_back_as_words() {
    // What Drive's markdown export writes: the picture is a base64 data URI
    // in a reference definition, and the body only points at its label. The
    // real document that prompted this was 201,000 characters, of which
    // 20,000 were text.
    let png = "iVBORw0KGgoAAAANSUhEUg".repeat(4096);
    let exported = format!(
        "# Notes\n\nSee the diagram: ![][image1]\n\nAnd the other: ![][image2]\n\n\
         [image1]: <data:image/png;base64,{png}>\n[image2]: <data:image/jpeg;base64,{png}>\n"
    );
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(
            "/drive/v3/files/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc/export",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(exported.as_bytes().to_vec(), "text/markdown"),
        )
        .mount(&h.server)
        .await;

    let doc = text::google_doc(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
    )
    .await
    .unwrap();

    // Not one byte of base64 survives, and what is left is the writing.
    assert!(!doc.contains("data:image"), "{doc}");
    assert!(!doc.contains("iVBORw0KGgo"), "{doc}");
    assert!(doc.contains("# Notes"), "{doc}");
    assert!(doc.contains("See the diagram: ![][image1]"), "{doc}");
    // Each label still resolves, to a line saying what stood there and what
    // to call to see it. The label is the argument, so a model reading this in
    // the middle of a document has the whole call in front of it.
    assert!(
        doc.contains(r#"[image1]: <a PNG of 66 KB, not included; docs_view_image image="image1">"#),
        "{doc}"
    );
    assert!(
        doc.contains(
            r#"[image2]: <a JPEG of 66 KB, not included; docs_view_image image="image2">"#
        ),
        "{doc}"
    );
    assert!(doc.contains("2 images were left out of this text"), "{doc}");
    assert!(doc.contains("docs_list_images lists them"), "{doc}");
    // The export was 180 KB of base64 and the text is a few lines.
    assert!(doc.len() < 500, "{} chars: {doc}", doc.len());
}

#[tokio::test]
async fn a_google_doc_is_read_as_markdown_and_a_sheet_as_csv() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path(
            "/drive/v3/files/1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc/export",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "# Q3 report\n\n- Revenue held\n".as_bytes().to_vec(),
            "text/markdown",
        ))
        .mount(&h.server)
        .await;
    let doc = text::google_doc(
        &h.client,
        CONNECTION,
        "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc",
    )
    .await
    .unwrap();
    assert_eq!(doc, "# Q3 report\n\n- Revenue held\n");

    h.mount_json(
        "GET",
        &format!("/v4/spreadsheets/{SHEET}"),
        fixture("sheets_spreadsheet.json"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v4/spreadsheets/[^/]+/values/.+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("sheets_values.json")))
        .mount(&h.server)
        .await;
    let sheet = text::google_sheet(&h.client, CONNECTION, SHEET)
        .await
        .unwrap();
    assert!(sheet.starts_with("## September\n\n"), "{sheet}");
    assert!(sheet.contains("## Rates"), "{sheet}");
    // Quoting is the csv crate's job, and a cell with a quote survives it.
    assert!(sheet.contains(r#""Migration, ""phase two"""#), "{sheet}");
}

#[test]
fn rows_become_csv_with_the_quoting_a_cell_needs() {
    let csv = text::to_csv(&[
        vec!["plain".into(), "with, comma".into()],
        vec!["with \"quotes\"".into(), "with\nnewline".into()],
    ])
    .unwrap();
    assert_eq!(
        csv,
        "plain,\"with, comma\"\n\"with \"\"quotes\"\"\",\"with\nnewline\"\n"
    );
}

#[test]
fn long_text_is_cut_with_a_line_saying_how_much() {
    use crate::domain::limits::TEXT_MAX_CHARS;

    let short = text::Extraction::new("short".into(), "text");
    assert_eq!(short.truncated_chars, 0);
    assert_eq!(short.text, "short");

    let long = text::Extraction::new("x".repeat(TEXT_MAX_CHARS + 250), "text");
    assert_eq!(long.truncated_chars, 250);
    assert!(
        long.text
            .ends_with("[… 250 more characters were cut off here]")
    );
    assert_eq!(
        long.text.chars().filter(|c| *c == 'x').count(),
        TEXT_MAX_CHARS
    );

    // The cap counts characters and not bytes, so multi-byte text is not cut
    // in the middle of one.
    let wide = text::Extraction::new("ł".repeat(TEXT_MAX_CHARS + 1), "text");
    assert_eq!(wide.truncated_chars, 1);
}

#[tokio::test]
async fn a_drive_file_takes_the_route_its_type_calls_for() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/drive/v3/files/[^/]+/export$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("# Q3 report".as_bytes().to_vec(), "text/markdown"),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/drive/v3/files/1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf",
        ))
        .and(query_param("alt", "media"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("plain bytes".as_bytes().to_vec(), "text/plain"),
        )
        .mount(&h.server)
        .await;
    let extractor = text::Extractor::new();

    let doc = drive::FileMeta {
        id: "1AbCdEfGhIjKlMnOpQrStUvWxYz0123456789doc".into(),
        name: "Q3 report".into(),
        mime_type: drive::DOCUMENT_MIME.into(),
        modified_time: None,
        size: None,
        owners: vec![],
        web_view_link: None,
        parents: vec![],
    };
    let extracted = text::drive_file(&h.client, CONNECTION, &extractor, &doc)
        .await
        .unwrap();
    assert_eq!(extracted.source, "google-doc");
    assert_eq!(extracted.text, "# Q3 report");

    let plain = drive::FileMeta {
        id: "1ZyXwVuTsRqPoNmLkJiHgFeDcBa9876543210pdf".into(),
        name: "notes.txt".into(),
        mime_type: "text/plain".into(),
        ..doc.clone()
    };
    let extracted = text::drive_file(&h.client, CONNECTION, &extractor, &plain)
        .await
        .unwrap();
    assert_eq!(extracted.text, "plain bytes");

    let form = drive::FileMeta {
        mime_type: "application/vnd.google-apps.form".into(),
        name: "Signup".into(),
        ..doc
    };
    let error = text::drive_file(&h.client, CONNECTION, &extractor, &form)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not read those"), "{error}");
}

#[tokio::test]
async fn a_gmail_attachment_is_fetched_and_extracted() {
    let h = harness().await;
    h.mount_json(
        "GET",
        "/gmail/v1/users/me/messages/18f0a1b2c3d4e5f6/attachments/ANGjdJ8txt",
        json!({"size": 5, "data": base64_url("hello")}),
    )
    .await;
    let attachment = gmail::Attachment {
        part_id: "1".into(),
        id: "ANGjdJ8txt".into(),
        filename: "hello.txt".into(),
        mime_type: "text/plain".into(),
        size: 5,
    };
    let extracted = text::gmail_attachment(
        &h.client,
        CONNECTION,
        &text::Extractor::new(),
        "18f0a1b2c3d4e5f6",
        &attachment,
    )
    .await
    .unwrap();
    assert_eq!(extracted.text, "hello");
}

fn base64_url(value: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}
