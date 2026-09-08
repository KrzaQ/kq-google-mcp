//! `/api/audit`: the activity page. Newest first, filtered, and paged by the
//! `(at, id)` cursor the database layer defines — two rows can share an
//! instant, so an instant alone is not a place in the log.
//!
//! A person sees their own rows and nobody else's. There is no person
//! switcher in this portal and no administrator view.

use std::str::FromStr;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use utoipa::IntoParams;

use super::dto::{AuditDto, AuditPage};
use crate::db::{AuditCursor, AuditFilter, AuditKind};
use crate::http::AppState;
use crate::http::auth::Principal;
use crate::http::error::{ApiError, ApiResult, ErrorBody};

/// Rows per page when the caller does not say; the database caps it at 500.
const DEFAULT_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct AuditQuery {
    /// Not before this instant, RFC 3339.
    pub from: Option<DateTime<Utc>>,
    /// Not after this instant, RFC 3339.
    pub to: Option<DateTime<Utc>>,
    pub connection: Option<i64>,
    pub token: Option<i64>,
    /// One of the audit kinds, e.g. `tool_call` or `link_used`.
    pub kind: Option<String>,
    /// An MCP tool name.
    pub tool: Option<String>,
    pub limit: Option<i64>,
    /// The `next` of the previous page: `<instant>,<id>`.
    pub before: Option<String>,
}

#[utoipa::path(get, path = "/api/audit", tag = "audit", params(AuditQuery),
    responses((status = 200, body = AuditPage), (status = 400, body = ErrorBody), (status = 401, body = ErrorBody)))]
pub async fn list_audit(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<AuditPage>> {
    let me = p.require_session()?;
    let kind = match &q.kind {
        Some(k) => Some(AuditKind::from_str(k).map_err(ApiError::bad_request)?),
        None => None,
    };
    let before = match &q.before {
        Some(c) => Some(AuditCursor::from_str(c).map_err(ApiError::bad_request)?),
        None => None,
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    let rows = state
        .db
        .list_audit(AuditFilter {
            user_id: Some(me.id),
            token_id: q.token,
            connection_id: q.connection,
            kind,
            tool: q.tool.clone(),
            from: q.from,
            to: q.to,
            before,
            limit: Some(limit),
        })
        .await?;
    // A full page may have more behind it; a short one is the end.
    let next = rows
        .last()
        .filter(|_| rows.len() as i64 >= limit)
        .map(|last| last.cursor().to_string());
    Ok(Json(AuditPage {
        entries: rows.into_iter().map(AuditDto::from).collect(),
        next,
    }))
}
