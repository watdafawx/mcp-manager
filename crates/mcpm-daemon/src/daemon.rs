//! Daemon lifecycle: single-instance check, bind, `daemon.json`, serve, shutdown.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::coreops::{CoreOps, StubCoreOps};
use crate::http_client;
use crate::logging::Logger;
use crate::pool::{Pool, PoolOptions};
use crate::server::{self, AppState};
use crate::storage::{self, DaemonInfo};
use crate::timeutil;
use crate::tray::{TrayCmd, TrayHandle, TrayHost, TraySnapshot, UpdateView};
use crate::update::{self, Fetcher, HttpFetcher, Service, ServiceCfg, Updater, UpdaterCfg};

/// Reads and writes the "start at sign-in" setting (the registry lives in another crate).
pub trait AutostartCtl: Send + Sync {
    fn is_on(&self) -> bool;
    fn set(&self, on: bool) -> Result<()>;
}

/// Notification-area icon settings.
#[derive(Clone, Default)]
pub struct TrayOptions {
    pub enabled: bool,
    /// The exe started for "Open MCP Manager" (`<exe> gui`).
    pub exe: PathBuf,
    pub autostart: Option<Arc<dyn AutostartCtl>>,
}

/// Self-update settings; off unless the CLI turns it on (tests never update).
pub struct UpdateOptions {
    pub enabled: bool,
    /// The exe to replace and to start again afterwards.
    pub exe: PathBuf,
    /// Arguments for the new daemon (`--no-tray`, `--port N`).
    pub respawn_args: Vec<String>,
    /// Network access; `None` = HTTPS through reqwest.
    pub fetcher: Option<Arc<dyn Fetcher>>,
    /// Releases API base; `None` = `MCPM_UPDATE_URL` or GitHub.
    pub base_url: Option<String>,
    pub service: ServiceCfg,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            exe: PathBuf::new(),
            respawn_args: vec![],
            fetcher: None,
            base_url: None,
            service: ServiceCfg::default(),
        }
    }
}

pub struct DaemonOptions {
    /// State dir (`config.json`, `token`, `daemon.json`, `logs/`).
    pub home: PathBuf,
    /// Overrides the configured port. `Some(0)` picks a free one (tests).
    pub port: Option<u16>,
    pub core: Arc<dyn CoreOps>,
    pub pool: PoolOptions,
    pub log_to_file: bool,
    pub tray: TrayOptions,
    pub update: UpdateOptions,
}

impl DaemonOptions {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            port: None,
            core: Arc::new(StubCoreOps),
            pool: PoolOptions::default(),
            log_to_file: true,
            tray: TrayOptions::default(),
            update: UpdateOptions::default(),
        }
    }
}

pub enum Started {
    Running(RunningDaemon),
    /// Another daemon for the same state dir is already serving.
    AlreadyRunning(DaemonInfo),
}

pub struct RunningDaemon {
    pub port: u16,
    pub token: String,
    pub home: PathBuf,
    state: Arc<AppState>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Probes `/health` of the daemon described by `daemon.json`.
pub async fn probe(home: &std::path::Path) -> Option<DaemonInfo> {
    let info = storage::read_daemon_info(home)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let r = http_client::request(addr, "GET", "/health", &[], None, Duration::from_secs(2))
        .await
        .ok()?;
    let v = r.json()?;
    (r.status == 200
        && v.get("ok") == Some(&serde_json::json!(true))
        && v.get("pid").and_then(|p| p.as_u64()) == Some(info.pid as u64))
    .then_some(info)
}

impl RunningDaemon {
    pub async fn start(opts: DaemonOptions) -> Result<Started> {
        std::fs::create_dir_all(&opts.home)
            .with_context(|| format!("create {}", opts.home.display()))?;
        if let Some(info) = probe(&opts.home).await {
            return Ok(Started::AlreadyRunning(info));
        }
        let log = Arc::new(if opts.log_to_file {
            Logger::open(&storage::log_dir(&opts.home))
        } else {
            Logger::null()
        });
        let cfg = storage::load_config(&opts.home).context("load config.json")?;
        let port = opts.port.unwrap_or(cfg.port);
        let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(l) => l,
            Err(e) => {
                // Lost a start race against another daemon of the same home?
                if let Some(info) = probe(&opts.home).await {
                    return Ok(Started::AlreadyRunning(info));
                }
                log.error(&format!("cannot bind 127.0.0.1:{port}: {e}"));
                return Err(e).with_context(|| format!("cannot bind 127.0.0.1:{port}"));
            }
        };
        let port = listener.local_addr()?.port();
        let token = storage::load_or_create_token(&opts.home)?;
        let started_ms = timeutil::now_ms();
        let pool = Pool::new(opts.home.clone(), cfg, opts.pool, log.clone());
        let (stop, _) = watch::channel(false);
        let shutdown_requested = Arc::new(Notify::new());
        let update = opts.update.enabled.then(|| {
            let fetcher = opts.update.fetcher.clone().unwrap_or_else(|| {
                Arc::new(HttpFetcher {
                    handle: tokio::runtime::Handle::current(),
                })
            });
            Service::new(
                Updater::new(UpdaterCfg {
                    exe: opts.update.exe.clone(),
                    state_dir: opts.home.clone(),
                    current: crate::VERSION.to_string(),
                    base_url: opts
                        .update
                        .base_url
                        .clone()
                        .unwrap_or_else(update::api_base),
                    fetcher,
                }),
                pool.clone(),
                log.clone(),
                shutdown_requested.clone(),
                ServiceCfg {
                    respawn_args: opts.update.respawn_args.clone(),
                    ..opts.update.service
                },
            )
        });
        let state = Arc::new(AppState {
            pool: pool.clone(),
            token: token.clone(),
            core: opts.core,
            log: log.clone(),
            home: opts.home.clone(),
            port,
            started_ms,
            sessions: Mutex::new(Default::default()),
            shutdown_requested,
            update: update.clone(),
            stop,
        });
        storage::write_daemon_info(
            &opts.home,
            &DaemonInfo {
                pid: std::process::id(),
                port,
                started_at: timeutil::iso(started_ms),
            },
        )?;
        log.info(&format!(
            "daemon {} listening on 127.0.0.1:{port} (pid {})",
            crate::VERSION,
            std::process::id()
        ));

