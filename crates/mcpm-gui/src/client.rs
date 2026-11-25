//! Blocking client for the daemon's loopback control API (`/api/*`, Bearer token).
//!
//! Used by the GUI worker thread and the CLI. Plain HTTP/1.1 over `std::net`, one request per
//! connection: no async runtime and no TLS needed for 127.0.0.1.

use std::fmt;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// Starting a child or listing its tools can take a while.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    /// `None` = the request never got an HTTP answer (daemon down, timeout, broken reply).
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    pub fn net(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into() }
    }
    pub fn is_connect(&self) -> bool {
        self.status.is_none()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} (HTTP {s})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, ApiError> {
    let bad = || ApiError::net("broken chunked reply");
    let mut out = Vec::new();
    loop {
        let eol = find(data, b"\r\n").ok_or_else(bad)?;
        let size_text = std::str::from_utf8(&data[..eol]).map_err(|_| bad())?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| bad())?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Parses a full `Connection: close` HTTP/1.1 reply.
pub fn parse_response(raw: &[u8]) -> Result<RawResponse, ApiError> {
    let split = find(raw, b"\r\n\r\n").ok_or_else(|| ApiError::net("incomplete HTTP reply"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| ApiError::net("bad HTTP status line"))?;
    let mut chunked = false;
    let mut length = None;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                length = v.parse::<usize>().ok();
            }
        }
    }
    let body = if chunked {
        decode_chunked(body)?
    } else if let Some(n) = length {
        if body.len() < n {
            return Err(ApiError::net("truncated HTTP reply"));
        }
        body[..n].to_vec()
    } else {
        body.to_vec()
    };
    Ok(RawResponse { status, body })
}

/// One request against `addr`. `token` adds the Bearer header.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<RawResponse, ApiError> {
    let connect = Duration::from_secs(3).min(timeout);
    let mut s = TcpStream::connect_timeout(&addr, connect)
        .map_err(|e| ApiError::net(format!("cannot reach the daemon on {addr}: {e}")))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let payload = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nAccept: application/json\r\n",
        addr.port()
    );
    if let Some(t) = token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(p) = &payload {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            p.len()
        ));
    } else if method != "GET" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if let Some(p) = payload {
        out.extend_from_slice(&p);
    }
    s.write_all(&out)
        .map_err(|e| ApiError::net(format!("send failed: {e}")))?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)
        .map_err(|e| ApiError::net(format!("read failed: {e}")))?;
    parse_response(&raw)
}

/// Pulls `{ "error": "..." }` out of an error body, else the raw text.
pub fn error_message(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => v
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        Err(_) => String::from_utf8_lossy(body).trim().to_string(),
    }
}

/// An authenticated connection to a running daemon.
#[derive(Debug, Clone)]
pub struct Conn {
    pub addr: SocketAddr,
    pub token: String,
    pub timeout: Duration,
}

impl Conn {
    pub fn new(port: u16, token: String) -> Self {
        Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), token, timeout: DEFAULT_TIMEOUT }
    }

    pub fn slow(&self) -> Self {
        Self { timeout: SLOW_TIMEOUT, ..self.clone() }
    }

    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let r = http(self.addr, method, path, Some(&self.token), body, self.timeout)?;
        if r.status >= 400 {
            let mut m = error_message(&r.body);
            if m.is_empty() {
                m = format!("request failed: {method} {path}");
            }
            return Err(ApiError { status: Some(r.status), message: m });
        }
        if r.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&r.body)
            .map_err(|e| ApiError::net(format!("bad JSON from the daemon: {e}")))
    }

    pub fn get(&self, path: &str) -> Result<Value, ApiError> {
        self.request("GET", path, None)
    }
    pub fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request("POST", path, Some(body))
    }
    pub fn put(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request("PUT", path, Some(body))
    }
    pub fn delete(&self, path: &str) -> Result<Value, ApiError> {
        self.request("DELETE", path, None)
    }
}

/// `daemon.json`: `{ pid, port, startedAt }`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    #[serde(default)]
    pub started_at: String,
}

