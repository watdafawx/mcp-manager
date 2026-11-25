//! The MCP endpoint harnesses connect to (`/mcp` and `/mcp/<harness>`): Streamable HTTP,
//! JSON responses, one optional SSE stream per session for `list_changed`.
//! Port of `relay-http.cjs` with the tools renamed `mcpm_*`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::pool::ListToolsOpts;
use crate::rpc::{self, TOOLS_LIST_CHANGED};
use crate::server::{AppState, Body, SessionInfo, json_response, status_response};
use crate::timeutil;

const MAX_BODY: usize = 4 * 1024 * 1024;
const SSE_KEEPALIVE: Duration = Duration::from_secs(25);
pub const SESSION_HEADER: &str = "mcp-session-id";
pub const HARNESS_HEADER: &str = "x-mcpm-harness";

pub const META_TOOLS: [&str; 4] = [
    "mcpm_list_servers",
    "mcpm_list_tools",
    "mcpm_call",
    "mcpm_reload_server",
];

pub fn meta_tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "mcpm_list_servers",
            "description": "List all MCP servers managed by MCP Manager that are exposed to this client, including whether each is enabled, running, idle or errored, and whether it is shared or session-isolated.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "mcpm_list_tools",
            "description": "List available tools and their JSON schemas from one MCP server (recommended) or all enabled servers. Use `query` to filter by name/description.",
            "inputSchema": { "type": "object", "properties": {
                "server": { "type": "string", "description": "Server id from mcpm_list_servers. Omit to query all enabled servers." },
                "query": { "type": "string", "description": "Optional case-insensitive substring filter." }
            }, "additionalProperties": false }
        }),
        json!({
            "name": "mcpm_call",
            "description": "Invoke a tool on any active MCP server through MCP Manager. Works for servers added or enabled mid-session.",
            "inputSchema": { "type": "object", "properties": {
                "server": { "type": "string", "description": "Server id from mcpm_list_servers." },
                "tool": { "type": "string", "description": "Tool name from mcpm_list_tools." },
                "arguments": { "type": "object", "description": "JSON arguments for the tool.", "additionalProperties": true },
                "sessionKey": { "type": "string", "description": "Optional explicit session key for session-mode servers (defaults to the caller's MCP session id)." }
            }, "required": ["server", "tool"], "additionalProperties": false }
        }),
        json!({
            "name": "mcpm_reload_server",
            "description": "Restart and reconnect a child MCP server if it crashed or its tools changed.",
            "inputSchema": { "type": "object", "properties": {
                "server": { "type": "string", "description": "Server id to restart." }
            }, "required": ["server"], "additionalProperties": false }
        }),
    ]
}

/// Picks a harness id from the MCP client name reported at initialize (fallback for plain `/mcp`).
pub fn detect_harness(client_name: Option<&str>) -> Option<&'static str> {
    let n = client_name?.to_ascii_lowercase();
    if n.contains("claude-desktop") || n.contains("claude desktop") {
        Some("claude-desktop")
    } else if n.contains("claude") {
        Some("claude")
    } else if n.contains("codex") {
        Some("codex")
    } else if n.contains("opencode") {
        Some("opencode")
    } else if n.contains("antigravity") {
        Some("antigravity")
    } else if n.contains("cursor") {
        Some("cursor")
    } else if n.contains("windsurf") {
        Some("windsurf")
    } else if n.contains("gemini") {
        Some("gemini-cli")
    } else if n.contains("vscode") || n.contains("visual studio code") {
        Some("vscode")
    } else {
        None
    }
}

