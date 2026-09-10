//! The public pages: what this app is, the privacy policy and the terms.
//! Plain HTML, no login, no JavaScript.
//!
//! Google checks all three before it will publish an app that asks for
//! sensitive scopes. It requires a home page that a signed-out visitor can
//! read, that says what the app does, that carries the same name as the
//! consent screen, and that links to the privacy policy. The portal itself
//! cannot be that page: it is a JavaScript bundle behind a login. So `/about`
//! is the home page, and it lives here rather than on a blog so that it
//! describes what this code actually does and changes when it does.

use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

/// Must match the app name on the Google consent screen exactly. Google
/// compares the two and refuses the app when they differ.
const APP_NAME: &str = "kq Google MCP";

/// Where a reader is told to write. The consent screen carries the same
/// address, so the two never disagree.
const CONTACT: &str = "the support address shown on the Google consent screen";

const STYLE: &str = concat!(
    ":root { color-scheme: light dark; }",
    "body { margin: 0 auto; padding: 2rem 1.25rem 4rem; max-width: 42rem;",
    " font: 16px/1.6 system-ui, -apple-system, Segoe UI, Roboto, sans-serif; }",
    "h1 { font-size: 1.6rem; margin-bottom: 0.25rem; }",
    "body > svg { width: 132px; height: auto; display: block; margin-bottom: 1rem; }",
    "h2 { font-size: 1.1rem; margin-top: 2rem; }",
    "p, li { margin: 0.6rem 0; }",
    ".updated { color: #6b7280; font-size: 0.9rem; margin-top: 0; }",
    "footer { margin-top: 3rem; font-size: 0.9rem; color: #6b7280; }"
);

/// The house mark, with the Q repainted in Google's four colours. Inline so
/// the page needs no second request and no asset pipeline.
const LOGO: &str = concat!(
    "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 -6 2481 1628\" fill-rule=\"evenodd",
    "\" clip-rule=\"evenodd\"><defs><linearGradient id=\"kqk\" gradientUnits=\"userSpaceOnUse",
    "\" gradientTransform=\"matrix(17.6586,61.4782,-61.4782,17.6586,1168.04,1339.44)\" x1=\"0",
    "\" y1=\"0\" x2=\"1\" y2=\"0\"><stop offset=\"0\" stop-color=\"#EF7625\"/><stop offset=\"",
    "1\" stop-color=\"#ED2222\"/></linearGradient><clipPath id=\"qt\"><path d=\"M1187.19,1376",
    ".08 L1045.77,1234.66 L1328.61,1234.66 Z\"/></clipPath><clipPath id=\"qr\"><path d=\"M118",
    "7.19,1376.08 L1328.61,1234.66 L1328.61,1517.50 Z\"/></clipPath><clipPath id=\"qb\"><path",
    " d=\"M1187.19,1376.08 L1328.61,1517.50 L1045.77,1517.50 Z\"/></clipPath><clipPath id=\"q",
    "l\"><path d=\"M1187.19,1376.08 L1045.77,1517.50 L1045.77,1234.66 Z\"/></clipPath></defs>",
    "<g transform=\"translate(0,-738.189)\"><g transform=\"matrix(20.8175,0,0,20.8175,-22865,",
    "-27101.3)\"><path d=\"m 1098.35,1337.31 h 12.35 v 63.61 h -12.35 z\" fill=\"url(#kqk)\"/",
    "><path d=\"m 1110.7,1369.12 27.36,-31.81 h 15.83 l -27.36,31.81 27.36,31.8 h -15.83 z\" ",
    "fill=\"url(#kqk)\"/></g><g transform=\"matrix(20.8175,0,0,20.8175,-22896.2,-27101.3)\"><",
    "g clip-path=\"url(#qt)\"><path d=\"M1155.39,1337.31L1219,1337.31L1219,1414.85L1187.19,14",
    "00.92L1155.39,1400.92L1155.39,1337.31ZM1167.73,1349.65L1206.66,1349.65L1206.66,1397.11L1",
    "187.19,1388.59L1167.73,1388.59L1167.73,1349.65Z\" fill=\"#4285F4\"/></g><g clip-path=\"u",
    "rl(#qr)\"><path d=\"M1155.39,1337.31L1219,1337.31L1219,1414.85L1187.19,1400.92L1155.39,1",
    "400.92L1155.39,1337.31ZM1167.73,1349.65L1206.66,1349.65L1206.66,1397.11L1187.19,1388.59L",
    "1167.73,1388.59L1167.73,1349.65Z\" fill=\"#EA4335\"/></g><g clip-path=\"url(#qb)\"><path",
    " d=\"M1155.39,1337.31L1219,1337.31L1219,1414.85L1187.19,1400.92L1155.39,1400.92L1155.39,",
    "1337.31ZM1167.73,1349.65L1206.66,1349.65L1206.66,1397.11L1187.19,1388.59L1167.73,1388.59",
    "L1167.73,1349.65Z\" fill=\"#FBBC05\"/></g><g clip-path=\"url(#ql)\"><path d=\"M1155.39,1",
    "337.31L1219,1337.31L1219,1414.85L1187.19,1400.92L1155.39,1400.92L1155.39,1337.31ZM1167.7",
    "3,1349.65L1206.66,1349.65L1206.66,1397.11L1187.19,1388.59L1167.73,1388.59L1167.73,1349.6",
    "5Z\" fill=\"#34A853\"/></g></g></g></svg>",
);

