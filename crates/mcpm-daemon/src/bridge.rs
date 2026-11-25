//! `mcpm bridge --harness <id>`: stdio <-> Streamable HTTP bridge for stdio-only harnesses.
//! Starts the daemon if it is not running. Uses the tiny `http_client` (no TLS stack loaded).

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

use crate::http_client;
use crate::{daemon, rpc};

#[cfg(windows)]
const DETACHED: u32 = 0x0000_0008 | 0x0000_0200 | 0x0800_0000; // DETACHED_PROCESS | NEW_PROCESS_GROUP | NO_WINDOW
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

pub struct BridgeOptions {
    /// State dir (same as the daemon's).
    pub home: PathBuf,
    pub harness: String,
    /// Executable started as `<exe> daemon` when no daemon runs. `None` = never start one.
    pub daemon_exe: Option<PathBuf>,
}

fn spawn_daemon(exe: &std::path::Path, home: &std::path::Path) {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").env("MCPM_HOME", home);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Leave the harness's job object if it allows breakaway so the daemon outlives the bridge.
        if cmd
            .creation_flags(DETACHED | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .is_ok()
        {
            return;
        }
        let _ = cmd.creation_flags(DETACHED).spawn();
    }
    #[cfg(not(windows))]
    let _ = cmd.spawn();
}

/// Finds the daemon's address, starting it when allowed.
pub async fn locate_daemon(opts: &BridgeOptions) -> Result<SocketAddr> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut spawned = false;
    loop {
        if let Some(info) = daemon::probe(&opts.home).await {
            return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, info.port)));
        }
        if !spawned {
            match &opts.daemon_exe {
                Some(exe) => spawn_daemon(exe, &opts.home),
                None => bail!("mcpm daemon is not running"),
            }
            spawned = true;
        }
        if Instant::now() > deadline {
            bail!("mcpm daemon did not start in time");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

struct Bridge<W> {
    opts: BridgeOptions,
    addr: AsyncMutex<SocketAddr>,
    session: Mutex<Option<String>>,
    out: AsyncMutex<W>,
}

impl<W: AsyncWrite + Unpin + Send + 'static> Bridge<W> {
    async fn write_line(&self, line: &str) {
        let mut out = self.out.lock().await;
        let _ = out.write_all(line.as_bytes()).await;
        let _ = out.write_all(b"\n").await;
        let _ = out.flush().await;
    }

    fn path(&self) -> String {
        format!("/mcp/{}", self.opts.harness)
    }

    async fn post(&self, body: &[u8]) -> std::io::Result<http_client::Response> {
        let addr = *self.addr.lock().await;
        let session = self.session.lock().unwrap().clone();
        let mut headers = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        let sid;
        if let Some(s) = session {
            sid = s;
            headers.push(("Mcp-Session-Id", sid.as_str()));
        }
        http_client::request(
            addr,
            "POST",
            &self.path(),
            &headers,
            Some(body),
            Duration::from_secs(300),
        )
        .await
    }

    /// Forwards one stdin message; retries once through a freshly located daemon.
    async fn forward(self: Arc<Self>, line: String) {
        let id = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .filter(|v| !v.is_null());
        let mut result = self.post(line.as_bytes()).await;
        if result.is_err()
            && let Ok(addr) = locate_daemon(&self.opts).await
        {
            *self.addr.lock().await = addr;
            result = self.post(line.as_bytes()).await;
        }
        match result {
            Ok(r) if r.status == 200 => {
                if let Some(sid) = r.header("mcp-session-id") {
                    let first = self
                        .session
                        .lock()
                        .unwrap()
                        .replace(sid.to_string())
                        .is_none();
                    if first {
                        self.clone().start_notifications();
                    }
                }
                let text = r.text();
                let text = text.trim();
                if !text.is_empty() {
                    self.write_line(text).await;
                }
            }
            Ok(r) if r.status == 202 || r.status == 204 => {}
            Ok(r) => {
                if let Some(id) = id {
                    self.write_line(
                        &rpc::err(
                            &id,
                            -32603,
                            format!(
                                "MCP Manager returned HTTP {}: {}",
                                r.status,
                                r.text().chars().take(200).collect::<String>()
                            ),
                        )
                        .to_string(),
                    )
                    .await;
                }
            }
            Err(e) => {
                if let Some(id) = id {
                    self.write_line(
                        &rpc::err(&id, -32603, format!("MCP Manager daemon unreachable: {e}"))
                            .to_string(),
                    )
                    .await;
                }
            }
        }
    }

    /// GET stream for server->client notifications (list_changed). Reconnects until stdin ends.
    fn start_notifications(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                let (addr, sid) = (
                    *self.addr.lock().await,
                    self.session.lock().unwrap().clone(),
                );
                let Some(sid) = sid else { return };
                if let Ok(mut stream) =
                    http_client::open_sse(addr, &self.path(), &[("Mcp-Session-Id", &sid)]).await
                    && stream.status == 200
                {
                    while let Ok(Some(data)) = stream.next_event().await {
                        self.write_line(&data).await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
}

/// Runs the bridge over arbitrary async IO (the CLI passes stdin/stdout; tests pass pipes).
pub async fn run_with_io<R, W>(opts: BridgeOptions, reader: R, writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let addr = locate_daemon(&opts).await?;
    let bridge = Arc::new(Bridge {
        opts,
        addr: AsyncMutex::new(addr),
        session: Mutex::new(None),
        out: AsyncMutex::new(writer),
    });
    let mut lines = reader.lines();
    let mut tasks = Vec::new();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        tasks.push(tokio::spawn(bridge.clone().forward(line)));
        tasks.retain(|t| !t.is_finished());
    }
    for t in tasks {
        let _ = t.await;
    }
    // End the MCP session so Session-mode children die with this bridge.
    let sid = bridge.session.lock().unwrap().take();
    if let Some(sid) = sid {
        let addr = *bridge.addr.lock().await;
        let _ = http_client::request(
            addr,
            "DELETE",
            &bridge.path(),
            &[("Mcp-Session-Id", &sid)],
            None,
            Duration::from_secs(3),
        )
        .await;
    }
    Ok(())
}

/// `mcpm bridge`: stdio <-> daemon until stdin closes.
pub fn run_blocking(opts: BridgeOptions) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_with_io(
        opts,
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    ))
}
