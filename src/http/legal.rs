//! The privacy policy and the terms, served as plain HTML without a login.
//!
//! Google requires both URLs on the OAuth consent screen before an app that
//! asks for sensitive scopes may be published. They sit here, in the server,
//! rather than on the blog, so the pages describe what this code actually does
//! and change with it.

use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

/// Where a reader is told to write. The consent screen carries the same
/// address, so the two never disagree.
const CONTACT: &str = "the support address shown on the Google consent screen";

const STYLE: &str = concat!(
    ":root { color-scheme: light dark; }",
    "body { margin: 0 auto; padding: 2rem 1.25rem 4rem; max-width: 42rem;",
    " font: 16px/1.6 system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }",
    "h1 { font-size: 1.6rem; margin-bottom: 0.25rem; }",
    "h2 { font-size: 1.1rem; margin-top: 2rem; }",
    "p, li { margin: 0.6rem 0; }",
    ".updated { color: #6b7280; font-size: 0.9rem; margin-top: 0; }",
    "footer { margin-top: 3rem; font-size: 0.9rem; color: #6b7280; }"
);

const UPDATED: &str = "Last updated 10 September 2026.";

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        concat!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
            "<title>{title} — gmcp</title><style>{style}</style></head><body>",
            "{body}",
            "<footer>gmcp is a private tool. It is not a product and it is not ",
            "for sale.</footer></body></html>"
        ),
        title = title,
        style = STYLE,
        body = body
    );
    (
        [(header::CACHE_CONTROL, "public, max-age=3600")],
        Html(html),
    )
        .into_response()
}

pub async fn privacy() -> Response {
    let body = format!(
        concat!(
            "<h1>Privacy policy</h1>",
            "<p class=\"updated\">{updated}</p>",
            "<p>gmcp is a private tool run by one person for a small number of ",
            "invited people. It is not a public service. It has no customers, ",
            "no advertising and no analytics.</p>",
            "<h2>What it reaches</h2>",
            "<p>You connect a Google account to gmcp yourself. When you do, ",
            "Google asks you which permissions to grant. gmcp can then read ",
            "your Gmail messages, your Google Drive files, your Google Docs ",
            "documents, your Google Sheets spreadsheets and your Google ",
            "Calendar events, but only for the permissions you granted and ",
            "only for the accounts you connected.</p>",
            "<p>gmcp never sends email. It can write a draft, and you send it ",
            "yourself from Gmail. It never moves a message to the bin and it ",
            "never deletes one. It never invites anyone to a calendar event ",
            "and it never sends a notification about one.</p>",
            "<h2>What it stores</h2>",
            "<ul>",
            "<li>Your name and email address, from the sign-in provider.</li>",
            "<li>For each connected Google account: the Google address, the ",
            "label you chose, the permissions Google granted, and the refresh ",
            "token that lets gmcp act for you. The refresh token is encrypted ",
            "before it is written to disk.</li>",
            "<li>A log of every action taken through the tools: the time, the ",
            "account, the tool and its arguments. Message bodies and document ",
            "text are cut out of the log before it is written. The result of ",
            "an action is never logged.</li>",
            "</ul>",
            "<p>Your mail, your files and your calendar entries are not copied ",
            "into gmcp. They are read from Google when a tool asks for them ",
            "and they are not kept afterwards.</p>",
            "<h2>Who else sees it</h2>",
            "<p>Nobody. Your data is not sold, not shared and not sent to any ",
            "third party. It is not used to train any model. It stays on one ",
            "private server.</p>",
            "<p>When you ask a question through a chat client, that client ",
            "sends your question and the tool results to whichever language ",
            "model you chose. gmcp does not choose that model for you, and ",
            "this policy does not cover it.</p>",
            "<h2>How long it is kept</h2>",
            "<p>A connection is kept until you remove it. The action log is ",
            "kept until it is pruned, which happens on a schedule set by the ",
            "operator.</p>",
            "<h2>How to end it</h2>",
            "<p>Remove the connection in the gmcp portal. gmcp then asks ",
            "Google to revoke the grant and deletes the stored token. You can ",
            "also revoke the grant yourself at <a href=\"https://myaccount.",
            "google.com/permissions\">your Google account permissions page</a>, ",
            "which works even if the portal is unreachable.</p>",
            "<p>To have your account and its log removed, write to ",
            "{contact}.</p>",
            "<h2>Google user data</h2>",
            "<p>gmcp's use of information received from Google APIs follows ",
            "the <a href=\"https://developers.google.com/terms/",
            "api-services-user-data-policy\">Google API Services User Data ",
            "Policy</a>, including the Limited Use requirements.</p>"
        ),
        updated = UPDATED,
        contact = CONTACT
    );
    page("Privacy", &body)
}

pub async fn terms() -> Response {
    let body = format!(
        concat!(
            "<h1>Terms of service</h1>",
            "<p class=\"updated\">{updated}</p>",
            "<p>gmcp is a private tool run by one person. Access is by ",
            "invitation. There is no charge, and there is no contract.</p>",
            "<h2>What you agree to</h2>",
            "<ul>",
            "<li>You connect only Google accounts that you are allowed to ",
            "connect.</li>",
            "<li>You keep the access tokens you create private. A token acts ",
            "for you.</li>",
            "<li>You do not use gmcp to break the law or Google's own ",
            "terms.</li>",
            "</ul>",
            "<h2>What is not promised</h2>",
            "<p>gmcp is provided as it is, with no warranty of any kind. It ",
            "may be unavailable, it may lose data, and it may stop working ",
            "when Google changes an interface. A language model decides which ",
            "tools to call, and a model can be wrong. Read what it wrote ",
            "before you act on it, and read every draft before you send ",
            "it.</p>",
            "<p>The operator is not liable for any loss that follows from ",
            "using gmcp, as far as the law allows.</p>",
            "<h2>Ending access</h2>",
            "<p>You may remove your connections and stop using gmcp at any ",
            "time. The operator may withdraw access at any time, for any ",
            "reason, without notice.</p>",
            "<h2>Changes</h2>",
            "<p>These terms may change. The date above says when they last ",
            "did.</p>",
            "<p>Questions go to {contact}.</p>"
        ),
        updated = UPDATED,
        contact = CONTACT
    );
    page("Terms", &body)
}