pub fn read_daemon_info(home: &Path) -> Option<DaemonInfo> {
    serde_json::from_slice(&std::fs::read(home.join("daemon.json")).ok()?).ok()
}

/// Connection to the daemon described by `daemon.json`, if it is alive and is the recorded pid.
pub fn locate(home: &Path) -> Option<Conn> {
    let info = read_daemon_info(home)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let r = http(addr, "GET", "/health", None, None, Duration::from_secs(2)).ok()?;
    let v: Value = serde_json::from_slice(&r.body).ok()?;
    let alive = r.status == 200
        && v.get("ok") == Some(&Value::Bool(true))
        && v.get("pid").and_then(Value::as_u64) == Some(u64::from(info.pid));
    if !alive {
        return None;
    }
    let token = mcpm_core::paths::token(home).ok()?;
    Some(Conn::new(info.port, token))
}

/// Starts `<exe> daemon` detached: no console window, outlives the caller.
pub fn spawn_daemon(exe: &Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let flags = CREATE_NO_WINDOW | DETACHED_PROCESS;
        // Leave a launcher's job object when it allows breakaway so the daemon outlives us.
        if cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok() {
            return Ok(());
        }
        cmd.creation_flags(flags).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    cmd.spawn().map(|_| ())
}

/// Connects to the daemon, starting it (`exe daemon`) when `daemon.json` is missing or stale.
pub fn ensure_daemon(home: &Path, exe: &Path, wait: Duration) -> Result<Conn, ApiError> {
    if let Some(c) = locate(home) {
        return Ok(c);
    }
    spawn_daemon(exe)
        .map_err(|e| ApiError::net(format!("cannot start the daemon ({}): {e}", exe.display())))?;
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(c) = locate(home) {
            return Ok(c);
        }
    }
    Err(ApiError::net(format!(
        "the daemon did not come up within {}s (see {})",
        wait.as_secs(),
        home.join("logs").join("daemon.log").display()
    )))
}

/// The state dir for this process (`MCPM_HOME` honoured).
pub fn state_dir() -> PathBuf {
    mcpm_core::Core::from_env().state_dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_content_length_and_chunked() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"{}"[..]));
        let r = parse_response(
            b"HTTP/1.1 201 Created\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (201, &b"abcde"[..]));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn error_bodies() {
        assert_eq!(error_message(br#"{"error":"nope"}"#), "nope");
        assert_eq!(error_message(b"plain"), "plain");
    }

    /// A one-shot loopback server answering a canned reply; returns (port, request-text handle).
    fn serve_once(reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).unwrap();
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (port, h)
    }

    #[test]
    fn sends_token_and_body_and_maps_errors() {
        let (port, h) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
        let c = Conn::new(port, "tok".into());
        let v = c.post("/api/x", &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(v["ok"], true);
        let req = h.join().unwrap();
        assert!(req.starts_with("POST /api/x HTTP/1.1"));
        assert!(req.contains("Authorization: Bearer tok"));
        assert!(req.ends_with("{\"a\":1}"));

        let body = r#"{"error":"unknown server"}"#;
        let reply: &'static str = Box::leak(
            format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .into_boxed_str(),
        );
        let (port, h) = serve_once(reply);
        let e = Conn::new(port, "t".into()).get("/api/servers/x").unwrap_err();
        h.join().unwrap();
        assert_eq!(e.status, Some(404));
        assert_eq!(e.message, "unknown server");
    }

    #[test]
    fn dead_port_is_a_connect_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(Conn::new(port, "t".into()).get("/api/status").unwrap_err().is_connect());
    }

    #[test]
    fn locate_ignores_missing_and_stale_daemon_json() {
        let d = tempfile::tempdir().unwrap();
        assert!(locate(d.path()).is_none());
        std::fs::write(d.path().join("daemon.json"), r#"{"pid":1,"port":1,"startedAt":"x"}"#).unwrap();
        assert!(locate(d.path()).is_none());
    }
}
