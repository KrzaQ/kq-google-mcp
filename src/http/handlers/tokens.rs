//! Bearer tokens for MCP clients. Managing them is session-only: a token must
//! never mint or revoke a token.
//!
//! Two kinds exist. A personal token belongs to a person and acts as them,
//! reaching either every connection they have or a named allowlist. A delegate
//! token belongs to nobody: it is the gateway's token, names the acting person
//! in `X-Gmcp-User` on every call, and reaches whatever that person has
//! flagged for the gateway — so it has no user and no allowlist of its own.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use super::dto::{TokenCreated, TokenDto, TokenInput};
use crate::db::{ApiToken, AuditKind, AuditOutcome, ClientProfile, NewAuditEntry, NewToken, User};
use crate::domain::scope::Scope;
use crate::domain::token;
use crate::http::auth::Principal;
use crate::http::error::{ApiError, ApiResult, ErrorBody};
use crate::http::{AppState, audit};

#[utoipa::path(get, path = "/api/tokens", tag = "tokens",
    responses((status = 200, body = Vec<TokenDto>), (status = 401, body = ErrorBody), (status = 403, body = ErrorBody)))]
pub async fn list_tokens(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<TokenDto>>> {
    let me = p.require_session()?;
    let mut out = Vec::new();
    for t in state.db.list_user_tokens(me.id).await? {
        let ids = state.db.token_connections(t.id).await?;
        out.push(TokenDto::from(t, ids));
    }
    Ok(Json(out))
}

#[utoipa::path(post, path = "/api/tokens", tag = "tokens", request_body = TokenInput,
    responses((status = 201, body = TokenCreated), (status = 400, body = ErrorBody), (status = 403, body = ErrorBody)))]
pub async fn create_token(
    State(state): State<AppState>,
    p: Principal,
    Json(input): Json<TokenInput>,
) -> ApiResult<(StatusCode, Json<TokenCreated>)> {
    let me = p.require_session()?;
    let client: ClientProfile = match &input.client {
        Some(c) => c.parse().map_err(ApiError::bad_request)?,
        None => ClientProfile::Generic,
    };
    // The same rules the CLI applies, in the same words: the registry is the
    // authority on the scopes, a delegate token takes no allowlist, and a
    // personal token has to reach something. The person asking is the person
    // a personal token acts as, so nothing here names a user.
    let valid = token::validate(token::Request {
        name: &input.name,
        scopes: &input.scopes,
        owner: token::Owner::Caller,
        all_connections: input.all_connections,
        connections: input.connection_ids.len(),
    })
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let scopes = valid.scopes;
    let user_id = if valid.delegate {
        None
    } else {
        check_allowlist(&state, me, &input.connection_ids).await?;
        Some(me.id)
    };
    let secret = token::generate();
    let created = state
        .db
        .create_token(NewToken {
            name: valid.name.to_string(),
            token_hash: secret.hash,
            scopes: scopes.iter().map(ToString::to_string).collect(),
            client,
            user_id,
            all_connections: input.all_connections,
            created_by: me.id,
        })
        .await?;
    if !input.connection_ids.is_empty() {
        state
            .db
            .set_token_connections(created.id, &input.connection_ids)
            .await?;
    }
    log(&state, me, &created, AuditKind::TokenCreated, &scopes).await;
    Ok((
        StatusCode::CREATED,
        Json(TokenCreated {
            token: TokenDto::from(created, input.connection_ids),
            secret: secret.secret,
        }),
    ))
}

#[utoipa::path(delete, path = "/api/tokens/{id}", tag = "tokens", params(("id" = i64, Path)),
    responses((status = 204), (status = 404, body = ErrorBody)))]
pub async fn revoke_token(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let me = p.require_session()?;
    let t = state.db.get_token(id).await?;
    // Someone else's token is not found rather than forbidden: whether it
    // exists is not their business.
    if t.user_id != Some(me.id) && t.created_by != me.id {
        return Err(ApiError::not_found());
    }
    state.db.revoke_token(id).await?;
    let scopes: Vec<Scope> = t.scopes.iter().filter_map(|s| s.parse().ok()).collect();
    log(&state, me, &t, AuditKind::TokenRevoked, &scopes).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Every id on a personal token's allowlist must be a connection of the person
/// the token acts as; someone else's is refused rather than silently dropped.
/// That there is an allowlist at all is [`token::validate`]'s business.
async fn check_allowlist(state: &AppState, me: &User, ids: &[i64]) -> ApiResult<()> {
    let mine = state.db.list_connections(me.id).await?;
    for id in ids {
        if !mine.iter().any(|c| c.id == *id) {
            return Err(ApiError::bad_request(format!(
                "connection {id} is not one of yours"
            )));
        }
    }
    Ok(())
}

async fn log(state: &AppState, me: &User, t: &ApiToken, kind: AuditKind, scopes: &[Scope]) {
    audit::record(
        &state.db,
        NewAuditEntry {
            token_id: Some(t.id),
            args: audit::args(serde_json::json!({
                "name": t.name,
                "client": t.client.as_str(),
                "scopes": scopes.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "all_connections": t.all_connections,
            })),
            detail: Some(t.name.clone()),
            ..audit::by(me, kind, AuditOutcome::Ok)
        },
    )
    .await;
}