const UPDATED: &str = "Last updated 10 September 2026.";

fn page(title: &str, body: &str) -> Response {
    let html = format!(
        concat!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">",
            "<title>{title} — {app}</title><style>{style}</style></head><body>",
            "{body}",
            "<footer>{app} is a private tool. It is not a product and it is not ",
            "for sale.</footer></body></html>"
        ),
        title = title,
        app = APP_NAME,
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
            "<p>{app} is a private tool run by one person for a small number of ",
            "invited people. It is not a public service. It has no customers, ",
            "no advertising and no analytics.</p>",
            "<h2>What it reaches</h2>",
            "<p>You connect a Google account to {app} yourself. When you do, ",
            "Google asks you which permissions to grant. {app} can then read ",
            "your Gmail messages, your Google Drive files, your Google Docs ",
            "documents, your Google Sheets spreadsheets and your Google ",
            "Calendar events, but only for the permissions you granted and ",
            "only for the accounts you connected.</p>",
            "<p>{app} never sends email. It can write a draft, and you send it ",
            "yourself from Gmail. It never moves a message to the bin and it ",
            "never deletes one. It never invites anyone to a calendar event ",
            "and it never sends a notification about one.</p>",
            "<h2>What it stores</h2>",
            "<ul>",
            "<li>Your name and email address, from the sign-in provider.</li>",
            "<li>For each connected Google account: the Google address, the ",
            "label you chose, the permissions Google granted, and the refresh ",
            "token that lets {app} act for you. The refresh token is encrypted ",
            "before it is written to disk.</li>",
            "<li>A log of every action taken through the tools: the time, the ",
            "account, the tool and its arguments. Message bodies and document ",
            "text are cut out of the log before it is written. The result of ",
            "an action is never logged.</li>",
            "</ul>",
            "<p>Your mail, your files and your calendar entries are not copied ",
            "into {app}. They are read from Google when a tool asks for them ",
            "and they are not kept afterwards.</p>",
            "<h2>Who else sees it</h2>",
            "<p>Nobody. Your data is not sold, not shared and not sent to any ",
            "third party. It is not used to train any model. It stays on one ",
            "private server.</p>",
            "<p>When you ask a question through a chat client, that client ",
            "sends your question and the tool results to whichever language ",
            "model you chose. {app} does not choose that model for you, and ",
            "this policy does not cover it.</p>",
            "<h2>How long it is kept</h2>",
            "<p>A connection is kept until you remove it. The action log is ",
            "kept until it is pruned, which happens on a schedule set by the ",
            "operator.</p>",
            "<h2>How to end it</h2>",
            "<p>Remove the connection in the {app} portal. It then asks ",
            "Google to revoke the grant and deletes the stored token. You can ",
            "also revoke the grant yourself at <a href=\"https://myaccount.",
            "google.com/permissions\">your Google account permissions page</a>, ",
            "which works even if the portal is unreachable.</p>",
            "<p>To have your account and its log removed, write to ",
            "{contact}.</p>",
            "<h2>Google user data</h2>",
            "<p>{app}'s use of information received from Google APIs follows ",
            "the <a href=\"https://developers.google.com/terms/",
            "api-services-user-data-policy\">Google API Services User Data ",
            "Policy</a>, including the Limited Use requirements.</p>"
        ),
        updated = UPDATED,
        app = APP_NAME,
        contact = CONTACT
    );
    page("Privacy", &body)
}

