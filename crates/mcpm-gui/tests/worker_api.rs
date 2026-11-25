//! Drives the GUI worker against a real in-process daemon (temp dirs only, no real harness files)
//! and feeds every message through the reducer, exactly as the window does.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use mcpm_core::Core;
use mcpm_core::model::{ServerConfig, Transport};
use mcpm_daemon::{DaemonOptions, RealCoreOps, RunningDaemon, Started};
use mcpm_gui::model::{HarnessState, RunState};
use mcpm_gui::state::{AppData, ConnState, Level, Msg};
use mcpm_gui::worker::{self, Job, RestoreTarget, Shared, WorkerCfg};

const ORIGINAL: &str =
    "{\n  \"mcpServers\": { \"fs\": { \"command\": \"npx\", \"args\": [\"fs\"] } }\n}\n";

struct Rig {
    _dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
    _daemon: RunningDaemon,
    core: Core,
    cursor: std::path::PathBuf,
    tx: std::sync::mpsc::Sender<Job>,
    rx: Receiver<Msg>,
    data: AppData,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let core = Core {
        home: dir.path().join("home"),
        appdata: dir.path().join("appdata"),
        state_dir: dir.path().join("state"),
    };
    let cursor = core.home.join(".cursor").join("mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, ORIGINAL).unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut opts = DaemonOptions::new(core.state_dir.clone());
    opts.port = Some(0);
    opts.log_to_file = false;
    opts.core = Arc::new(RealCoreOps::new(
        core.clone(),
        Path::new("mcpm.exe").to_path_buf(),
    ));
    let Started::Running(daemon) = rt.block_on(RunningDaemon::start(opts)).unwrap() else {
        panic!("already running")
    };

    let (out, rx) = channel();
    let tx = worker::spawn(WorkerCfg {
        home: core.state_dir.clone(),
        exe: Path::new("mcpm.exe").to_path_buf(),
        shared: Shared::new(),
        out,
        repaint: Arc::new(|| {}),
    });
    Rig {
        _dir: dir,
        _rt: rt,
        _daemon: daemon,
        core,
        cursor,
        tx,
        rx,
        data: AppData::default(),
    }
}

impl Rig {
    /// Applies messages until `done` holds and the worker is idle.
    fn pump(&mut self, what: &str, done: impl Fn(&AppData) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if done(&self.data) && self.data.busy == 0 {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for: {what}; toasts: {:?}",
                self.data.toasts
            );
            if let Ok(m) = self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                self.data.apply(m);
            }
        }
    }

    fn job(&mut self, j: Job, what: &str, done: impl Fn(&AppData) -> bool) {
        self.tx.send(j).unwrap();
        self.pump(what, done);
    }

    fn no_errors(&self) {
        let errs: Vec<_> = self
            .data
            .toasts
            .iter()
            .filter(|t| t.level == Level::Error)
            .collect();
        assert!(errs.is_empty(), "unexpected error toasts: {errs:?}");
    }
}

fn fs_server() -> ServerConfig {
    ServerConfig {
        id: "echo".into(),
        name: "Echo".into(),
        transport: Transport::Http {
            url: "http://127.0.0.1:1/mcp".into(),
            headers: [("X-Key".to_string(), "secret".to_string())].into(),
        },
        enabled: true,
        mode: Default::default(),
        pinned_tools: vec![],
        harnesses: vec![],
        start_timeout_secs: None,
        origin: None,
    }
}

