//! MCP clients for child servers: newline-delimited JSON-RPC over stdio, and Streamable HTTP.
//! Port of `relay-clients.cjs`, with tree-kill (Job Object) and `CREATE_NO_WINDOW` on Windows.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mcpm_core::model::{ServerConfig, Transport};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::rpc::{LATEST_PROTOCOL, TOOLS_LIST_CHANGED};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `initialize` and the first `tools/list` may take (`startTimeoutSecs`, default 30 s).
pub fn start_timeout(cfg: &ServerConfig) -> Duration {
    cfg.start_timeout_secs
        .filter(|s| *s > 0)
        .map_or(REQUEST_TIMEOUT, |s| Duration::from_secs(s.into()))
}
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_TAIL: usize = 4000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Hooks a pool instance installs on a client.
#[derive(Clone)]
pub struct Callbacks {
    /// The child went away on its own (crash, session expired). Not called after `close()`.
    pub on_exit: Arc<dyn Fn(String) + Send + Sync>,
    /// The child sent `notifications/tools/list_changed`.
    pub on_tools_changed: Arc<dyn Fn() + Send + Sync>,
}

pub enum McpClient {
    Stdio(StdioClient),
    Http(HttpClient),
}

impl McpClient {
    /// Starts the child / opens the session and completes the MCP `initialize` handshake.
    pub async fn connect(cfg: &ServerConfig, cb: Callbacks) -> Result<McpClient> {
        match &cfg.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let c = StdioClient::spawn(command, args, env, cwd.as_deref(), cb).await?;
                if let Err(e) = c.initialize(start_timeout(cfg)).await {
                    c.close();
                    return Err(e);
                }
                Ok(McpClient::Stdio(c))
            }
            Transport::Http { url, headers } => {
                let c = HttpClient::new(url, headers, cb)?;
                c.initialize(start_timeout(cfg)).await?;
                Ok(McpClient::Http(c))
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        match self {
            McpClient::Stdio(c) => c.is_alive(),
            McpClient::Http(c) => c.alive.load(Ordering::SeqCst),
        }
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        match self {
            McpClient::Stdio(c) => c.request(method, params, timeout).await,
            McpClient::Http(c) => c.request(method, params, timeout).await,
        }
    }

    /// All tools (follows `nextCursor` pagination).
    pub async fn list_tools(&self) -> Result<Vec<Value>> {
        self.list_tools_within(REQUEST_TIMEOUT).await
    }

    /// All tools, each page answered within `timeout`.
    pub async fn list_tools_within(&self, timeout: Duration) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let res = self.request("tools/list", params, timeout).await?;
            if let Some(arr) = res.get("tools").and_then(Value::as_array) {
                tools.extend(arr.iter().cloned());
            }
            match res.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<Value> {
        let args = if args.is_null() { json!({}) } else { args };
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": args }),
            CALL_TIMEOUT,
        )
        .await
    }

    pub fn close(&self) {
        match self {
            McpClient::Stdio(c) => c.close(),
            McpClient::Http(c) => c.close(),
        }
    }
}

fn init_params() -> Value {
    json!({
        "protocolVersion": LATEST_PROTOCOL,
        "capabilities": {},
        "clientInfo": { "name": "mcpm", "version": crate::VERSION },
    })
}

// ---------------------------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------------------------

type Reply = Result<Value, String>;

#[derive(Default)]
struct Shared {
    pending: HashMap<u64, oneshot::Sender<Reply>>,
    alive: bool,
}

pub struct StdioClient {
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    closed: Arc<AtomicBool>,
}

fn fail_all(shared: &Mutex<Shared>, msg: &str) {
    let pending: Vec<_> = {
        let mut s = shared.lock().unwrap();
        s.alive = false;
        s.pending.drain().collect()
    };
    for (_, tx) in pending {
        let _ = tx.send(Err(msg.to_string()));
    }
}

