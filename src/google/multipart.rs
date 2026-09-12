//! The `multipart/related` body Google's upload endpoints take: a JSON
//! metadata part and the content beside it, in one request.
//!
//! Two services need it and neither owns it. Drive converts a CSV into a
//! Sheet this way, and Gmail takes a draft this way once the message is too
//! large to go as JSON — a draft with a file attached to it is.

/// The two parts, with CRLF line endings as the format requires.
/// `content_type` is written into the part's header exactly as it is given.
pub fn related(boundary: &str, metadata: &str, content_type: &str, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(content.len() + metadata.len() + 256);
    let mut push = |s: &str| body.extend_from_slice(s.as_bytes());
    push(&format!("--{boundary}\r\n"));
    push("Content-Type: application/json; charset=UTF-8\r\n\r\n");
    push(metadata);
    push(&format!("\r\n--{boundary}\r\n"));
    push(&format!("Content-Type: {content_type}\r\n\r\n"));
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// A boundary that cannot occur in the content. Random rather than fixed
/// because the content is whatever a model wrote.
pub fn boundary() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("os randomness");
    format!("gmcp{}", hex::encode(bytes))
}

/// The `Content-Type` header a request carrying such a body needs.
pub fn header(boundary: &str) -> String {
    format!("multipart/related; boundary={boundary}")
}
