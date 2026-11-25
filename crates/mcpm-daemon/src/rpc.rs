//! JSON-RPC 2.0 / MCP helpers over `serde_json::Value`.

use serde_json::{Value, json};

pub const LATEST_PROTOCOL: &str = "2025-06-18";
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";

/// Picks the protocol version to answer `initialize` with: the client's if supported, else ours.
pub fn negotiate(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|r| SUPPORTED_PROTOCOLS.iter().find(|s| **s == r))
        .copied()
        .unwrap_or(LATEST_PROTOCOL)
}

pub fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn err(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// MCP tool result with a single text block.
pub fn text_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        v => serde_json::to_string_pretty(v).unwrap_or_default(),
    };
    let mut r = json!({ "content": [{ "type": "text", "text": text }] });
    if is_error {
        r["isError"] = Value::Bool(true);
    }
    r
}

pub fn error_result(message: impl Into<String>) -> Value {
    text_result(&Value::String(message.into()), true)
}

pub fn notification(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": method })
}