impl StdioClient {
    async fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&str>,
        cb: Callbacks,
    ) -> Result<StdioClient> {
        let mut cmd = build_command(command, args, env);
        if let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start `{command}`"))?;
        #[cfg(windows)]
        let job = child
            .raw_handle()
            .and_then(|h| crate::jobobject::KillJob::assign(h).ok());
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("child stdout unavailable"))?;
        let stderr = child.stderr.take();

        let shared = Arc::new(Mutex::new(Shared {
            pending: HashMap::new(),
            alive: true,
        }));
        let closed = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        // Writer.
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // stderr tail (kept for crash messages; never logged).
        if let Some(mut stderr) = stderr {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > STDERR_TAIL {
                        let mut cut = t.len() - STDERR_TAIL;
                        while !t.is_char_boundary(cut) {
                            cut += 1;
                        }
                        t.drain(..cut);
                    }
                }
            });
        }

        // Reader.
        {
            let shared = shared.clone();
            let tx = tx.clone();
            let on_tools_changed = cb.on_tools_changed.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    handle_incoming(&shared, &tx, &on_tools_changed, msg);
                }
            });
        }

        // Supervisor: owns the child, reaps it, kills the tree on close.
        {
            let shared = shared.clone();
            let closed = closed.clone();
            let tail = stderr_tail.clone();
            let on_exit = cb.on_exit.clone();
            tokio::spawn(async move {
                let mut child = child;
                let status = tokio::select! {
                    s = child.wait() => s.ok(),
                    _ = kill_rx => {
                        let _ = child.kill().await;
                        None
                    }
                };
                #[cfg(windows)]
                drop(job); // kills any grandchildren still running
                let was_closed = closed.load(Ordering::SeqCst);
                let tail = tail.lock().unwrap().trim().to_string();
                let code = status
                    .and_then(|s| s.code())
                    .map_or_else(|| "none".to_string(), |c| c.to_string());
                let mut msg = format!("MCP stdio process exited (code={code})");
                if !tail.is_empty() {
                    let cut = tail.char_indices().rev().nth(299).map_or(0, |(i, _)| i);
                    msg.push_str(": ");
                    msg.push_str(&tail[cut..]);
                }
                fail_all(&shared, &msg);
                if !was_closed {
                    on_exit(msg);
                }
            });
        }

        Ok(StdioClient {
            shared,
            tx,
            next_id: AtomicU64::new(1),
            kill: Mutex::new(Some(kill_tx)),
            closed,
        })
    }

    async fn initialize(&self, timeout: Duration) -> Result<()> {
        self.request("initialize", init_params(), timeout).await?;
        self.notify("notifications/initialized");
        Ok(())
    }

    fn notify(&self, method: &str) {
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": method, "params": {} })
        );
        let _ = self.tx.send(line);
    }

    pub fn is_alive(&self) -> bool {
        self.shared.lock().unwrap().alive
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (otx, orx) = oneshot::channel();
        {
            let mut s = self.shared.lock().unwrap();
            if !s.alive {
                bail!("MCP stdio process is not running");
            }
            s.pending.insert(id, otx);
        }
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        );
        if self.tx.send(line).is_err() {
            self.shared.lock().unwrap().pending.remove(&id);
            bail!("MCP stdio process is not running");
        }
        match tokio::time::timeout(timeout, orx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(e)),
            Ok(Err(_)) => bail!("MCP stdio client closed"),
            Err(_) => {
                self.shared.lock().unwrap().pending.remove(&id);
                bail!(
                    "MCP stdio request \"{method}\" timed out after {}ms",
                    timeout.as_millis()
                )
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        fail_all(&self.shared, "MCP stdio client closed");
        if let Some(k) = self.kill.lock().unwrap().take() {
            let _ = k.send(());
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Routes one JSON-RPC message from the child: responses, server->client requests, notifications.
fn handle_incoming(
    shared: &Mutex<Shared>,
    tx: &mpsc::UnboundedSender<String>,
    on_tools_changed: &Arc<dyn Fn() + Send + Sync>,
    msg: Value,
) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").filter(|v| !v.is_null());
    match (method, id) {
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let Some(sender) = shared.lock().unwrap().pending.remove(&id) else {
                return;
            };
            let reply = match msg.get("error") {
                Some(e) => Err(e
                    .get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| e.to_string(), str::to_string)),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(reply);
        }
        (Some(m), Some(id)) => {
            // The child asks us something (ping, roots, sampling...). Answer so it never hangs.
            let reply = if m == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } })
            };
            let _ = tx.send(format!("{reply}\n"));
        }
        (Some(TOOLS_LIST_CHANGED), None) => on_tools_changed(),
        _ => {}
    }
}

/// Windows resolves `npx`, `.cmd` and `.bat` shims only through PATH/PATHEXT. We resolve them
/// ourselves (std then runs `.cmd`/`.bat` through cmd.exe with safe quoting); unresolvable
/// commands fall back to `cmd /C`, the equivalent of the relay's `needsShell`.
fn build_command(command: &str, args: &[String], env: &BTreeMap<String, String>) -> Command {
    let mut cmd = match resolve_program(command, env) {
        Some(p) => {
            let mut c = Command::new(p);
            c.args(args);
            c
        }
        None => {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command).args(args);
            c
        }
    };
    cmd.envs(env);
    cmd
}

#[cfg(not(windows))]
fn resolve_program(command: &str, _env: &BTreeMap<String, String>) -> Option<PathBuf> {
    Some(PathBuf::from(command))
}

