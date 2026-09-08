//! `/api/scopes`: the registry, as JSON. The token grid in the UI is drawn
//! from this rather than from a copy of the matrix in TypeScript, so adding a
//! service stays one code change on this side.

use axum::Json;

use super::dto::{ScopeDto, ScopeRegistry, ServiceDto};
use crate::db::ClientProfile;
use crate::domain::scope::{Scope, Service, tools_of_scope};
use crate::domain::token::SCOPE_DELEGATE;
use crate::http::auth::Principal;
use crate::http::error::{ApiResult, ErrorBody};

#[utoipa::path(get, path = "/api/scopes", tag = "tokens",
    responses((status = 200, body = ScopeRegistry), (status = 401, body = ErrorBody)))]
pub async fn scopes(p: Principal) -> ApiResult<Json<ScopeRegistry>> {
    p.require_session()?;
    let services = Service::ALL
        .into_iter()
        .map(|service| ServiceDto {
            service: service.to_string(),
            google_scopes: service
                .google_scopes()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            levels: service
                .levels()
                .iter()
                .map(|level| {
                    let scope = Scope::Service(service, *level);
                    ScopeDto {
                        scope: scope.to_string(),
                        level: Some(level.to_string()),
                        requires: scope.requires().map(|r| r.to_string()),
                        tools: tools_of_scope(scope)
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                    }
                })
                .collect(),
        })
        .collect();
    Ok(Json(ScopeRegistry {
        services,
        delegate: ScopeDto {
            scope: SCOPE_DELEGATE.to_string(),
            level: None,
            requires: None,
            // `delegate` unlocks no tool of its own: it changes who the token
            // acts for, not what it may do.
            tools: Vec::new(),
        },
        clients: ClientProfile::ALL.iter().map(|c| c.to_string()).collect(),
    }))
}
