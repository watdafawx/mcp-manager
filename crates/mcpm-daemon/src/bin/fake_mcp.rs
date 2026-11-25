//! Tiny fake stdio MCP server for the daemon's integration tests.
//!
//! Tools: `echo {text}` (returns server name, pid, text), `pid`, `crash` (exits without answering),
//! `grow` (adds tool `extra` and sends `notifications/tools/list_changed`), `env {name}`.
//! Env: `FAKE_NAME` (default "fake"), `FAKE_STARTUP_MS` (delay before answering `initialize`).

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn send(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn tool(name: &str, desc: &str) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } }
    })
}

fn text(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": v.to_string() }] })
}

fn main() {
    let name = std::env::var("FAKE_NAME").unwrap_or_else(|_| "fake".into());
    let mut grown = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(method), id) = (
            msg.get("method").and_then(Value::as_str),
            msg.get("id").cloned(),
        ) else {
            continue;
        };
        let Some(id) = id else { continue }; // notifications
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                if let Some(ms) = std::env::var("FAKE_STARTUP_MS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                send(&json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": name, "version": "0.0.1" }
                }}));
            }
            "tools/list" => {
                let mut tools = vec![
                    tool("echo", "Echo text back with the server name and pid"),
                    tool("pid", "Return the process id"),
                    tool("crash", "Exit the process"),
                    tool("grow", "Add a tool and announce it"),
                    tool("env", "Return an environment variable"),
                ];
                if grown {
                    tools.push(tool("extra", "Added by grow"));
                }
                send(&json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }));
            }
            "tools/call" => {
                let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match tool_name {
                    "echo" => text(
                        json!({ "server": name, "pid": std::process::id(), "echo": args.get("text") }),
                    ),
                    "pid" => text(json!({ "server": name, "pid": std::process::id() })),
                    "env" => {
                        let var = args.get("name").and_then(Value::as_str).unwrap_or("");
                        text(json!({ "value": std::env::var(var).ok() }))
                    }
                    "crash" => std::process::exit(3),
                    "grow" => {
                        grown = true;
                        send(
                            &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
                        );
                        text(json!({ "grown": true }))
                    }
                    other => {
                        send(
                            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": format!("unknown tool {other}") } }),
                        );
                        continue;
                    }
                };
                send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            "ping" => send(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
            other => send(
                &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {other}") } }),
            ),
        }
    }
}