#[cfg(windows)]
fn resolve_program(command: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    use std::ffi::OsString;
    let pathext: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase())
        .collect();
    let has_ext = std::path::Path::new(command).extension().is_some();
    let candidates = |base: PathBuf| -> Option<PathBuf> {
        if has_ext && base.is_file() {
            return Some(base);
        }
        for ext in &pathext {
            let mut s: OsString = base.clone().into_os_string();
            s.push(ext);
            let p = PathBuf::from(s);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    };
    if command.contains(['/', '\\', ':']) {
        return candidates(PathBuf::from(command));
    }
    let path = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| OsString::from(v))
        .or_else(|| std::env::var_os("PATH"))?;
    // `cmd.exe` itself and other system tools live in a PATH dir like any other.
    std::env::split_paths(&path).find_map(|dir| candidates(dir.join(command)))
}

// ---------------------------------------------------------------------------------------------
// Streamable HTTP
// ---------------------------------------------------------------------------------------------

pub struct HttpClient {
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    next_id: AtomicU64,
    pub(crate) alive: AtomicBool,
    closed: AtomicBool,
    cb: Callbacks,
}

impl HttpClient {
    fn new(url: &str, headers: &BTreeMap<String, String>, cb: Callbacks) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("build http client")?;
        Ok(Self {
            http,
            url: url.to_string(),
            headers: headers.clone(),
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            cb,
        })
    }

    async fn initialize(&self, timeout: Duration) -> Result<()> {
        let res = self.request("initialize", init_params(), timeout).await?;
        if let Some(v) = res.get("protocolVersion").and_then(Value::as_str) {
            *self.protocol.lock().unwrap() = Some(v.to_string());
        }
        self.post(
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
            None,
            REQUEST_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let msg = self
            .post(&body, Some(id), timeout)
            .await?
            .ok_or_else(|| anyhow!("Empty response for \"{method}\""))?;
        if let Some(e) = msg.get("error") {
            bail!(
                "{}",
                e.get("message")
                    .and_then(Value::as_str)
                    .map_or_else(|| e.to_string(), str::to_string)
            );
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    /// POSTs `body`; with `want_id` returns the matching JSON-RPC response (JSON or SSE body).
    async fn post(
        &self,
        body: &Value,
        want_id: Option<u64>,
        timeout: Duration,
    ) -> Result<Option<Value>> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("MCP HTTP client closed");
        }
        let mut req = self
            .http
            .post(&self.url)
            .timeout(timeout)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let session = self.session.lock().unwrap().clone();
        if let Some(s) = &session {
            req = req.header("mcp-session-id", s.as_str());
        }
        if let Some(p) = self.protocol.lock().unwrap().clone() {
            req = req.header("mcp-protocol-version", p);
        }
        let mut res = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request failed: {}", e.without_url()))?;
        if let Some(sid) = res
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().unwrap() = Some(sid.to_string());
        }
        let status = res.status();
        if status.as_u16() == 404 && session.is_some() {
            self.alive.store(false, Ordering::SeqCst);
            (self.cb.on_exit)("MCP HTTP session expired".to_string());
            bail!("MCP HTTP session expired");
        }
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            bail!(
                "HTTP {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            );
        }
        let Some(want) = want_id else { return Ok(None) };
        let ct = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ct.contains("text/event-stream") {
            let bytes = res
                .bytes()
                .await
                .map_err(|e| anyhow!("HTTP body failed: {}", e.without_url()))?;
            return Ok(Some(
                serde_json::from_slice(&bytes).context("invalid JSON response")?,
            ));
        }
        // SSE: scan events until the response with our id arrives.
        let mut buf = String::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| anyhow!("HTTP stream failed: {}", e.without_url()))?
        {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            buf = buf.replace("\r\n", "\n");
            while let Some(end) = buf.find("\n\n") {
                let event: String = buf.drain(..end + 2).collect();
                let data: String = event
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|d| d.strip_prefix(' ').unwrap_or(d))
                    .collect::<Vec<_>>()
                    .join("\n");
                let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if msg.get("id").and_then(Value::as_u64) == Some(want)
                    && msg.get("method").is_none()
                {
                    return Ok(Some(msg));
                }
                if msg.get("method").and_then(Value::as_str) == Some(TOOLS_LIST_CHANGED) {
                    (self.cb.on_tools_changed)();
                }
            }
        }
        bail!("HTTP stream ended without a response")
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.alive.store(false, Ordering::SeqCst);
        // Best effort: end the server-side session.
        if let (Some(sid), Ok(handle)) = (
            self.session.lock().unwrap().take(),
            tokio::runtime::Handle::try_current(),
        ) {
            let mut req = self
                .http
                .delete(&self.url)
                .timeout(Duration::from_secs(3))
                .header("mcp-session-id", sid);
            for (k, v) in &self.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            handle.spawn(async move {
                let _ = req.send().await;
            });
        }
    }
}

impl Drop for HttpClient {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn resolves_cmd_exe_via_path() {
        let p = resolve_program("cmd", &BTreeMap::new()).expect("cmd on PATH");
        assert!(
            p.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe")
        );
    }

    #[test]
    fn unknown_command_falls_back() {
        assert!(resolve_program("definitely-not-a-command-xyz", &BTreeMap::new()).is_none());
    }
}
