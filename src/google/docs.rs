//! Docs, the two edits this release makes: text appended at the end and a
//! find-and-replace across the document. Reads go through Drive's markdown
//! export, so `documents.get` is asked only for the end index an insert needs
//! and for the tab list the read tool reports alongside the text.
//!
//! Structural edits are deliberately out: a model that can only append and
//! replace cannot rearrange someone's document by accident.

use serde::{Deserialize, Serialize};

use super::client::{Client, Error, Result, urlencode};

/// Docs is served from its own host, never from `www.googleapis.com`.
const DOCS: &str = "docs";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Document {
    pub document_id: String,
    pub title: String,
    /// Where the body ends. An insert goes one before it, because the last
    /// index is the newline Docs keeps at the end of the body and nothing may
    /// be written after it.
    pub end_index: i64,
    pub tabs: Vec<Tab>,
}

impl Document {
    /// The index `insertText` may write at.
    pub fn append_index(&self) -> i64 {
        (self.end_index - 1).max(1)
    }

    pub fn url(&self) -> String {
        format!(
            "https://docs.google.com/document/d/{}/edit",
            self.document_id
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tab {
    pub id: String,
    pub title: String,
}

/// `documents.get`. The body content is asked for because the end index is
/// the last element's, and the tab list because the read tool reports it.
pub async fn get(client: &Client, connection_id: i64, document_id: &str) -> Result<Document> {
    let request = client
        .service(DOCS)
        .get(&format!("v1/documents/{}", urlencode(document_id)))?
        .query(&[("includeTabsContent", "false")]);
    let wire: WireDocument = client.json(connection_id, request).await?;
    let end_index = wire
        .body
        .content
        .iter()
        .filter_map(|e| e.end_index)
        .max()
        .unwrap_or(1);
    Ok(Document {
        document_id: wire.document_id,
        title: wire.title,
        end_index,
        tabs: flatten_tabs(&wire.tabs),
    })
}

/// Tabs nest; the list a person needs is flat and in reading order.
fn flatten_tabs(tabs: &[WireTab]) -> Vec<Tab> {
    let mut out = Vec::new();
    for tab in tabs {
        out.push(Tab {
            id: tab.tab_properties.tab_id.clone(),
            title: tab.tab_properties.title.clone(),
        });
        out.extend(flatten_tabs(&tab.child_tabs));
    }
    out
}

/// `documents.batchUpdate` with one `insertText` at the end. Plain text: Docs
/// takes the string as written, so markdown arrives as markdown characters
/// and the tool description says so.
pub async fn append_text(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    text: &str,
) -> Result<i64> {
    if text.is_empty() {
        return Err(Error::Unsupported(
            "there is nothing to append: the text is empty".into(),
        ));
    }
    let document = get(client, connection_id, document_id).await?;
    let index = document.append_index();
    let request = client
        .service(DOCS)
        .post(&format!(
            "v1/documents/{}:batchUpdate",
            urlencode(document_id)
        ))?
        .json(&BatchUpdate {
            requests: vec![DocRequest {
                insert_text: Some(InsertText {
                    text: text.to_string(),
                    location: Location { index },
                }),
                ..DocRequest::default()
            }],
        });
    let _: WireBatchReply = client.json(connection_id, request).await?;
    Ok(index)
}

/// `documents.batchUpdate` with `replaceAllText`; the answer is how many
/// occurrences were replaced, which is what the tool reports back.
pub async fn replace_all_text(
    client: &Client,
    connection_id: i64,
    document_id: &str,
    find: &str,
    replace: &str,
    match_case: bool,
) -> Result<i64> {
    if find.is_empty() {
        return Err(Error::Unsupported(
            "the text to find is empty; that would match everywhere".into(),
        ));
    }
    let request = client
        .service(DOCS)
        .post(&format!(
            "v1/documents/{}:batchUpdate",
            urlencode(document_id)
        ))?
        .json(&BatchUpdate {
            requests: vec![DocRequest {
                replace_all_text: Some(ReplaceAllText {
                    contains_text: SubstringMatch {
                        text: find.to_string(),
                        match_case,
                    },
                    replace_text: replace.to_string(),
                }),
                ..DocRequest::default()
            }],
        });
    let reply: WireBatchReply = client.json(connection_id, request).await?;
    Ok(reply
        .replies
        .iter()
        .filter_map(|r| r.replace_all_text.as_ref())
        .map(|r| r.occurrences_changed)
        .sum())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireDocument {
    document_id: String,
    title: String,
    body: WireBody,
    tabs: Vec<WireTab>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBody {
    content: Vec<WireElement>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireElement {
    end_index: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTab {
    tab_properties: WireTabProperties,
    child_tabs: Vec<WireTab>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireTabProperties {
    tab_id: String,
    title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireBatchReply {
    replies: Vec<WireReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReply {
    replace_all_text: Option<WireReplaceReply>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct WireReplaceReply {
    occurrences_changed: i64,
}

#[derive(Debug, Serialize)]
struct BatchUpdate {
    requests: Vec<DocRequest>,
}

/// One entry of a `batchUpdate`. Google's shape is an object with exactly
/// one of these keys set, which a struct with skipped `None`s expresses
/// directly; an enum would nest the payload one level too deep.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    insert_text: Option<InsertText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replace_all_text: Option<ReplaceAllText>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InsertText {
    text: String,
    location: Location,
}

#[derive(Debug, Serialize)]
struct Location {
    index: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceAllText {
    contains_text: SubstringMatch,
    replace_text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubstringMatch {
    text: String,
    match_case: bool,
}
