use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::web::{middleware::AuthScope, require_scope, WebState};

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SkillStatus {
    name: String,
    description: String,
    enabled: bool,
    source: String,
    version: Option<String>,
    platforms: Vec<String>,
    reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SkillsListResponse {
    ok: bool,
    skills: Vec<SkillStatus>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SkillToggleResponse {
    ok: bool,
    message: String,
}

#[utoipa::path(
    get,
    path = "/api/skills",
    operation_id = "skills_list",
    tag = "skills",
    responses(
        (status = 200, description = "Discovered skills with availability status", body = SkillsListResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Read)"),
    ),
)]
pub(super) async fn api_list_skills(
    headers: HeaderMap,
    State(state): State<WebState>,
) -> Result<Json<SkillsListResponse>, (StatusCode, String)> {
    require_scope(&state, &headers, AuthScope::Read).await?;

    let skills = state.app_state.skills.discover_skills_with_status(true);
    let mut result = Vec::new();

    for skill in skills {
        result.push(SkillStatus {
            name: skill.meta.name,
            description: skill.meta.description,
            enabled: skill.available,
            source: skill.meta.source,
            version: skill.meta.version,
            platforms: skill.meta.platforms,
            reason: skill.reason,
        });
    }

    Ok(Json(SkillsListResponse {
        ok: true,
        skills: result,
    }))
}

#[utoipa::path(
    post,
    path = "/api/skills/{name}/enable",
    operation_id = "skills_enable",
    tag = "skills",
    params(
        ("name" = String, Path, description = "Skill name to enable"),
    ),
    responses(
        (status = 200, description = "Skill enabled", body = SkillToggleResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Write)"),
        (status = 404, description = "Skill not found"),
        (status = 500, description = "Skill manager failed to enable"),
    ),
)]
pub(super) async fn api_enable_skill(
    headers: HeaderMap,
    Path(name): Path<String>,
    State(state): State<WebState>,
) -> Result<Json<SkillToggleResponse>, (StatusCode, String)> {
    require_scope(&state, &headers, AuthScope::Write).await?;

    if !state.app_state.skills.has_skill(&name) {
        return Err((StatusCode::NOT_FOUND, "Skill not found".into()));
    }

    state
        .app_state
        .skills
        .set_enabled(&name, true)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(SkillToggleResponse {
        ok: true,
        message: "Skill enabled".to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/skills/{name}/disable",
    operation_id = "skills_disable",
    tag = "skills",
    params(
        ("name" = String, Path, description = "Skill name to disable"),
    ),
    responses(
        (status = 200, description = "Skill disabled", body = SkillToggleResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Write)"),
        (status = 404, description = "Skill not found"),
        (status = 500, description = "Skill manager failed to disable"),
    ),
)]
pub(super) async fn api_disable_skill(
    headers: HeaderMap,
    Path(name): Path<String>,
    State(state): State<WebState>,
) -> Result<Json<SkillToggleResponse>, (StatusCode, String)> {
    require_scope(&state, &headers, AuthScope::Write).await?;

    if !state.app_state.skills.has_skill(&name) {
        return Err((StatusCode::NOT_FOUND, "Skill not found".into()));
    }

    state
        .app_state
        .skills
        .set_enabled(&name, false)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(SkillToggleResponse {
        ok: true,
        message: "Skill disabled".to_string(),
    }))
}
