//! Download links: minting them, and the unauthenticated `/dl/{id}` route
//! that spends one.
//!
//! A file never leaves through MCP content. A tool mints a link once its own
//! scope and connection checks have passed, the person clicks it or the agent
//! curls it, and the server streams from Google on each hit and stores
//! nothing. The id is the whole capability, so the route needs no auth and the
//! id is unguessable; the life and the use cap are in `domain::limits`.
//!
//! The three ways a hit can be refused — unknown, expired, spent — answer the
//! same 404 with the same body, so an id cannot be probed for its state. The
//! log records which of the three it was.

use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use super::AppState;
use super::audit;
use super::error::{ApiError, ApiResult};
use crate::db::{AuditKind, AuditOutcome, Link, LinkKind, LinkRefusal, NewAuditEntry, NewLink};
use crate::domain::limits::DOWNLOAD_MAX_BYTES;
use crate::domain::link;
use crate::google::drive::ExportFormat;
use crate::google::{docs, drive, gmail};

/// Where a download link points. This is the `target` column, typed; the JSON
/// shape is this module's business and nothing else reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    GmailAttachment {
        message_id: String,
        attachment_id: String,
    },
    DriveDownload {
        file_id: String,
    },
    DriveExport {
        file_id: String,
        format: ExportFormat,
    },
    /// A picture inside a Google Doc, named by the document and Docs' own id
    /// for the object. The URL Google serves the picture from is deliberately
    /// not stored: it expires in about half an hour, which is sooner than a
    /// link may be spent, so the route asks the document for a fresh one on
    /// every hit.
    DocsImage {
        doc_id: String,
        object_id: String,
    },
}

impl Target {
    pub fn kind(&self) -> LinkKind {
        match self {
            Self::GmailAttachment { .. } => LinkKind::GmailAttachment,
            Self::DriveDownload { .. } => LinkKind::DriveDownload,
            Self::DriveExport { .. } => LinkKind::DriveExport,
            Self::DocsImage { .. } => LinkKind::DocsImage,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::GmailAttachment {
                message_id,
                attachment_id,
            } => json!({ "message_id": message_id, "attachment_id": attachment_id }),
            Self::DriveDownload { file_id } => json!({ "file_id": file_id }),
            Self::DriveExport { file_id, format } => {
                json!({ "file_id": file_id, "format": format.as_str() })
            }
            Self::DocsImage { doc_id, object_id } => {
                json!({ "doc_id": doc_id, "object_id": object_id })
            }
        }
    }

    /// The inverse, for a row that was written by an older process. A row this
    /// cannot read is a server error and not a refusal: the link was valid.
    fn from_row(link: &Link) -> Result<Self, String> {
        let field = |name: &str| {
            link.target
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("link {} has no {name}", link.id))
        };
        match link.kind {
            LinkKind::GmailAttachment => Ok(Self::GmailAttachment {
                message_id: field("message_id")?,
                attachment_id: field("attachment_id")?,
            }),
            LinkKind::DriveDownload => Ok(Self::DriveDownload {
                file_id: field("file_id")?,
            }),
            LinkKind::DriveExport => Ok(Self::DriveExport {
                file_id: field("file_id")?,
                format: field("format")?
                    .parse()
                    .map_err(|e: drive::ExportFormatError| e.to_string())?,
            }),
            LinkKind::DocsImage => Ok(Self::DocsImage {
                doc_id: field("doc_id")?,
                object_id: field("object_id")?,
            }),
        }
    }
}

/// What a tool knows when it mints a link.
#[derive(Debug, Clone)]
pub struct NewDownload {
    pub connection_id: i64,
    /// The token whose call minted it; the log ties the hit back to it.
    pub token_id: i64,
    pub target: Target,
    pub filename: String,
    pub mime_type: String,
    /// When Google said how big it is; the route sends it as `Content-Length`.
    pub size: Option<i64>,
}

/// A minted link, as a tool result reports it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Minted {
    pub url: String,
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: Option<i64>,
    pub expires_at: DateTime<Utc>,
}