#[test]
fn takeover_restore_and_server_edits_through_the_worker() {
    let mut r = rig();

    // Start: connects, loads status, servers, harnesses, snapshots.
    r.job(Job::Start, "initial load", |d| {
        d.conn == ConnState::Online && !d.harnesses.is_empty()
    });
    let cursor = r.data.harness("cursor").expect("cursor harness listed");
    assert!(cursor.detected);
    assert_eq!(cursor.state, HarnessState::NotConnected);
    assert_eq!(cursor.foreign_entries(), vec!["fs"]);
    assert!(r.data.status.as_ref().unwrap().port > 0);

    // Dry run shows the plan and writes nothing.
    r.job(Job::PlanTakeover("cursor".into()), "plan", |d| {
        d.plan.is_some()
    });
    let plan = r.data.plan.clone().unwrap();
    assert_eq!(plan.plans[0].import_count(), 1);
    assert_eq!(plan.plans[0].remove_count(), 1);
    assert!(plan.plans[0].files[0].final_text.contains("\"mcpm\""));
    assert_eq!(std::fs::read_to_string(&r.cursor).unwrap(), ORIGINAL);

    // Apply: harness taken over, server imported, snapshot kept.
    r.job(Job::ApplyTakeover("cursor".into()), "takeover", |d| {
        d.harness("cursor")
            .is_some_and(|h| h.state == HarnessState::TakenOver)
            && !d.servers.is_empty()
            && !d.snapshots.is_empty()
    });
    assert_eq!(r.data.servers[0].cfg.id, "fs");
    assert_eq!(r.data.snapshots[0].operation, "takeover");
    assert!(std::fs::read_to_string(&r.cursor).unwrap().contains("mcpm"));

    // The stored bytes are the original file.
    let snap = &r.data.snapshots[0];
    let s = mcpm_core::snapshot::load(&r.core.state_dir, &snap.id).unwrap();
    let stored = mcpm_core::snapshot::bytes(&r.core.state_dir, &s, 0).unwrap();
    assert_eq!(String::from_utf8(stored).unwrap(), ORIGINAL);

    // Add a server with a secret header: masked in the list, real on reveal.
    r.job(
        Job::Save {
            editing: false,
            cfg: Box::new(fs_server()),
        },
        "add",
        |d| d.server("echo").is_some(),
    );
    let row = r.data.server("echo").unwrap();
    assert_eq!(row.state, RunState::Idle);
    let Transport::Http { headers, .. } = &row.cfg.transport else {
        panic!()
    };
    assert_eq!(headers["X-Key"], mcpm_daemon::mask::MASK);
    r.job(Job::Reveal("echo".into()), "reveal", |d| {
        d.revealed.contains_key("echo")
    });
    let Transport::Http { headers, .. } = &r.data.revealed["echo"].transport else {
        panic!()
    };
    assert_eq!(headers["X-Key"], "secret");

    // Enable switch and exposure edit (masked values survive the round trip).
    r.job(
        Job::SetEnabled {
            id: "echo".into(),
            on: false,
        },
        "disable",
        |d| d.server("echo").is_some_and(|s| !s.cfg.enabled),
    );
    assert_eq!(r.data.server("echo").unwrap().state, RunState::Disabled);
    let mut edited = r.data.server("echo").unwrap().cfg.clone();
    edited.harnesses = vec!["cursor".into()];
    r.job(
        Job::Save {
            editing: true,
            cfg: Box::new(edited),
        },
        "exposure",
        |d| {
            d.server("echo")
                .is_some_and(|s| s.cfg.harnesses == vec!["cursor".to_string()])
        },
    );
    r.job(Job::Reveal("echo".into()), "reveal again", |d| {
        d.revealed.contains_key("echo")
    });
    let Transport::Http { headers, .. } = &r.data.revealed["echo"].transport else {
        panic!()
    };
    assert_eq!(
        headers["X-Key"], "secret",
        "saving masked values must keep the stored secret"
    );

    // Remove, then restore the harness: file is byte-identical again.
    r.job(Job::Remove("echo".into()), "remove", |d| {
        d.server("echo").is_none()
    });
    r.job(
        Job::Restore {
            target: RestoreTarget::Harness("cursor".into()),
            keep: false,
        },
        "restore",
        |d| {
            d.harness("cursor")
                .is_some_and(|h| h.state == HarnessState::NotConnected)
                && d.servers.is_empty()
        },
    );
    assert_eq!(std::fs::read_to_string(&r.cursor).unwrap(), ORIGINAL);
    assert!(!r.data.snapshots.is_empty());

    // Errors come back as toasts, never panics.
    r.job(Job::Remove("ghost".into()), "error toast", |d| {
        d.toasts.iter().any(|t| t.level == Level::Error)
    });
    assert!(
        r.data
            .toasts
            .iter()
            .any(|t| t.text.contains("unknown server")),
        "{:?}",
        r.data.toasts
    );
    r.data.toasts.clear();
    r.no_errors();
}

#[test]
fn offline_worker_reports_instead_of_panicking() {
    // No daemon at all: the worker must say so instead of panicking.
    let dir = tempfile::tempdir().unwrap();
    let (out, rx) = channel();
    let tx = worker::spawn(WorkerCfg {
        home: dir.path().to_path_buf(),
        exe: dir.path().join("does-not-exist.exe"),
        shared: Shared::new(),
        out,
        repaint: Arc::new(|| {}),
    });
    tx.send(Job::Refresh).unwrap();
    let mut data = AppData::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(data.conn, ConnState::Offline(_)) && Instant::now() < deadline {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(200)) {
            data.apply(m);
        }
    }
    assert!(
        matches!(data.conn, ConnState::Offline(_)),
        "{:?}",
        data.conn
    );
}