fn valid_harness_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Entry point for any request under `/mcp`.
pub async fn handle(
    st: Arc<AppState>,
    req: Request<Incoming>,
    path_harness: Option<String>,
) -> Response<Body> {
    if let Some(h) = &path_harness
        && !valid_harness_segment(h)
    {
        return status_response(StatusCode::NOT_FOUND, "unknown harness id");
    }
    let session_id = req
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let header_harness = req
        .headers()
        .get(HARNESS_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| valid_harness_segment(s));
    let harness_hint = path_harness.or(header_harness);

    match *req.method() {
        Method::POST => post(st, req, session_id, harness_hint).await,
        Method::GET => get_stream(st, session_id, harness_hint),
        Method::DELETE => {
            if let Some(sid) = session_id {
                st.sessions.lock().unwrap().remove(&sid);
                st.pool.close_session(&sid);
            }
            status_response(StatusCode::NO_CONTENT, "")
        }
        _ => status_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

struct Ctx {
    session: Option<String>,
    harness: String,
}

async fn post(
    st: Arc<AppState>,
    req: Request<Incoming>,
    session_id: Option<String>,
    hint: Option<String>,
) -> Response<Body> {
    let body: Bytes = match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return status_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc::err(&Value::Null, -32700, "Parse error"),
                &[],
            );
        }
    };
    let is_batch = parsed.is_array();
    let messages: Vec<Value> = match parsed {
        Value::Array(a) => a,
        v => vec![v],
    };

    // Session bookkeeping.
    let init = messages
        .iter()
        .find(|m| m.get("method").and_then(Value::as_str) == Some("initialize"));
    let mut new_session: Option<String> = None;
    let sid = match (&session_id, init) {
        (None, Some(init)) => {
            let client_name = init
                .pointer("/params/clientInfo/name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let harness = hint
                .clone()
                .or_else(|| detect_harness(client_name.as_deref()).map(str::to_string));
            let id = timeutil::uuid();
            st.sessions
                .lock()
                .unwrap()
                .insert(id.clone(), SessionInfo::new(harness, client_name));
            new_session = Some(id.clone());
            Some(id)
        }
        (Some(id), _) => {
            // Lenient about unknown ids (daemon restarted): adopt them instead of forcing re-init.
            let mut sessions = st.sessions.lock().unwrap();
            let info = sessions
                .entry(id.clone())
                .or_insert_with(|| SessionInfo::new(hint.clone(), None));
            info.touch();
            if let Some(h) = &hint {
                info.harness = Some(h.clone());
            }
            Some(id.clone())
        }
        (None, None) => None,
    };
    let harness = sid
        .as_ref()
        .and_then(|id| {
            st.sessions
                .lock()
                .unwrap()
                .get(id)
                .and_then(|i| i.harness.clone())
        })
        .or(hint)
        .unwrap_or_else(|| "unknown".to_string());
    let ctx = Ctx {
        session: sid.clone(),
        harness,
    };

    let mut responses = Vec::new();
    for msg in messages {
        if let Some(r) = dispatch(&st, &msg, &ctx).await {
            responses.push(r);
        }
    }
    let mut extra: Vec<(&str, String)> = Vec::new();
    if let Some(id) = new_session.or(sid) {
        extra.push((SESSION_HEADER, id));
    }
    if responses.is_empty() {
        let mut b = Response::builder().status(StatusCode::ACCEPTED);
        for (k, v) in &extra {
            b = b.header(*k, v.as_str());
        }
        return b.body(Body::empty()).unwrap();
    }
    let value = if is_batch {
        Value::Array(responses)
    } else {
        responses.remove(0)
    };
    json_response(StatusCode::OK, &value, &extra)
}

/// Handles one JSON-RPC message; `None` for notifications and client responses.
async fn dispatch(st: &Arc<AppState>, msg: &Value, ctx: &Ctx) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id").filter(|v| !v.is_null())?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let ver = rpc::negotiate(params.get("protocolVersion").and_then(Value::as_str));
            rpc::ok(
                id,
                json!({
                    "protocolVersion": ver,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "mcpm", "version": crate::VERSION },
                    "instructions": "MCP Manager proxies your MCP servers. Call mcpm_list_servers, then mcpm_list_tools {server}, then mcpm_call {server, tool, arguments}.",
                }),
            )
        }
        "ping" => rpc::ok(id, json!({})),
        "tools/list" => {
            let pinned = st
                .pool
                .pinned_tools(Some(&ctx.harness), ctx.session.as_deref())
                .await;
            let mut tools = meta_tool_defs();
            tools.extend(pinned.into_iter().map(|p| json!({ "name": p.name, "description": p.description, "inputSchema": p.input_schema })));
            rpc::ok(id, json!({ "tools": tools }))
        }
        "tools/call" => rpc::ok(id, tool_call(st, &params, ctx).await),
        _ => rpc::err(id, -32601, format!("Method not found: {method}")),
    })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
}

