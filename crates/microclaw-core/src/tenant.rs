//! M1.5 transitional: callers without an explicit user context use this until phase 2 wires real sources.

pub const BOOTSTRAP_USER_ID_ENV: &str = "MICROCLAW_BOOTSTRAP_USER_ID";
pub const BOOTSTRAP_USER_ID_PLACEHOLDER: &str = "__pending__";

pub fn bootstrap_user_id() -> String {
    std::env::var(BOOTSTRAP_USER_ID_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| BOOTSTRAP_USER_ID_PLACEHOLDER.to_string())
}
