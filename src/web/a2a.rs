use super::*;
use crate::a2a::{
    build_agent_card, default_session_key_for_source, local_agent_name, A2AMessageRequest,
    A2AMessageResponse, A2A_PROTOCOL_VERSION,
};
use utoipa::ToSchema;

#[derive(Debug, serde::Serialize, ToSchema)]
#[allow(dead_code)]
pub(super) struct A2AAgentCardSchema {
    pub protocol_version: String,
    pub agent_id: String,
    pub agent_name: String,
    pub description: Option<String>,
    pub public_base_url: Option<String>,
    pub endpoints: A2AEndpointsSchema,
    pub capabilities: Vec<String>,
}

#[derive(Debug, serde::Serialize, ToSchema)]
#[allow(dead_code)]
pub(super) struct A2AEndpointsSchema {
    pub agent_card: String,
    pub message: String,
}

#[derive(Debug, serde::Deserialize, ToSchema)]
#[allow(dead_code)]
pub(super) struct A2AMessageRequestSchema {
    pub session_key: Option<String>,
    pub sender_name: Option<String>,
    pub source_agent: Option<String>,
    pub source_url: Option<String>,
    pub message: String,
}

#[derive(Debug, serde::Serialize, ToSchema)]
#[allow(dead_code)]
pub(super) struct A2AMessageResponseSchema {
    pub ok: bool,
    pub protocol_version: String,
    pub agent_name: String,
    pub session_key: String,
    pub response: String,
}

fn a2a_token_allowed(config: &Config, headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let raw = raw.trim();
    let mut parts = raw.splitn(2, char::is_whitespace);
    let Some(scheme) = parts.next() else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    };
    let Some(token) = parts.next().map(str::trim).filter(|v| !v.is_empty()) else {
        return false;
    };
    config
        .a2a
        .shared_tokens
        .iter()
        .any(|candidate| candidate == token)
}

#[utoipa::path(
    get,
    path = "/api/a2a/agent-card",
    operation_id = "a2a_agent_card",
    tag = "a2a",
    responses(
        (status = 200, description = "Agent card describing this microclaw instance for A2A peers", body = A2AAgentCardSchema),
        (status = 404, description = "A2A is disabled"),
    ),
)]
pub(super) async fn api_a2a_agent_card(
    State(state): State<WebState>,
) -> Result<Json<crate::a2a::A2AAgentCard>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    if !state.app_state.config.a2a.enabled {
        return Err((StatusCode::NOT_FOUND, "A2A is disabled".into()));
    }
    Ok(Json(build_agent_card(&state.app_state.config)))
}

#[utoipa::path(
    post,
    path = "/api/a2a/message",
    operation_id = "a2a_message_send",
    tag = "a2a",
    request_body = A2AMessageRequestSchema,
    responses(
        (status = 200, description = "Agent reply to the inbound A2A message", body = A2AMessageResponseSchema),
        (status = 400, description = "message body missing or empty"),
        (status = 401, description = "Invalid bearer token"),
        (status = 403, description = "A2A inbound auth not configured"),
        (status = 404, description = "A2A is disabled"),
    ),
)]
pub(super) async fn api_a2a_message(
    headers: HeaderMap,
    State(state): State<WebState>,
    Json(body): Json<A2AMessageRequest>,
) -> Result<Json<A2AMessageResponse>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    if !state.app_state.config.a2a.enabled {
        return Err((StatusCode::NOT_FOUND, "A2A is disabled".into()));
    }
    if state.app_state.config.a2a.shared_tokens.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            "A2A inbound auth is not configured".into(),
        ));
    }
    if !a2a_token_allowed(&state.app_state.config, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "invalid A2A bearer token".into()));
    }

    let message = body.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message is required".into()));
    }
    let session_key = body
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_session_key_for_source(body.source_agent.as_deref()));
    let sender_name = body
        .sender_name
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            body.source_agent
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|v| format!("a2a:{v}"))
        })
        .unwrap_or_else(|| "a2a-remote".to_string());

    let result = super::send_and_store_response(
        state.clone(),
        super::SendRequest {
            session_key: Some(session_key.clone()),
            sender_name: Some(sender_name),
            message,
        },
    )
    .await?;
    let payload = result.0;
    let response = payload
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let resolved_session_key = payload
        .get("session_key")
        .and_then(|v| v.as_str())
        .unwrap_or(&session_key)
        .to_string();

    audit_log(
        &state,
        "a2a",
        body.source_agent.as_deref().unwrap_or("a2a-peer"),
        "a2a.message",
        Some(&resolved_session_key),
        "ok",
        body.source_url.as_deref(),
    )
    .await;

    Ok(Json(A2AMessageResponse {
        ok: true,
        protocol_version: A2A_PROTOCOL_VERSION.to_string(),
        agent_name: local_agent_name(&state.app_state.config),
        session_key: resolved_session_key,
        response,
    }))
}