async fn tool_call(st: &Arc<AppState>, params: &Value, ctx: &Ctx) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let harness = Some(ctx.harness.as_str());
    let session = ctx.session.as_deref();
    let pool = &st.pool;
    match name {
        "mcpm_list_servers" => rpc::text_result(&Value::Array(pool.list_servers(harness)), false),
        "mcpm_list_tools" => {
            let server = Some(str_arg(&args, "server"))
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_string);
            let listing = pool
                .list_tools(ListToolsOpts {
                    server,
                    query,
                    session: session.map(str::to_string),
                    harness: harness.map(str::to_string),
                })
                .await;
            rpc::text_result(&serde_json::to_value(listing).unwrap_or_default(), false)
        }
        "mcpm_call" => {
            let (server, tool) = (str_arg(&args, "server"), str_arg(&args, "tool"));
            if server.is_empty() || tool.is_empty() {
                return rpc::error_result("mcpm_call requires both `server` and `tool`");
            }
            let key = Some(str_arg(&args, "sessionKey"))
                .filter(|s| !s.is_empty())
                .or(session);
            let call_args = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match pool.call_tool(server, tool, call_args, key, harness).await {
                Ok(v) => v,
                Err(e) => rpc::error_result(e.to_string()),
            }
        }
        "mcpm_reload_server" => {
            let server = str_arg(&args, "server");
            if server.is_empty() {
                return rpc::error_result("mcpm_reload_server requires `server`");
            }
            match pool.restart_server(server, session, harness).await {
                Ok(v) => rpc::text_result(&v, false),
                Err(e) => rpc::error_result(e.to_string()),
            }
        }
        other => {
            // Pinned tool `<server>__<tool>` (server ids may contain `_`, so try every split).
            for (i, _) in other.match_indices("__") {
                let (server, tool) = (&other[..i], &other[i + 2..]);
                if let Some(cfg) = pool.server(server)
                    && cfg.is_exposed_to(harness)
                    && cfg.pins(tool)
                {
                    return match pool.call_tool(server, tool, args, session, harness).await {
                        Ok(v) => v,
                        Err(e) => rpc::error_result(e.to_string()),
                    };
                }
            }
            rpc::error_result(format!(
                "Unknown tool \"{other}\". Call mcpm_list_servers / mcpm_list_tools first."
            ))
        }
    }
}

/// `GET /mcp`: SSE stream carrying `notifications/tools/list_changed`.
fn get_stream(
    st: Arc<AppState>,
    session_id: Option<String>,
    hint: Option<String>,
) -> Response<Body> {
    if session_id.is_none() {
        return status_response(StatusCode::BAD_REQUEST, "Mcp-Session-Id header required");
    }
    if let Some(id) = &session_id {
        let mut sessions = st.sessions.lock().unwrap();
        sessions
            .entry(id.clone())
            .or_insert_with(|| SessionInfo::new(hint, None))
            .touch();
    }
    let (tx, rx) = mpsc::channel::<Bytes>(16);
    let mut events = st.pool.subscribe();
    let mut stop = st.stop.subscribe();
    tokio::spawn(async move {
        if tx.send(Bytes::from_static(b": mcpm\n\n")).await.is_err() {
            return;
        }
        let note = format!("data: {}\n\n", rpc::notification(TOOLS_LIST_CHANGED));
        let mut tick = tokio::time::interval(SSE_KEEPALIVE);
        tick.tick().await;
        loop {
            let chunk = tokio::select! {
                ev = events.recv() => match ev {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Bytes::from(note.clone()),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => Bytes::from_static(b": keep-alive\n\n"),
                _ = tx.closed() => break,
                _ = stop.changed() => break,
            };
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::Stream(rx))
        .unwrap()
}
