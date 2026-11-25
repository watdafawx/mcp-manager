//! Background thread: owns the daemon connection, runs every API call and polls status.
//! The window never blocks on the network; it sends [`Job`]s and receives [`Msg`]s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mcpm_core::model::ServerConfig;
use serde_json::{Value, json};

use crate::autostart;
use crate::client::{self, ApiError, Conn};
use crate::model::{
    CallRow, HarnessRow, HarnessState, Plan, ServerRow, SnapRow, Status, ToolInfo,
};
use crate::state::{Level, Msg, Page, PlanView};

pub const POLL_EVERY: Duration = Duration::from_secs(2);

/// What the window is looking at; decides which extra data the poll fetches.
#[derive(Debug, Clone, Default)]
pub struct PollCtx {
    pub page: Page,
    pub selected: Option<String>,
}

/// Shared between the window and the worker.
#[derive(Debug)]
pub struct Shared {
    /// False while the window is minimized: polling pauses.
    pub visible: AtomicBool,
    pub ctx: Mutex<PollCtx>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { visible: AtomicBool::new(true), ctx: Mutex::new(PollCtx::default()) })
    }
}

#[derive(Debug, Clone)]
pub enum RestoreTarget {
    Harness(String),
    All,
    Snapshot(String),
}

#[derive(Debug, Clone)]
pub enum Job {
    /// Connect (starting the daemon if needed) and load everything.
    Start,
    Refresh,
    SetEnabled { id: String, on: bool },
    Restart(String),
    Remove(String),
    /// Add (`editing == false`) or update a server.
    Save { editing: bool, cfg: Box<ServerConfig> },
    LoadTools(String),
    Reveal(String),
    /// Dry run for one harness id, or `"all"`.
    PlanTakeover(String),
    ApplyTakeover(String),
    Restore { target: RestoreTarget, keep: bool },
    DeleteSnapshot(String),
    SetPort(u16),
    SetAutostart(bool),
    StopDaemon,
    Uninstall,
}

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub struct WorkerCfg {
    pub home: PathBuf,
    /// Executable started as `<exe> daemon`.
    pub exe: PathBuf,
    pub shared: Arc<Shared>,
    pub out: Sender<Msg>,
    pub repaint: Repaint,
}

pub fn spawn(cfg: WorkerCfg) -> Sender<Job> {
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new()
        .name("mcpm-gui-worker".into())
        .spawn(move || Runner::new(cfg).run(rx));
    if let Err(e) = spawned {
        eprintln!("mcpm: cannot start the worker thread: {e}");
    }
    tx
}

struct Runner {
    cfg: WorkerCfg,
    conn: Option<Conn>,
    harnesses: Vec<HarnessRow>,
    tick: u64,
}

fn parse<T: serde::de::DeserializeOwned>(v: Value, what: &str) -> Result<T, ApiError> {
    serde_json::from_value(v).map_err(|e| ApiError::net(format!("unexpected {what} reply: {e}")))
}

impl Runner {
    fn new(cfg: WorkerCfg) -> Self {
        Self { cfg, conn: None, harnesses: Vec::new(), tick: 0 }
    }

    fn send(&self, m: Msg) {
        let _ = self.cfg.out.send(m);
        (self.cfg.repaint)();
    }

    fn toast(&self, level: Level, text: impl Into<String>) {
        self.send(Msg::Toast(level, text.into()));
    }

