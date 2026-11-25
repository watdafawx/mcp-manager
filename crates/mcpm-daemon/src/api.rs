//! Control API (`/api/*`), Bearer-token protected. Used by the GUI and CLI.
//!
//! ```text
//! GET    /api/status
//! GET    /api/servers                    POST /api/servers (add)
//! GET    /api/servers/{id}[?reveal=1]    PUT  /api/servers/{id}   DELETE /api/servers/{id}
//! POST   /api/servers/{id}/enable | disable | restart
//! GET    /api/servers/{id}/tools
//! GET    /api/calls[?limit=N]
//! POST   /api/shutdown
//! GET    /api/harnesses                  (CoreOps)
//! POST   /api/takeover/plan  {harness}   (CoreOps)
//! POST   /api/takeover/apply {harness|"all"}
//! GET    /api/snapshots   DELETE /api/snapshots/{id}
//! POST   /api/restore     {harness}|{all:true}|{snapshot:id}[,keepInManager]
//! ```

#![allow(clippy::result_large_err)] // error variant is a ready HTTP response

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use mcpm_core::model::ServerConfig;
use serde_json::{Value, json};

use crate::coreops::{OpError, OpResult};
use crate::mask;
use crate::server::{AppState, Body, json_response, status_response};
use crate::timeutil;

const MAX_BODY: usize = 1024 * 1024;

fn authorized(req: &Request<Incoming>, token: &str) -> bool {
    let Some(h) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(given) = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
    else {
        return false;
    };
    constant_time_eq(given.trim().as_bytes(), token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

fn err(status: StatusCode, msg: impl AsRef<str>) -> Response<Body> {
    status_response(status, msg.as_ref())
}

fn ok(value: Value) -> Response<Body> {
    json_response(StatusCode::OK, &value, &[])
}

fn op_response(r: OpResult) -> Response<Body> {
    match r {
        Ok(v) => ok(v),
        Err(OpError::NotImplemented(what)) => err(
            StatusCode::NOT_IMPLEMENTED,
            format!("not implemented: {what}"),
        ),
        Err(OpError::Invalid(m)) => err(StatusCode::BAD_REQUEST, m),
        Err(OpError::Failed(m)) => err(StatusCode::INTERNAL_SERVER_ERROR, m),
    }
}

async fn read_json(req: Request<Incoming>) -> Result<Value, Response<Body>> {
    let bytes: Bytes = match Limited::new(req.into_body(), MAX_BODY).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")),
    };
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

/// Parses a server config body; `name` defaults to the id.
fn parse_server(mut v: Value, id_override: Option<&str>) -> Result<ServerConfig, Response<Body>> {
    if let Some(obj) = v.as_object_mut() {
        if let Some(id) = id_override {
            obj.insert("id".into(), json!(id));
        }
        if !obj.contains_key("name") {
            let id = obj.get("id").cloned().unwrap_or(json!(""));
            obj.insert("name".into(), id);
        }
    }
    serde_json::from_value(v).map_err(|e| {
        err(
            StatusCode::BAD_REQUEST,
            format!("invalid server config: {e}"),
        )
    })
}

async fn run_core<F>(f: F) -> OpResult
where
    F: FnOnce() -> OpResult + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(OpError::Failed(format!("core task failed: {e}"))))
}