        let mut tasks = vec![
            tokio::spawn(server::serve(listener, state.clone())),
            pool.spawn_reaper(),
        ];
        // Forget web sessions nobody used for a day (their SSE streams ended long ago).
        let st = state.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                st.sessions
                    .lock()
                    .unwrap()
                    .retain(|_, s| s.last_seen.elapsed() < Duration::from_secs(24 * 3600));
            }
        }));
        if let Some(u) = update {
            tasks.push(tokio::spawn(u.run()));
        }
        Ok(Started::Running(RunningDaemon {
            port,
            token,
            home: opts.home,
            state,
            tasks: Mutex::new(tasks),
        }))
    }

    pub fn pool(&self) -> &Pool {
        &self.state.pool
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Starts the notification-area icon; `None` when disabled, or the session has no desktop.
    pub fn start_tray(&self, opts: TrayOptions) -> Option<TrayHandle> {
        if !opts.enabled {
            return None;
        }
        let host = Arc::new(DaemonTray {
            state: self.state.clone(),
            handle: tokio::runtime::Handle::current(),
            exe: opts.exe,
            autostart: opts.autostart,
        });
        let tray = TrayHandle::start(host);
        if tray.is_none() {
            self.state
                .log
                .info("no notification-area icon (no desktop in this session)");
        }
        tray
    }

    /// The update service, when updating is on.
    pub fn update_service(&self) -> Option<Arc<Service>> {
        self.state.update.clone()
    }

    /// Resolves when `POST /api/shutdown` was called.
    pub async fn shutdown_requested(&self) {
        self.state.shutdown_requested.notified().await
    }

    /// Stops serving, kills every child, removes `daemon.json`.
    pub async fn shutdown(self) {
        let _ = self.state.stop.send(true);
        self.state.pool.shutdown();
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        storage::remove_daemon_info(&self.home, std::process::id());
        self.state.log.info("daemon stopped");
        // Give killed children a moment to be reaped by their supervisors.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The menu's connection to the daemon.
struct DaemonTray {
    state: Arc<AppState>,
    handle: tokio::runtime::Handle,
    exe: PathBuf,
    autostart: Option<Arc<dyn AutostartCtl>>,
}

impl TrayHost for DaemonTray {
    fn snapshot(&self) -> TraySnapshot {
        let cfg = self.state.pool.config();
        TraySnapshot {
            version: crate::VERSION.to_string(),
            port: self.state.port,
            servers: cfg.servers.len(),
            enabled_servers: cfg.servers.iter().filter(|s| s.enabled).count(),
            running_instances: self.state.pool.running_instances(),
            autostart: self.autostart.as_ref().is_some_and(|a| a.is_on()),
            update: self.state.update.as_ref().map(|u| {
                let st = u.state();
                UpdateView {
                    latest: st.latest,
                    phase: st.phase,
                    waiting_for_idle: st.waiting_for_idle,
                }
            }),
        }
    }

    fn run(&self, cmd: TrayCmd) {
        let st = self.state.clone();
        match cmd {
            TrayCmd::Open => {
                if !crate::tray::focus_gui_window()
                    && let Err(e) = spawn_gui(&self.exe)
                {
                    st.log.warn(&format!("cannot open the window: {e}"));
                }
            }
            TrayCmd::RestartAll => {
                self.handle.spawn(async move {
                    let n = st.pool.restart_all().await;
                    st.log.info(&format!("tray: restarted {n} servers"));
                });
            }
            TrayCmd::CheckUpdates => {
                if let Some(u) = st.update.clone() {
                    self.handle.spawn(async move {
                        u.check().await;
                    });
                }
            }
            TrayCmd::InstallUpdate => {
                if let Some(u) = st.update.clone() {
                    self.handle.spawn(async move {
                        let _ = u.install().await;
                    });
                }
            }
            TrayCmd::SetAutostart(on) => {
                if let Some(a) = &self.autostart
                    && let Err(e) = a.set(on)
                {
                    st.log.warn(&format!("autostart: {e:#}"));
                }
            }
            TrayCmd::Quit => st.shutdown_requested.notify_one(),
        }
    }
}

/// Starts `<exe> gui` as an independent process.
fn spawn_gui(exe: &std::path::Path) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("gui")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn().map(|_| ())
}

pub async fn run(mut opts: DaemonOptions) -> Result<()> {
    let tray_opts = std::mem::take(&mut opts.tray);
    match RunningDaemon::start(opts).await? {
        Started::AlreadyRunning(_) => Ok(()),
        Started::Running(d) => {
            let tray = d.start_tray(tray_opts);
            tokio::select! {
                _ = d.shutdown_requested() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            if let Some(t) = tray {
                t.stop();
            }
            d.shutdown().await;
            Ok(())
        }
    }
}

/// `mcpm daemon`: 2 tokio worker threads, runs until `/api/shutdown` or Ctrl-C.
pub fn run_blocking(opts: DaemonOptions) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    let r = rt.block_on(run(opts));
    // A download or update check still running must not hold the process open.
    rt.shutdown_timeout(Duration::from_secs(2));
    r
}
