use axum::http::{HeaderMap, StatusCode};

pub const X_CLAWCHAT_USER_ID: &str = "X-Clawchat-User-Id";

pub fn extract_user_id(headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let raw = headers.get(X_CLAWCHAT_USER_ID).ok_or((
        StatusCode::BAD_REQUEST,
        "missing_user_header: X-Clawchat-User-Id required on this endpoint".into(),
    ))?;
    let s = raw
        .to_str()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "missing_user_header: header is not ASCII".into(),
            )
        })?
        .trim();
    if s.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "missing_user_header: empty value".into(),
        ));
    }
    if s.len() > 64 || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err((
            StatusCode::BAD_REQUEST,
            "missing_user_header: invalid shape".into(),
        ));
    }
    Ok(s.to_string())
}
