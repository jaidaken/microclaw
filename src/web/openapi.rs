use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};

use crate::web::WebState;
use crate::web::metrics_http_inc;

// utoipa-axum 0.2's `routes!()` macro auto-collects each handler's
// `#[utoipa::path]` into the spec. Annotated handlers grow under
// `OpenApiRouter::routes(...)` over M1.A.2/A.3.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "microclaw",
        version = env!("CARGO_PKG_VERSION"),
        description = "Microclaw multi-channel agent runtime HTTP API",
        license(name = "MIT"),
    ),
    tags(
        (name = "auth",     description = "Authentication, sessions, API keys"),
        (name = "sessions", description = "Chat session lifecycle"),
        (name = "chat",     description = "Message send + receive (non-stream)"),
        (name = "stream",   description = "SSE streaming endpoints"),
        (name = "memory",   description = "Memory observability + mutations"),
        (name = "metrics",  description = "Metrics summary, history, subagent observability"),
        (name = "skills",   description = "Skill enable/disable"),
        (name = "a2a",      description = "Agent-to-agent messaging"),
        (name = "system",   description = "Health, config, audit, usage"),
    ),
)]
pub(super) struct ApiDoc;

/// Response body for the public `/health` snapshot (unauthenticated variant).
#[derive(Serialize, Deserialize, ToSchema)]
pub(super) struct HealthRoot {
    pub ok: bool,
    pub version: String,
    pub web_enabled: bool,
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "system_health_root",
    tag = "system",
    responses(
        (status = 200, description = "Microclaw health snapshot (unauthenticated minimal variant)", body = HealthRoot),
    ),
)]
pub(super) async fn api_health_root(State(state): State<WebState>) -> Json<HealthRoot> {
    metrics_http_inc(&state).await;
    Json(HealthRoot {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        web_enabled: state.app_state.config.web_enabled,
    })
}

// `/openapi.json` + `/docs` are mounted inline in `build_router`
// after `split_for_parts` because they capture the OpenApi value.
