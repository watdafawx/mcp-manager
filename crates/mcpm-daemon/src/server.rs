//! HTTP plumbing: listener, routing, response helpers, shared state. hyper 1 / HTTP/1.1 only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};

use crate::coreops::CoreOps;
use crate::logging::Logger;
use crate::pool::Pool;
use crate::{api, mcp};

/// Response body: a single buffer, or a channel for SSE.
pub enum Body {
    Full(Option<Bytes>),
    Stream(mpsc::Receiver<Bytes>),
}

impl Body {
    pub fn empty() -> Body {
        Body::Full(None)
    }
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Body::Full(b) => Poll::Ready(
                b.take()
                    .filter(|b| !b.is_empty())
                    .map(|b| Ok(Frame::data(b))),
            ),
            Body::Stream(rx) => rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Body::Full(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Body::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            Body::Full(None) => SizeHint::with_exact(0),
            Body::Stream(_) => SizeHint::default(),
        }
    }
}

pub struct SessionInfo {
    pub harness: Option<String>,
    pub client_name: Option<String>,
    pub last_seen: Instant,
}

impl SessionInfo {
    pub fn new(harness: Option<String>, client_name: Option<String>) -> Self {
        Self {
            harness,
            client_name,
            last_seen: Instant::now(),
        }
    }
    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }
}

pub struct AppState {
    pub pool: Pool,
    pub token: String,
    pub core: Arc<dyn CoreOps>,
    pub log: Arc<Logger>,
    pub home: std::path::PathBuf,
    pub port: u16,
    pub started_ms: u64,
    pub sessions: Mutex<HashMap<String, SessionInfo>>,
    /// Set by `POST /api/shutdown`; the daemon main loop waits on it.
    pub shutdown_requested: Arc<Notify>,
    /// Self-update (None in tests and when switched off).
    pub update: Option<Arc<crate::update::Service>>,
    /// `true` once the daemon is stopping: ends connections and SSE streams.
    pub stop: watch::Sender<bool>,
}

pub fn status_response(status: StatusCode, msg: &str) -> Response<Body> {
    if msg.is_empty() {
        return Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap();
    }
    json_response(status, &json!({ "error": msg }), &[])
}

pub fn json_response(
    status: StatusCode,
    value: &Value,
    headers: &[(&str, String)],
) -> Response<Body> {
    let bytes = Bytes::from(serde_json::to_vec(value).unwrap_or_default());
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", bytes.len());
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    b.body(Body::Full(Some(bytes))).unwrap()
}

fn host_is_local(host: &str) -> bool {
    let h = host.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        h.split(':').next().unwrap_or("")
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// DNS-rebinding guard: only loopback `Host` and (if sent) loopback `Origin`.
fn check_local(req: &Request<Incoming>) -> Option<Response<Body>> {
    let host_ok = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .is_none_or(host_is_local);
    let origin_ok = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|o| {
            o.split_once("://")
                .is_some_and(|(_, rest)| host_is_local(rest.split('/').next().unwrap_or("")))
        });
    if host_ok && origin_ok {
        None
    } else {
        Some(status_response(StatusCode::FORBIDDEN, "forbidden origin"))
    }
}

pub async fn route(st: Arc<AppState>, req: Request<Incoming>) -> Response<Body> {
    if let Some(r) = check_local(&req) {
        return r;
    }
    let path = req.uri().path().to_string();
    match path.as_str() {
        "/health" => json_response(
            StatusCode::OK,
            &json!({ "ok": true, "pid": std::process::id(), "version": crate::VERSION }),
            &[],
        ),
        "/mcp" => mcp::handle(st, req, None).await,
        p if p.starts_with("/mcp/") => {
            let h = p["/mcp/".len()..].trim_end_matches('/').to_string();
            mcp::handle(st, req, Some(h)).await
        }
        p if p == "/api" || p.starts_with("/api/") => api::handle(st, req).await,
        _ => status_response(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Accept loop; returns when `stop` flips to true.
pub async fn serve(listener: TcpListener, st: Arc<AppState>) {
    let mut stop = st.stop.subscribe();
    loop {
        let (stream, _peer): (_, SocketAddr) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    st.log.warn(&format!("accept failed: {e}"));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = stop.changed() => break,
        };
        let st2 = st.clone();
        let mut stop2 = st.stop.subscribe();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let st = st2.clone();
                async move { Ok::<_, Infallible>(route(st, req).await) }
            });
            let conn = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), svc);
            tokio::select! {
                _ = conn => {}
                _ = stop2.changed() => {}
            }
        });
    }
}