pub async fn terms() -> Response {
    let body = format!(
        concat!(
            "<h1>Terms of service</h1>",
            "<p class=\"updated\">{updated}</p>",
            "<p>{app} is a private tool run by one person. Access is by ",
            "invitation. There is no charge, and there is no contract.</p>",
            "<h2>What you agree to</h2>",
            "<ul>",
            "<li>You connect only Google accounts that you are allowed to ",
            "connect.</li>",
            "<li>You keep the access tokens you create private. A token acts ",
            "for you.</li>",
            "<li>You do not use {app} to break the law or Google's own ",
            "terms.</li>",
            "</ul>",
            "<h2>What is not promised</h2>",
            "<p>{app} is provided as it is, with no warranty of any kind. It ",
            "may be unavailable, it may lose data, and it may stop working ",
            "when Google changes an interface. A language model decides which ",
            "tools to call, and a model can be wrong. Read what it wrote ",
            "before you act on it, and read every draft before you send ",
            "it.</p>",
            "<p>The operator is not liable for any loss that follows from ",
            "using {app}, as far as the law allows.</p>",
            "<h2>Ending access</h2>",
            "<p>You may remove your connections and stop using {app} at any ",
            "time. The operator may withdraw access at any time, for any ",
            "reason, without notice.</p>",
            "<h2>Changes</h2>",
            "<p>These terms may change. The date above says when they last ",
            "did.</p>",
            "<p>Questions go to {contact}.</p>"
        ),
        updated = UPDATED,
        app = APP_NAME,
        contact = CONTACT
    );
    page("Terms", &body)
}

/// The home page Google's branding check reads: reachable without a session,
/// named exactly as the consent screen names it, and linking to both of the
/// documents below.
pub async fn about() -> Response {
    let body = format!(
        concat!(
            "{logo}<h1>{app}</h1>",
            "<p class=\"updated\">A private tool for connecting Google ",
            "accounts to chat assistants.</p>",
            "<h2>What it does</h2>",
            "<p>{app} lets a small number of invited people give a chat ",
            "assistant careful access to their own Google accounts. Once you ",
            "connect an account, an assistant can search your Gmail, read a ",
            "thread, look at an attachment, open a Google Docs document or a ",
            "Google Sheets spreadsheet, and read or add Google Calendar ",
            "events. You decide which of those it may do, one permission at a ",
            "time, and you can connect several Google accounts and keep them ",
            "apart.</p>",
            "<h2>What it will not do</h2>",
            "<p>{app} never sends email. It writes drafts, and you send them ",
            "yourself from Gmail. It never moves a message to the bin and ",
            "never deletes one. It never invites anyone to a calendar event ",
            "and never sends a notification about one. Google has no ",
            "permission that means \"drafts but never send\", so this app is ",
            "the thing that draws that line.</p>",
            "<h2>Who it is for</h2>",
            "<p>This is not a public service and there is nothing to sign up ",
            "for. It runs on one private server for its operator and a few ",
            "invited people. Access is by invitation, and there is no ",
            "charge.</p>",
            "<h2>Your data</h2>",
            "<p>Your mail, files and calendar entries are never copied into ",
            "{app}. They are read from Google when you ask for them and are ",
            "not kept afterwards. Nothing is shared with anyone and nothing ",
            "trains a model. The <a href=\"/privacy\">privacy policy</a> says ",
            "exactly what is stored and for how long, and the ",
            "<a href=\"/terms\">terms of service</a> say what is and is not ",
            "promised.</p>",
            "<p>You can withdraw access at any time, either in this portal or ",
            "at <a href=\"https://myaccount.google.com/permissions\">your ",
            "Google account permissions page</a>.</p>",
            "<h2>Already invited?</h2>",
            "<p><a href=\"/\">Sign in to the portal</a>. You will need an ",
            "account on the operator's identity provider; there is no ",
            "registration here.</p>",
            "<p>Questions go to {contact}.</p>"
        ),
        app = APP_NAME,
        logo = LOGO,
        contact = CONTACT
    );
    page("About", &body)
}