pub async fn handle(st: Arc<AppState>, req: Request<Incoming>) -> Response<Body> {
    if !authorized(&req, &st.token) {
        let mut r = err(StatusCode::UNAUTHORIZED, "unauthorized");
        r.headers_mut()
            .insert("www-authenticate", "Bearer".parse().unwrap());
        return r;
    }
    let method = req.method().clone();
    let path = req.uri().path().trim_end_matches('/').to_string();
    let query = req.uri().query().map(str::to_string);
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect(); // ["api", ...]

    match (&method, segs.as_slice()) {
        (&Method::GET, ["api", "status"]) => ok(status(&st)),
        (&Method::POST, ["api", "shutdown"]) => {
            let st2 = st.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                st2.shutdown_requested.notify_one();
            });
            ok(json!({ "ok": true }))
        }

        (&Method::GET, ["api", "servers"]) => ok(Value::Array(st.pool.list_servers(None))),
        (&Method::POST, ["api", "servers"]) => {
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let cfg = match parse_server(body, None) {
                Ok(c) => c,
                Err(r) => return r,
            };
            match st.pool.add_server(cfg) {
                Ok(c) => json_response(
                    StatusCode::CREATED,
                    &serde_json::to_value(mask::mask_config(&c)).unwrap_or_default(),
                    &[],
                ),
                Err(e) if e.to_string().contains("already exists") => {
                    err(StatusCode::CONFLICT, e.to_string())
                }
                Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
            }
        }
        (&Method::GET, ["api", "servers", id]) => match st.pool.server(id) {
            None => err(StatusCode::NOT_FOUND, "unknown server"),
            Some(c) => {
                let reveal = matches!(query_param(query.as_deref(), "reveal"), Some("1" | "true"));
                let shown = if reveal { c } else { mask::mask_config(&c) };
                ok(serde_json::to_value(shown).unwrap_or_default())
            }
        },
        (&Method::PUT, ["api", "servers", id]) => {
            let Some(old) = st.pool.server(id) else {
                return err(StatusCode::NOT_FOUND, "unknown server");
            };
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut cfg = match parse_server(body, Some(id)) {
                Ok(c) => c,
                Err(r) => return r,
            };
            mask::restore_masked(&old, &mut cfg);
            match st.pool.update_server(cfg) {
                Ok(c) => ok(serde_json::to_value(mask::mask_config(&c)).unwrap_or_default()),
                Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
            }
        }
        (&Method::DELETE, ["api", "servers", id]) => {
            if st.pool.remove_server(id) {
                ok(json!({ "ok": true }))
            } else {
                err(StatusCode::NOT_FOUND, "unknown server")
            }
        }
        (&Method::POST, ["api", "servers", id, action @ ("enable" | "disable")]) => {
            match st.pool.set_enabled(id, *action == "enable") {
                Ok(c) => ok(serde_json::to_value(mask::mask_config(&c)).unwrap_or_default()),
                Err(e) => err(StatusCode::NOT_FOUND, e.to_string()),
            }
        }
        (&Method::POST, ["api", "servers", id, "restart"]) => {
            if st.pool.server(id).is_none() {
                return err(StatusCode::NOT_FOUND, "unknown server");
            }
            match st.pool.restart_server(id, None, None).await {
                Ok(v) => ok(v),
                Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
            }
        }
        (&Method::GET, ["api", "servers", id, "tools"]) => {
            if st.pool.server(id).is_none() {
                return err(StatusCode::NOT_FOUND, "unknown server");
            }
            match st
                .pool
                .server_tools(id, query_param(query.as_deref(), "session"))
                .await
            {
                Ok(t) => ok(Value::Array(t)),
                Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
            }
        }
        (&Method::GET, ["api", "calls"]) => {
            let limit = query_param(query.as_deref(), "limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(25usize)
                .min(crate::pool::MAX_CALL_LOG);
            ok(serde_json::to_value(st.pool.recent_calls(limit)).unwrap_or_default())
        }

        // ---- CoreOps ----
        (&Method::GET, ["api", "harnesses"]) => {
            let core = st.core.clone();
            op_response(run_core(move || core.harnesses()).await)
        }
        (&Method::POST, ["api", "takeover", "plan"]) => {
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let Some(h) = body
                .get("harness")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return err(StatusCode::BAD_REQUEST, "body needs `harness`");
            };
            let core = st.core.clone();
            op_response(run_core(move || core.plan_takeover(&h)).await)
        }
        (&Method::POST, ["api", "takeover", "apply"]) => {
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let target = if body.get("all").and_then(Value::as_bool) == Some(true) {
                "all".to_string()
            } else if let Some(h) = body.get("harness").and_then(Value::as_str) {
                h.to_string()
            } else {
                return err(
                    StatusCode::BAD_REQUEST,
                    "body needs `harness` or `all: true`",
                );
            };
            let core = st.core.clone();
            let r = run_core(move || core.apply_takeover(&target)).await;
            after_core_write(&st, &r);
            op_response(r)
        }
        (&Method::GET, ["api", "snapshots"]) => {
            let core = st.core.clone();
            op_response(run_core(move || core.snapshots()).await)
        }
        (&Method::DELETE, ["api", "snapshots", id]) => {
            let (core, id) = (st.core.clone(), id.to_string());
            op_response(run_core(move || core.delete_snapshot(&id)).await)
        }
        (&Method::POST, ["api", "restore"]) => {
            let body = match read_json(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let core = st.core.clone();
            let r = run_core(move || core.restore(&body)).await;
            after_core_write(&st, &r);
            op_response(r)
        }

        (_, ["api", ..]) => err(StatusCode::NOT_FOUND, "no such endpoint"),
        _ => err(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Core ops rewrite `config.json`: pick the changes up and tell connected sessions.
fn after_core_write(st: &AppState, r: &OpResult) {
    if r.is_ok()
        && let Err(e) = st.pool.reload_from_disk()
    {
        st.log.warn(&format!("reload after core op failed: {e:#}"));
    }
}

fn status(st: &AppState) -> Value {
    let cfg = st.pool.config();
    json!({
        "pid": std::process::id(),
        "version": crate::VERSION,
        "port": st.port,
        "startedAt": timeutil::iso(st.started_ms),
        "uptimeSecs": timeutil::now_ms().saturating_sub(st.started_ms) / 1000,
        "servers": cfg.servers.len(),
        "enabledServers": cfg.servers.iter().filter(|s| s.enabled).count(),
        "runningInstances": st.pool.running_instances(),
        "sessions": st.sessions.lock().unwrap().len(),
        "home": st.home.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn query() {
        assert_eq!(query_param(Some("a=1&limit=5"), "limit"), Some("5"));
        assert_eq!(query_param(None, "x"), None);
    }
}