    fn run(mut self, rx: Receiver<Job>) {
        let mut next_poll = Instant::now() + POLL_EVERY;
        loop {
            let wait = next_poll.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(job) => self.job(job),
                Err(RecvTimeoutError::Timeout) => {
                    next_poll = Instant::now() + POLL_EVERY;
                    if self.cfg.shared.visible.load(Ordering::Relaxed) {
                        self.poll();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    // ---- connection ----

    fn connection(&mut self) -> Result<Conn, ApiError> {
        if let Some(c) = &self.conn {
            return Ok(c.clone());
        }
        let c = client::locate(&self.cfg.home).ok_or_else(|| ApiError::net("the daemon is not running"))?;
        self.conn = Some(c.clone());
        Ok(c)
    }

    /// Runs `f` on the connection; a transport failure marks the daemon offline.
    fn call<T>(&mut self, f: impl FnOnce(&Conn) -> Result<T, ApiError>) -> Result<T, ApiError> {
        let c = match self.connection() {
            Ok(c) => c,
            Err(e) => {
                self.send(Msg::Offline(e.message.clone()));
                return Err(e);
            }
        };
        let r = f(&c);
        if let Err(e) = &r
            && e.is_connect()
        {
            self.conn = None;
            self.send(Msg::Offline(e.message.clone()));
        }
        r
    }

    fn start(&mut self) {
        self.send(Msg::Starting);
        match client::ensure_daemon(&self.cfg.home, &self.cfg.exe, Duration::from_secs(20)) {
            Ok(c) => {
                self.conn = Some(c);
                self.send(Msg::Autostart(autostart::current().unwrap_or(None)));
                self.refresh_all();
            }
            Err(e) => {
                self.conn = None;
                self.send(Msg::Offline(e.message.clone()));
            }
        }
    }

    // ---- reads ----

    fn refresh_status(&mut self) -> Result<(), ApiError> {
        let s: Status = parse(self.call(|c| c.get("/api/status"))?, "status")?;
        self.send(Msg::Status(s));
        Ok(())
    }

    fn refresh_servers(&mut self) -> Result<(), ApiError> {
        let v = self.call(|c| c.get("/api/servers"))?;
        self.send(Msg::Servers(ServerRow::from_list(&v)));
        Ok(())
    }

    fn refresh_harnesses(&mut self) -> Result<(), ApiError> {
        let v = self.call(|c| c.get("/api/harnesses"))?;
        self.harnesses = HarnessRow::from_list(&v);
        self.send(Msg::Harnesses(self.harnesses.clone()));
        Ok(())
    }

    fn refresh_snapshots(&mut self) -> Result<(), ApiError> {
        let v = self.call(|c| c.get("/api/snapshots"))?;
        self.send(Msg::Snapshots(SnapRow::from_list(&v)));
        Ok(())
    }

    fn refresh_calls(&mut self) -> Result<(), ApiError> {
        let v = self.call(|c| c.get("/api/calls?limit=100"))?;
        self.send(Msg::Calls(CallRow::from_list(&v)));
        Ok(())
    }

    fn refresh_all(&mut self) {
        let r = self
            .refresh_status()
            .and_then(|_| self.refresh_servers())
            .and_then(|_| self.refresh_harnesses())
            .and_then(|_| self.refresh_snapshots())
            .and_then(|_| self.refresh_calls());
        self.report(r);
    }

    /// Transport errors already produced an `Offline`; show the rest as toasts.
    fn report(&self, r: Result<(), ApiError>) {
        if let Err(e) = r
            && !e.is_connect()
        {
            self.toast(Level::Error, e.to_string());
        }
    }

    fn poll(&mut self) {
        self.tick += 1;
        if self.conn.is_none() && client::locate(&self.cfg.home).is_none() {
            return; // stays offline until the user starts the daemon
        }
        let ctx = self.cfg.shared.ctx.lock().map(|c| c.clone()).unwrap_or_default();
        let mut r = self.refresh_status().and_then(|_| self.refresh_servers());
        if r.is_ok() {
            // File-reading views refresh less often than the live server state.
            let slow = self.tick.is_multiple_of(3);
            if matches!(ctx.page, Page::Harnesses | Page::Servers) && (slow || self.harnesses.is_empty()) {
                r = self.refresh_harnesses();
            }
            if r.is_ok() && ctx.page == Page::Snapshots && slow {
                r = self.refresh_snapshots();
            }
            if r.is_ok() && ctx.page == Page::Servers && ctx.selected.is_some() {
                r = self.refresh_calls();
            }
        }
        self.report(r);
    }

    // ---- jobs ----

    fn job(&mut self, job: Job) {
        if matches!(job, Job::Start) {
            self.send(Msg::Busy(1));
            self.start();
            self.send(Msg::Busy(-1));
            return;
        }
        self.send(Msg::Busy(1));
        // `Ok(Some(text))` = success toast; mutating jobs refresh everything afterwards.
        let (result, mutating) = self.run_job(job);
        match result {
            Ok(Some(text)) => self.toast(Level::Info, text),
            Ok(None) => {}
            Err(e) if e.is_connect() => {}
            Err(e) => self.toast(Level::Error, e.to_string()),
        }
        if mutating {
            self.refresh_all();
        }
        self.send(Msg::Busy(-1));
    }

    fn run_job(&mut self, job: Job) -> (Result<Option<String>, ApiError>, bool) {
        match job {
            Job::Start => (Ok(None), false),
            Job::Refresh => (Ok(None), true),
            Job::SetEnabled { id, on } => {
                let verb = if on { "enable" } else { "disable" };
                let r = self.call(|c| c.post(&format!("/api/servers/{id}/{verb}"), &json!({})));
                (r.map(|_| None), true)
            }
            Job::Restart(id) => {
                let r = self.call(|c| c.slow().post(&format!("/api/servers/{id}/restart"), &json!({})));
                (r.map(|_| Some(format!("Restarted {id}"))), true)
            }
            Job::Remove(id) => {
                let r = self.call(|c| c.delete(&format!("/api/servers/{id}")));
                (r.map(|_| Some(format!("Removed {id}"))), true)
            }
            Job::Save { editing, cfg } => {
                let body = serde_json::to_value(&*cfg).unwrap_or_default();
                let id = cfg.id.clone();
                let r = if editing {
                    self.call(|c| c.put(&format!("/api/servers/{id}"), &body))
                } else {
                    self.call(|c| c.post("/api/servers", &body))
                };
                let text = if editing { format!("Saved {id}") } else { format!("Added {id}") };
                (r.map(|_| Some(text)), true)
            }
            Job::LoadTools(id) => {
                let r = self.call(|c| c.slow().get(&format!("/api/servers/{id}/tools")));
                let result = match r {
                    Ok(v) => Ok(ToolInfo::from_list(&v)),
                    Err(e) => Err(e.message),
                };
                self.send(Msg::Tools { id, result });
                (Ok(None), true)
            }
            Job::Reveal(id) => {
                let r = self.call(|c| c.get(&format!("/api/servers/{id}?reveal=1")));
                let r = r.and_then(|v| parse::<ServerConfig>(v, "server"));
                match r {
                    Ok(cfg) => {
                        self.send(Msg::Revealed { id, cfg: Box::new(cfg) });
                        (Ok(None), false)
                    }
                    Err(e) => (Err(e), false),
                }
            }
            Job::PlanTakeover(target) => (self.plan(&target), false),
            Job::ApplyTakeover(target) => {
                let body = if target == "all" { json!({ "all": true }) } else { json!({ "harness": target }) };
                let r = self.call(|c| c.slow().post("/api/takeover/apply", &body));
                let text = if target == "all" {
                    "Took over every detected harness".to_string()
                } else {
                    format!("Took over {target}")
                };
                (r.map(|_| Some(text)), true)
            }
            Job::Restore { target, keep } => {
                let mut body = match &target {
                    RestoreTarget::Harness(h) => json!({ "harness": h }),
                    RestoreTarget::All => json!({ "all": true }),
                    RestoreTarget::Snapshot(s) => json!({ "snapshot": s }),
                };
                body["keepInManager"] = json!(keep);
                let r = self.call(|c| c.slow().post("/api/restore", &body));
                (r.map(|v| Some(restore_summary(&v))), true)
            }
            Job::DeleteSnapshot(id) => {
                let r = self.call(|c| c.delete(&format!("/api/snapshots/{id}")));
                (r.map(|_| Some("Snapshot deleted".to_string())), true)
            }
            Job::SetPort(port) => (self.set_port(port), true),
            Job::SetAutostart(on) => {
                let r = std::env::current_exe()
                    .map_err(anyhow::Error::from)
                    .and_then(|exe| autostart::set(on, &exe));
                self.send(Msg::Autostart(autostart::current().unwrap_or(None)));
                match r {
                    Ok(()) => (Ok(Some(format!("Autostart {}", if on { "on" } else { "off" }))), false),
                    Err(e) => (Err(ApiError::net(format!("autostart: {e:#}"))), false),
                }
            }
            Job::StopDaemon => {
                let r = self.call(|c| c.post("/api/shutdown", &json!({})));
                self.wait_gone();
                self.conn = None;
                self.send(Msg::Offline("the daemon was stopped".into()));
                (r.map(|_| Some("Daemon stopped".to_string())), false)
            }
            Job::Uninstall => (self.uninstall(), false),
        }
    }

    fn plan(&mut self, target: &str) -> Result<Option<String>, ApiError> {
        let ids: Vec<String> = if target == "all" {
            self.harnesses
                .iter()
                .filter(|h| h.detected && h.state != HarnessState::TakenOver)
                .map(|h| h.id.clone())
                .collect()
        } else {
            vec![target.to_string()]
        };
        let mut view = PlanView { target: target.to_string(), plans: vec![], errors: vec![] };
        for id in ids {
            match self.call(|c| c.post("/api/takeover/plan", &json!({ "harness": id }))) {
                Ok(v) => match Plan::from_value(&v) {
                    Some(p) => view.plans.push(p),
                    None => view.errors.push((id, "unexpected plan reply".into())),
                },
                Err(e) if e.is_connect() => return Err(e),
                Err(e) => view.errors.push((id, e.message)),
            }
        }
        self.send(Msg::Plan(view));
        Ok(None)
    }

    fn wait_gone(&self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if client::locate(&self.cfg.home).is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Stop the daemon, rewrite the port in `config.json` (nothing else is running to race
    /// with), start it again.
    fn set_port(&mut self, port: u16) -> Result<Option<String>, ApiError> {
        if port < 1024 {
            return Err(ApiError::net("choose a port between 1024 and 65535"));
        }
        self.call(|c| c.post("/api/shutdown", &json!({})))?;
        self.wait_gone();
        self.conn = None;
        let dir = &self.cfg.home;
        let saved = mcpm_core::store::load(dir).and_then(|mut cfg| {
            cfg.port = port;
            mcpm_core::store::save(dir, &cfg)
        });
        if let Err(e) = saved {
            self.start();
            return Err(ApiError::net(format!("cannot save the port: {e:#}")));
        }
        self.start();
        Ok(Some(format!("Port is now {port}")))
    }

    /// Restore every harness, remove autostart, stop the daemon, then close the window.
    fn uninstall(&mut self) -> Result<Option<String>, ApiError> {
        self.call(|c| {
            c.slow().post("/api/restore", &json!({ "all": true, "keepInManager": false }))
        })?;
        autostart::set(false, std::path::Path::new(""))
            .map_err(|e| ApiError::net(format!("autostart: {e:#}")))?;
        let _ = self.call(|c| c.post("/api/shutdown", &json!({})));
        self.wait_gone();
        self.send(Msg::Quit);
        Ok(Some("Everything restored".into()))
    }
}

/// `[{"path":..,"merged":bool}]` -> "Restored 2 files (1 merged)".
pub fn restore_summary(v: &Value) -> String {
    let files = v.as_array().map(Vec::len).unwrap_or(0);
    let merged = v
        .as_array()
        .map(|a| a.iter().filter(|r| r.get("merged").and_then(Value::as_bool) == Some(true)).count())
        .unwrap_or(0);
    let mut s = format!("Restored {files} file{}", if files == 1 { "" } else { "s" });
    if merged > 0 {
        s.push_str(&format!(" ({merged} merged with newer edits)"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_summary_counts() {
        assert_eq!(restore_summary(&json!([])), "Restored 0 files");
        assert_eq!(
            restore_summary(&json!([{"path":"a","merged":false},{"path":"b","merged":true}])),
            "Restored 2 files (1 merged with newer edits)"
        );
        assert_eq!(restore_summary(&json!([{"path":"a","merged":false}])), "Restored 1 file");
    }
}
