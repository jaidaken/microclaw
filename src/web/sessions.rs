use super::*;
use microclaw_tools::todo_store::clear_todos;
use utoipa::ToSchema;

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SessionItemView {
    session_key: String,
    label: String,
    chat_id: i64,
    chat_type: String,
    last_message_time: String,
    last_message_preview: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SessionsListResponse {
    ok: bool,
    sessions: Vec<SessionItemView>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct HistoryItemView {
    id: String,
    sender_name: String,
    content: String,
    is_from_bot: bool,
    timestamp: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct HistoryResponse {
    ok: bool,
    session_key: String,
    chat_id: i64,
    messages: Vec<HistoryItemView>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SessionTreeNode {
    chat_id: i64,
    session_key: String,
    parent_session_key: Option<String>,
    fork_point: Option<i64>,
    updated_at: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, ToSchema)]
pub(super) struct SessionTreeResponse {
    ok: bool,
    nodes: Vec<SessionTreeNode>,
}

#[utoipa::path(
    get,
    path = "/api/sessions",
    description = "Lists recent chat sessions across all channels with last-message previews and timestamps",
    operation_id = "sessions_list",
    tag = "sessions",
    responses(
        (status = 200, description = "Recent chat sessions across channels", body = SessionsListResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Read)"),
        (status = 500, description = "Database read failure"),
    ),
)]
pub(super) async fn api_sessions(
    headers: HeaderMap,
    State(state): State<WebState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    let identity = require_scope(&state, &headers, AuthScope::Read).await?;

    let user_filter = if identity.is_operator() {
        None
    } else {
        Some(extract_user_id(&headers)?)
    };
    let chats = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_recent_chats(user_filter.as_deref(), 400)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let sessions = chats
        .into_iter()
        .map(|c| map_chat_to_session(&state.app_state.channel_registry, c))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "ok": true, "sessions": sessions })))
}

#[utoipa::path(
    get,
    path = "/api/history",
    description = "Returns the stored message history for a chat session, optionally tail-limited to the newest N messages",
    operation_id = "sessions_history",
    tag = "sessions",
    params(
        ("session_key" = Option<String>, Query, description = "Session key; defaults to 'main'"),
        ("limit" = Option<usize>, Query, description = "Limit messages returned (newest tail)"),
    ),
    responses(
        (status = 200, description = "Stored messages for the requested session", body = HistoryResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Read)"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Database read failure"),
    ),
)]
pub(super) async fn api_history(
    headers: HeaderMap,
    State(state): State<WebState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    let identity = require_scope(&state, &headers, AuthScope::Read).await?;

    let session_key = normalize_session_key(query.session_key.as_deref());
    let resolve_filter = if identity.is_operator() {
        None
    } else {
        Some(extract_user_id(&headers)?)
    };
    let chat_id =
        resolve_chat_id_for_session_key_read(&state, &session_key, resolve_filter.as_deref())
            .await?;
    assert_chat_visible_to_caller(&state, &identity, &headers, chat_id).await?;

    let mut messages = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_all_messages(chat_id)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if let Some(limit) = query.limit {
        if messages.len() > limit {
            messages = messages[messages.len() - limit..].to_vec();
        }
    }

    let items: Vec<HistoryItem> = messages
        .into_iter()
        .map(|m| HistoryItem {
            id: m.id,
            sender_name: m.sender_name,
            content: m.content,
            is_from_bot: m.is_from_bot,
            timestamp: m.timestamp,
        })
        .collect();

    Ok(Json(json!({
        "ok": true,
        "session_key": session_key,
        "chat_id": chat_id,
        "messages": items,
    })))
}