/// Mint a link and log it. The caller has already checked that the principal
/// may reach the connection; this only writes the capability down.
///
/// Expired rows are swept on the way past: minting is the one moment there is
/// certainly a writer, and it keeps the table from being a job's problem.
pub async fn mint(state: &AppState, user_id: i64, new: NewDownload) -> ApiResult<Minted> {
    let now = Utc::now();
    // Google already said how big it is, so the refusal belongs here, in the
    // tool call the model is waiting on, rather than in a link that turns out
    // to be a 404 with a stranger's ip in the log.
    if let Some(size) = new.size
        && link::too_large(size.max(0) as u64)
    {
        return Err(ApiError::bad_request(format!(
            "{} is {} MB, over the {} MB download cap",
            new.filename,
            size / (1024 * 1024),
            DOWNLOAD_MAX_BYTES / (1024 * 1024)
        )));
    }
    if let Err(e) = state.db.delete_expired_links(now).await {
        tracing::warn!("sweeping expired links: {e}");
    }
    let row = state
        .db
        .create_link(NewLink {
            id: link::new_id(),
            connection_id: new.connection_id,
            token_id: new.token_id,
            kind: new.target.kind(),
            target: new.target.to_json(),
            filename: new.filename,
            mime_type: link::safe_mime(&new.mime_type),
            size: new.size,
            expires_at: link::expires_at(now),
            uses_left: link::uses(),
        })
        .await?;
    audit::record(
        &state.db,
        NewAuditEntry {
            user_id: Some(user_id),
            token_id: Some(row.token_id),
            connection_id: Some(row.connection_id),
            detail: Some(format!("{} ({})", row.filename, row.id)),
            ..NewAuditEntry::new(now, AuditKind::LinkCreated, AuditOutcome::Ok)
        },
    )
    .await;
    Ok(Minted {
        url: url_of(state, &row.id),
        id: row.id,
        filename: row.filename,
        mime_type: row.mime_type,
        size: row.size,
        expires_at: row.expires_at,
    })
}

/// The public URL of a link. Built from `GMCP_PUBLIC_URL` and never from a
/// request header, so a forwarded `Host` cannot move where people are sent.
fn url_of(state: &AppState, id: &str) -> String {
    match state.config.public_url.join(&format!("/dl/{id}")) {
        Ok(url) => url.to_string(),
        Err(e) => {
            tracing::error!("GMCP_PUBLIC_URL cannot carry a download path: {e}");
            format!("/dl/{id}")
        }
    }
}

/// Spend one use of a link and stream the bytes. Unauthenticated by design:
/// knowing the id is the permission.
#[utoipa::path(get, path = "/dl/{id}", tag = "links",
    params(("id" = String, Path, description = "the link id")),
    responses(
        (status = 200, description = "the file, as an attachment"),
        (status = 404, body = super::error::ErrorBody, description = "unknown, expired or spent"),
    ))]
pub async fn download(
    State(state): State<AppState>,
    Path(id): Path<String>,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
) -> Response {
    // The peer address is there when the server put it there — `serve` does —
    // and absent in an in-process test call, where there is no socket.
    let peer = extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let ip = client_ip(peer, &headers);
    let now = Utc::now();
    let taken = match state.db.take_link(&id, now).await {
        Ok(taken) => taken,
        Err(e) => return ApiError::from(e).into_response(),
    };
    let link = match taken {
        Ok(link) => link,
        Err(refusal) => {
            refused(&state, &id, refusal, ip).await;
            return ApiError::not_found().into_response();
        }
    };
    let user_id = owner(&state, link.connection_id).await;
    match stream(&state, &link, user_id, ip.clone()).await {
        Ok(response) => response,
        Err(e) => {
            audit::record(
                &state.db,
                NewAuditEntry {
                    user_id,
                    token_id: Some(link.token_id),
                    connection_id: Some(link.connection_id),
                    detail: Some(format!("{}: {}", link.filename, e.message)),
                    ip,
                    ..NewAuditEntry::new(now, AuditKind::LinkUsed, AuditOutcome::Error)
                },
            )
            .await;
            e.into_response()
        }
    }
}

/// Whose link this is. The route has no principal — the id is the permission —
/// but the row does: it names a connection, and a connection belongs to a
/// person. Without this the hit is logged against nobody and never appears on
/// the Activity page of the one person who wanted to see it.
async fn owner(state: &AppState, connection_id: i64) -> Option<i64> {
    match state.db.get_connection(connection_id).await {
        Ok(connection) => Some(connection.user_id),
        Err(e) => {
            tracing::warn!("connection {connection_id} behind a link: {e}");
            None
        }
    }
}

