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

pub struct DaemonOptions {
    /// State dir (`config.json`, `token`, `daemon.json`, `logs/`).
    pub home: PathBuf,
    /// Overrides the configured port. `Some(0)` picks a free one (tests).
    pub port: Option<u16>,
    pub core: Arc<dyn CoreOps>,
    pub pool: PoolOptions,
    pub log_to_file: bool,
}

impl DaemonOptions {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            port: None,
            core: Arc::new(StubCoreOps),
            pool: PoolOptions::default(),
            log_to_file: true,
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
        let state = Arc::new(AppState {
            pool: pool.clone(),
            token: token.clone(),
            core: opts.core,
            log: log.clone(),
            home: opts.home.clone(),
            port,
            started_ms,
            sessions: Mutex::new(Default::default()),
            shutdown_requested: Notify::new(),
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

pub async fn run(opts: DaemonOptions) -> Result<()> {
    match RunningDaemon::start(opts).await? {
        Started::AlreadyRunning(_) => Ok(()),
        Started::Running(d) => {
            tokio::select! {
                _ = d.shutdown_requested() => {}
                _ = tokio::signal::ctrl_c() => {}
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
    rt.block_on(run(opts))
}