#[utoipa::path(
    post,
    path = "/api/reset",
    description = "Clears session state for a chat while keeping scheduled tasks intact; clears the TODO store for the channel",
    operation_id = "sessions_reset",
    tag = "sessions",
    request_body(content_type = "application/json", description = "Request body: { session_key?: string }"),
    responses(
        (status = 200, description = "Session reset, returns ok + deleted flag"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Approvals)"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Database write failure"),
    ),
)]
pub(super) async fn api_reset(
    headers: HeaderMap,
    State(state): State<WebState>,
    Json(body): Json<ResetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    let identity = require_scope(&state, &headers, AuthScope::Approvals).await?;
    let user_id = extract_user_id(&headers)?;

    let session_key = normalize_session_key(body.session_key.as_deref());
    let chat_id = resolve_chat_id_for_session_key(&state, &session_key, &user_id).await?;

    let is_web = get_chat_routing(
        &state.app_state.channel_registry,
        state.app_state.db.clone(),
        chat_id,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
    .map(|r| r.channel_name == "web")
    .unwrap_or(false);

    let deleted = if is_web {
        let deleted = call_blocking(state.app_state.db.clone(), move |db| {
            db.delete_chat_data(chat_id)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let session_key_for_chat = session_key.clone();
        let user_id_for_chat = user_id.clone();
        call_blocking(state.app_state.db.clone(), move |db| {
            db.upsert_chat(&user_id_for_chat, chat_id, Some(&session_key_for_chat), "web")
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        deleted
    } else {
        call_blocking(state.app_state.db.clone(), move |db| {
            db.delete_session(chat_id)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    let todo_channel = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_chat_channel(chat_id)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "web".to_string());
    let groups_dir = std::path::PathBuf::from(&state.app_state.config.data_dir).join("groups");
    if let Err(e) = clear_todos(&groups_dir, &todo_channel, chat_id) {
        warn!("Failed to clear TODO.json for chat {}: {}", chat_id, e);
    }

    audit_log(
        &state,
        "operator",
        &identity.actor,
        "session.reset",
        Some(&session_key),
        if deleted { "ok" } else { "miss" },
        None,
    )
    .await;
    Ok(Json(json!({ "ok": true, "deleted": deleted })))
}

#[utoipa::path(
    post,
    path = "/api/delete_session",
    description = "Deletes a chat session and all associated message history; clears the TODO store for the channel",
    operation_id = "sessions_delete",
    tag = "sessions",
    request_body(content_type = "application/json", description = "Request body: { session_key?: string }"),
    responses(
        (status = 200, description = "Session deleted, returns ok + deleted flag"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Approvals)"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Database write failure"),
    ),
)]
pub(super) async fn api_delete_session(
    headers: HeaderMap,
    State(state): State<WebState>,
    Json(body): Json<ResetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    let identity = require_scope(&state, &headers, AuthScope::Approvals).await?;
    let user_id = extract_user_id(&headers)?;

    let session_key = normalize_session_key(body.session_key.as_deref());
    let chat_id = resolve_chat_id_for_session_key(&state, &session_key, &user_id).await?;
    let todo_channel = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_chat_channel(chat_id)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "web".to_string());

    let deleted = call_blocking(state.app_state.db.clone(), move |db| {
        db.delete_chat_data(chat_id)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let groups_dir = std::path::PathBuf::from(&state.app_state.config.data_dir).join("groups");
    if let Err(e) = clear_todos(&groups_dir, &todo_channel, chat_id) {
        warn!("Failed to clear TODO.json for chat {}: {}", chat_id, e);
    }

    audit_log(
        &state,
        "operator",
        &identity.actor,
        "session.delete",
        Some(&session_key),
        if deleted { "ok" } else { "miss" },
        None,
    )
    .await;
    Ok(Json(json!({ "ok": true, "deleted": deleted })))
}

#[utoipa::path(
    post,
    path = "/api/sessions/fork",
    description = "Creates a new web session forked from an existing session, optionally truncated at a fork-point message index",
    operation_id = "sessions_fork",
    tag = "sessions",
    request_body(content_type = "application/json", description = "Request body: { source_session_key: string, target_session_key?: string, fork_point?: number }"),
    responses(
        (status = 200, description = "Forked session metadata"),
        (status = 400, description = "Target session_key matches source"),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Approvals)"),
        (status = 404, description = "Source session not found"),
        (status = 500, description = "Database write failure"),
    ),
)]
pub(super) async fn api_sessions_fork(
    headers: HeaderMap,
    State(state): State<WebState>,
    Json(body): Json<ForkSessionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    let identity = require_scope(&state, &headers, AuthScope::Approvals).await?;
    let user_id = extract_user_id(&headers)?;

    let source_session_key = normalize_session_key(Some(&body.source_session_key));
    let target_session_key = body
        .target_session_key
        .map(|v| normalize_session_key(Some(&v)))
        .unwrap_or_else(|| {
            let short = uuid::Uuid::new_v4().simple().to_string();
            format!("{source_session_key}-fork-{}", &short[..8])
        });
    if source_session_key == target_session_key {
        return Err((
            StatusCode::BAD_REQUEST,
            "target_session_key must differ from source_session_key".into(),
        ));
    }

    let source_chat_id = resolve_chat_id_for_session_key(&state, &source_session_key, &user_id).await?;
    let source_messages = call_blocking(state.app_state.db.clone(), move |db| {
        db.get_all_messages(source_chat_id)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let fork_point = body
        .fork_point
        .unwrap_or(source_messages.len())
        .min(source_messages.len());
    let fork_messages = source_messages[..fork_point].to_vec();
    let target_session_key_for_create = target_session_key.clone();
    let user_id_for_create = user_id.clone();
    let target_chat_id = call_blocking(state.app_state.db.clone(), move |db| {
        db.resolve_or_create_chat_id(
            &user_id_for_create,
            "web",
            &target_session_key_for_create,
            Some(&target_session_key_for_create),
            "web",
        )
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let target_chat_id_for_delete = target_chat_id;
    call_blocking(state.app_state.db.clone(), move |db| {
        db.delete_chat_data(target_chat_id_for_delete)?;
        Ok(())
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let target_session_key_for_upsert = target_session_key.clone();
    let user_id_for_upsert = user_id.clone();
    call_blocking(state.app_state.db.clone(), move |db| {
        db.upsert_chat(&user_id_for_upsert, target_chat_id, Some(&target_session_key_for_upsert), "web")
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    for msg in fork_messages {
        let copied = StoredMessage {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: target_chat_id,
            sender_name: msg.sender_name,
            content: msg.content,
            is_from_bot: msg.is_from_bot,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        call_blocking(state.app_state.db.clone(), move |db| {
            db.store_message(&copied)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    let source_session_key_for_save = source_session_key.clone();
    call_blocking(state.app_state.db.clone(), move |db| {
        db.save_session_with_meta(
            target_chat_id,
            "[]",
            Some(&source_session_key_for_save),
            Some(fork_point as i64),
            None,
        )
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    audit_log(
        &state,
        "operator",
        &identity.actor,
        "session.fork",
        Some(&target_session_key),
        "ok",
        Some(&source_session_key),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "source_session_key": source_session_key,
        "source_chat_id": source_chat_id,
        "target_session_key": target_session_key,
        "target_chat_id": target_chat_id,
        "fork_point": fork_point
    })))
}

#[utoipa::path(
    get,
    path = "/api/sessions/tree",
    description = "Returns the fork lineage tree across sessions: each node carries its parent session key, fork point, and last update time",
    operation_id = "sessions_tree",
    tag = "sessions",
    params(
        ("limit" = Option<usize>, Query, description = "Maximum nodes to return (1..=5000, default 1000)"),
    ),
    responses(
        (status = 200, description = "Session fork tree nodes", body = SessionTreeResponse),
        (status = 401, description = "Missing or invalid credentials"),
        (status = 403, description = "Insufficient scope (requires Read)"),
        (status = 500, description = "Database read failure"),
    ),
)]
pub(super) async fn api_sessions_tree(
    headers: HeaderMap,
    State(state): State<WebState>,
    Query(query): Query<SessionTreeQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    metrics_http_inc(&state).await;
    require_scope(&state, &headers, AuthScope::Read).await?;
    let limit = query.limit.unwrap_or(1000).clamp(1, 5000);
    let rows = call_blocking(state.app_state.db.clone(), move |db| {
        db.list_session_meta(limit)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut out = Vec::new();
    for (chat_id, parent_session_key, fork_point, updated_at) in rows {
        let session_key = call_blocking(state.app_state.db.clone(), move |db| {
            db.get_chat_external_id(chat_id)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .unwrap_or_else(|| format!("chat:{chat_id}"));

        out.push(json!({
            "chat_id": chat_id,
            "session_key": session_key,
            "parent_session_key": parent_session_key,
            "fork_point": fork_point,
            "updated_at": updated_at
        }));
    }
    Ok(Json(json!({"ok": true, "nodes": out})))
}