/// A refused hit is logged with the reason and the ip, and answered with the
/// same 404 as the other two so the id cannot be probed. An id that exists is
/// logged against its owner, so an expired link someone kept clicking is on
/// their page; an id that was never minted belongs to nobody.
async fn refused(state: &AppState, id: &str, refusal: LinkRefusal, ip: Option<String>) {
    let link = state.db.get_link(id).await.unwrap_or_else(|e| {
        tracing::warn!("reading the refused link {id}: {e}");
        None
    });
    let (user_id, token_id, connection_id) = match &link {
        Some(link) => (
            owner(state, link.connection_id).await,
            Some(link.token_id),
            Some(link.connection_id),
        ),
        None => (None, None, None),
    };
    audit::record(
        &state.db,
        NewAuditEntry {
            user_id,
            token_id,
            connection_id,
            detail: Some(format!("{id}: {refusal}")),
            ip,
            ..NewAuditEntry::new(Utc::now(), AuditKind::LinkRefused, AuditOutcome::Forbidden)
        },
    )
    .await;
}

async fn stream(
    state: &AppState,
    link: &Link,
    user_id: Option<i64>,
    ip: Option<String>,
) -> ApiResult<Response> {
    let google = state.google().ok_or_else(ApiError::google_unconfigured)?;
    let target = Target::from_row(link).map_err(ApiError::internal)?;
    let (body, length) = match target {
        // Gmail hands attachments back base64 inside JSON, so there is nothing
        // to stream: the bytes are whole before the first one is sent.
        Target::GmailAttachment {
            message_id,
            attachment_id,
        } => {
            let bytes = gmail::get_attachment(
                &google.client,
                link.connection_id,
                &message_id,
                &attachment_id,
            )
            .await?;
            let length = bytes.len() as u64;
            (Body::from(bytes), Some(length))
        }
        Target::DriveDownload { file_id } => {
            let file = drive::download(&google.client, link.connection_id, &file_id).await?;
            streamed(file)?
        }
        Target::DriveExport { file_id, format } => {
            let file = drive::export(&google.client, link.connection_id, &file_id, format).await?;
            streamed(file)?
        }
        // The picture is found again on every hit. Google's `contentUri` lives
        // about half an hour, a link lives fifteen minutes and may be spent
        // later than it was minted, and a document is edited in between: the
        // object id is what stays true, so the current URL is read out of the
        // document each time rather than stored with the link.
        Target::DocsImage { doc_id, object_id } => {
            let held = docs::images(&google.client, link.connection_id, &doc_id).await?;
            let picture = docs::open_image(&google.client, held.find(&object_id)?).await?;
            streamed(picture)?
        }
    };
    if let Err(e) = state.db.touch_connection_used(link.connection_id).await {
        tracing::warn!("connection {}: {e}", link.connection_id);
    }
    audit::record(
        &state.db,
        NewAuditEntry {
            user_id,
            token_id: Some(link.token_id),
            connection_id: Some(link.connection_id),
            detail: Some(format!("{} ({})", link.filename, link.id)),
            ip,
            ..NewAuditEntry::new(Utc::now(), AuditKind::LinkUsed, AuditOutcome::Ok)
        },
    )
    .await;
    let mut response = Response::builder()
        .status(StatusCode::OK)
        // The stored type came from a mail header or an uploader, so it is
        // normalised again here: rows written before it was normalised at mint
        // are still out there, and a browser must not sniff its way past it.
        .header(header::CONTENT_TYPE, link::safe_mime(&link.mime_type))
        .header(header::CONTENT_DISPOSITION, disposition(&link.filename))
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    // Only what this response actually carries. The size recorded at mint
    // describes a file Google may have changed since, and a Content-Length
    // that disagrees with the body truncates the download.
    if let Some(length) = length {
        response = response.header(header::CONTENT_LENGTH, length);
    }
    response
        .body(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, "google", e.to_string()))
}

/// A Drive response, checked against the download cap before a byte of it is
/// forwarded. `into_stream` enforces the cap as the bytes go past as well, for
/// a response that lied about its length.
fn streamed(file: crate::google::client::Download) -> ApiResult<(Body, Option<u64>)> {
    if file.size().is_some_and(link::too_large) {
        return Err(ApiError::from(crate::google::Error::TooLarge));
    }
    let length = file.size();
    Ok((Body::from_stream(file.into_stream()), length))
}

/// Everything that is not an unreserved character is percent-encoded, which is
/// always allowed inside an RFC 5987 `ext-value` and saves reasoning about
/// quoting rules per filename.
const FILENAME: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn disposition(filename: &str) -> String {
    let name = if filename.trim().is_empty() {
        "download"
    } else {
        filename
    };
    format!(
        "attachment; filename*=UTF-8''{}",
        utf8_percent_encode(name, FILENAME)
    )
}

/// Where a hit came from.
///
/// The shipped topology is one container whose port is published on the
/// host's loopback only, with apache in front of it: nothing outside the host
/// can open a socket to this server, and the peer address inside the container
/// is apache seen through the Docker bridge — a private address like
/// `172.18.0.1`, not loopback. So `X-Forwarded-For` is believed — its first
/// value, the original client — when the peer is loopback or a private
/// address, and only apache can be either. A public peer address means
/// something else is talking to this process directly, and then the header is
/// a claim by whoever connected and the socket address is the truth.
pub fn client_ip(peer: Option<SocketAddr>, headers: &HeaderMap) -> Option<String> {
    let peer = peer?.ip();
    if from_the_proxy(peer)
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        && forwarded.parse::<IpAddr>().is_ok()
    {
        return Some(forwarded.to_string());
    }
    Some(peer.to_string())
}

/// Whether an address can only be the house proxy: loopback, an RFC 1918
/// private range, or a link-local one. The container port is published on
/// loopback, so nothing else can reach this server from a private address.
fn from_the_proxy(peer: IpAddr) -> bool {
    match peer {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            // fc00::/7 (unique local) and fe80::/10 (link local), spelled out
            // rather than taken from std so this compiles on any toolchain.
            let first = ip.octets()[0];
            let second = ip.octets()[1];
            ip.is_loopback() || first & 0xfe == 0xfc || (first == 0xfe && second & 0xc0 == 0x80)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The topology this assumes: the container's port published on the
    /// host's loopback with apache in front, so the peer is either loopback
    /// (apache on the host itself) or the Docker bridge gateway, which is a
    /// private address. Nothing else can reach the socket at all.
    #[test]
    fn a_forwarded_address_is_believed_from_loopback_or_the_docker_bridge() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        let local: SocketAddr = "127.0.0.1:44444".parse().unwrap();
        let bridge: SocketAddr = "172.18.0.1:44444".parse().unwrap();
        let remote: SocketAddr = "198.51.100.9:44444".parse().unwrap();
        assert_eq!(
            client_ip(Some(local), &headers).as_deref(),
            Some("203.0.113.7")
        );
        assert_eq!(
            client_ip(Some(bridge), &headers).as_deref(),
            Some("203.0.113.7"),
            "in the container the peer is the bridge gateway, not loopback"
        );
        assert_eq!(
            client_ip(Some(remote), &headers).as_deref(),
            Some("198.51.100.9")
        );
        // Nonsense in the header falls back to the socket.
        let mut junk = HeaderMap::new();
        junk.insert("x-forwarded-for", "not-an-address".parse().unwrap());
        assert_eq!(client_ip(Some(local), &junk).as_deref(), Some("127.0.0.1"));
        assert_eq!(
            client_ip(Some(local), &HeaderMap::new()).as_deref(),
            Some("127.0.0.1")
        );
        // No socket address at all (an in-process test call) is no ip.
        assert_eq!(client_ip(None, &headers), None);
    }

    #[test]
    fn the_filename_survives_a_space_and_a_quote_and_polish() {
        assert_eq!(
            disposition("zażółć gęślą.pdf"),
            "attachment; filename*=UTF-8''za%C5%BC%C3%B3%C5%82%C4%87%20g%C4%99%C5%9Bl%C4%85.pdf"
        );
        assert_eq!(
            disposition("re\"port\".csv"),
            "attachment; filename*=UTF-8''re%22port%22.csv"
        );
        assert_eq!(disposition("  "), "attachment; filename*=UTF-8''download");
    }

    #[test]
    fn a_target_round_trips_through_the_row_it_is_stored_in() {
        for target in [
            Target::GmailAttachment {
                message_id: "18f".into(),
                attachment_id: "ANGjdJ".into(),
            },
            Target::DriveDownload {
                file_id: "1AbC".into(),
            },
            Target::DriveExport {
                file_id: "1AbC".into(),
                format: ExportFormat::Pdf,
            },
            Target::DocsImage {
                doc_id: "1PiC".into(),
                object_id: "kix.chart".into(),
            },
        ] {
            let row = Link {
                id: "x".into(),
                connection_id: 1,
                token_id: 1,
                kind: target.kind(),
                target: target.to_json(),
                filename: "f".into(),
                mime_type: "application/pdf".into(),
                size: None,
                expires_at: Utc::now(),
                uses_left: 3,
                created_at: Utc::now(),
            };
            assert_eq!(Target::from_row(&row).unwrap(), target);
        }
    }
}
