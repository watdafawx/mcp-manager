//! Self-update from GitHub releases.
//!
//! Flow: ask `<base>/releases/latest` (no token) -> compare the tag with this build -> download
//! `mcpm.exe` into `<state>/updates/` -> check size and sha256 against the release's
//! `mcpm.exe.sha256` asset -> swap files (the running exe is renamed to `mcpm.exe.old`, the new
//! one takes its name) -> the daemon starts the new exe and exits. The old file is deleted by
//! the next start.
//!
//! [`Updater`] holds the state machine and the file work and is blocking (run it on a blocking
//! thread); [`Service`] ties it to a running daemon (schedule, idle wait, restart). The network is
//! behind [`Fetcher`] so tests use a fake.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::logging::Logger;
use crate::pool::Pool;
use crate::timeutil;

/// Where releases are asked from (override for tests with env `MCPM_UPDATE_URL`).
pub const REPO_API: &str = "https://api.github.com/repos/watdafawx/mcp-manager";
pub const EXE_ASSET: &str = "mcpm.exe";
pub const SHA_ASSET: &str = "mcpm.exe.sha256";
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Refuse absurd downloads (the exe is a few MB).
const MAX_EXE: u64 = 256 * 1024 * 1024;

/// Base URL of the releases API: `MCPM_UPDATE_URL` when set (hidden, for tests), else GitHub.
pub fn api_base() -> String {
    match std::env::var("MCPM_UPDATE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => REPO_API.to_string(),
    }
}

// ---- versions ---------------------------------------------------------------------------------

/// "1.2.10" as numbers; a leading "v" and any "-beta"/"+build" tail are ignored.
fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Total order on versions ("1.0" equals "1.0.0").
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len()))
        .map(|i| {
            a.get(i)
                .copied()
                .unwrap_or(0)
                .cmp(&b.get(i).copied().unwrap_or(0))
        })
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current).is_gt()
}

// ---- release JSON -----------------------------------------------------------------------------

/// Where the expected checksum comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Checksum {
    /// URL of the `mcpm.exe.sha256` asset.
    Asset(String),
    /// `sha256:<hex>` digest GitHub lists on the exe asset itself.
    Digest(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub published: String,
    pub exe_url: String,
    pub exe_size: u64,
    pub checksum: Option<Checksum>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// The installable release described by a `releases/latest` reply. `None` for drafts,
/// prereleases and releases without an `mcpm.exe` asset.
pub fn parse_release(json: &str) -> Result<Option<Release>> {
    let r: GhRelease = serde_json::from_str(json).context("unexpected release reply")?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let Some(exe) = r.assets.iter().find(|a| a.name == EXE_ASSET) else {
        return Ok(None);
    };
    let checksum = r
        .assets
        .iter()
        .find(|a| a.name == SHA_ASSET)
        .map(|a| Checksum::Asset(a.browser_download_url.clone()))
        .or_else(|| {
            exe.digest
                .as_deref()
                .and_then(|d| d.strip_prefix("sha256:"))
                .map(|h| Checksum::Digest(h.trim().to_ascii_lowercase()))
        });
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_string(),
        notes: r.body.unwrap_or_default(),
        published: r.published_at.unwrap_or_default(),
        exe_url: exe.browser_download_url.clone(),
        exe_size: exe.size,
        checksum,
    }))
}

/// First token of a `sha256sum`-style file (`<64 hex>  mcpm.exe`), lowercased.
pub fn parse_sha256_text(text: &str) -> Option<String> {
    let tok = text.split_whitespace().next()?;
    (tok.len() == 64 && tok.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| tok.to_ascii_lowercase())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Size, program header and sha256 of a downloaded file.
pub fn verify_download(path: &Path, size: u64, sha256: &str) -> Result<()> {
    let len = std::fs::metadata(path)?.len();
    if size != 0 && len != size {
        bail!("the download is {len} bytes, the release lists {size}");
    }
    if len > MAX_EXE {
        bail!("the download is too big");
    }
    let mut head = [0u8; 2];
    std::fs::File::open(path)?.read_exact(&mut head).ok();
    if &head != b"MZ" {
        bail!("the download is not a Windows program");
    }
    let got = sha256_file(path)?;
    if !got.eq_ignore_ascii_case(sha256) {
        bail!("the download does not match its published sha256");
    }
    Ok(())
}

// ---- network ----------------------------------------------------------------------------------

/// What the updater needs from the network. Blocking: call it from a blocking thread.
pub trait Fetcher: Send + Sync {
    /// GET a small text body (release JSON, checksum file).
    fn get_text(&self, url: &str) -> Result<String>;
    /// GET `url` into `dest`; `progress` gets the bytes written so far.
    fn download(&self, url: &str, dest: &Path, progress: &mut dyn FnMut(u64)) -> Result<()>;
}

/// reqwest on the daemon's tokio runtime (`handle`); the client lives only for one call so an
/// idle daemon holds no TLS state.
pub struct HttpFetcher {
    pub handle: tokio::runtime::Handle,
}

impl HttpFetcher {
    fn client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .user_agent(format!("mcpm/{}", crate::VERSION))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .context("build http client")
    }
}

impl Fetcher for HttpFetcher {
    fn get_text(&self, url: &str) -> Result<String> {
        let client = self.client()?;
        self.handle.block_on(async {
            let mut r = client
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .with_context(|| format!("GET {url}"))?
                .error_for_status()?;
            let mut body = Vec::new();
            while let Some(c) = r.chunk().await? {
                body.extend_from_slice(&c);
                if body.len() > 4 * 1024 * 1024 {
                    bail!("reply too large");
                }
            }
            Ok(String::from_utf8_lossy(&body).into_owned())
        })
    }

    fn download(&self, url: &str, dest: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
        use std::io::Write;
        let client = self.client()?;
        self.handle.block_on(async {
            let mut r = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?
                .error_for_status()?;
            let mut f = std::fs::File::create(dest)
                .with_context(|| format!("create {}", dest.display()))?;
            let mut n = 0u64;
            while let Some(c) = r.chunk().await? {
                n += c.len() as u64;
                if n > MAX_EXE {
                    bail!("the download is too big");
                }
                f.write_all(&c)?;
                progress(n);
            }
            f.sync_all()?;
            Ok(())
        })
    }
}

// ---- state ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    Downloading,
    Installing,
}

/// What `GET /api/update` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    pub current: String,
    /// Version of the newest release that is newer than `current`.
    pub latest: Option<String>,
    pub available: bool,
    pub phase: Phase,
    pub notes: Option<String>,
    pub last_check: Option<String>,
    pub last_error: Option<String>,
    pub auto_update: bool,
    pub downloaded: u64,
    pub size: u64,
    /// An update is ready but waits for running tool calls to finish.
    pub waiting_for_idle: bool,
}

#[derive(Default)]
struct Inner {
    phase: Phase,
    release: Option<Release>,
    last_check_ms: Option<u64>,
    last_error: Option<String>,
    downloaded: u64,
    waiting_for_idle: bool,
}

pub struct UpdaterCfg {
    /// The exe to replace (the running one).
    pub exe: PathBuf,
    /// State dir; downloads go to `<state>/updates/`.
    pub state_dir: PathBuf,
    pub current: String,
    pub base_url: String,
    pub fetcher: Arc<dyn Fetcher>,
}

pub struct Updater {
    cfg: UpdaterCfg,
    inner: Mutex<Inner>,
}

/// `<exe><suffix>` next to the exe: `mcpm.exe.old`, `mcpm.exe.new`.
pub fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

impl Updater {
    pub fn new(cfg: UpdaterCfg) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            inner: Mutex::new(Inner::default()),
        })
    }

    pub fn exe(&self) -> &Path {
        &self.cfg.exe
    }

    pub fn updates_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("updates")
    }

    /// Snapshot of the state; `auto_update` comes from the config.
    pub fn state(&self, auto_update: bool) -> UpdateState {
        let i = self.inner.lock().unwrap();
        let rel = i.release.as_ref();
        UpdateState {
            current: self.cfg.current.clone(),
            latest: rel.map(|r| r.version.clone()),
            available: rel.is_some(),
            phase: i.phase,
            notes: rel
                .map(|r| r.notes.trim().to_string())
                .filter(|n| !n.is_empty()),
            last_check: i.last_check_ms.map(timeutil::iso),
            last_error: i.last_error.clone(),
            auto_update,
            downloaded: i.downloaded,
            size: rel.map_or(0, |r| r.exe_size),
            waiting_for_idle: i.waiting_for_idle,
        }
    }

    pub fn available_version(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .release
            .as_ref()
            .map(|r| r.version.clone())
    }

    pub fn set_waiting(&self, waiting: bool) {
        self.inner.lock().unwrap().waiting_for_idle = waiting;
    }

    /// Moves to `phase` unless something else is running. `false` = busy.
    fn begin(&self, phase: Phase) -> bool {
        let mut i = self.inner.lock().unwrap();
        if i.phase != Phase::Idle {
            return false;
        }
        i.phase = phase;
        i.downloaded = 0;
        true
    }

    fn finish(&self, error: Option<String>) {
        let mut i = self.inner.lock().unwrap();
        i.phase = Phase::Idle;
        i.last_error = error;
    }

    /// Asks the release API. Returns the newer release, if any. A reply that fails to parse or
    /// a network error is stored as `last_error` and returned.
    pub fn check(&self) -> Result<Option<Release>> {
        if !self.begin(Phase::Checking) {
            return Ok(self.inner.lock().unwrap().release.clone());
        }
        let r = self.check_inner();
        let mut i = self.inner.lock().unwrap();
        i.phase = Phase::Idle;
        i.last_check_ms = Some(timeutil::now_ms());
        match &r {
            Ok(rel) => {
                i.last_error = None;
                i.release = rel.clone();
            }
            Err(e) => i.last_error = Some(format!("{e:#}")),
        }
        r
    }

    fn check_inner(&self) -> Result<Option<Release>> {
        let url = format!(
            "{}/releases/latest",
            self.cfg.base_url.trim_end_matches('/')
        );
        let text = self.cfg.fetcher.get_text(&url)?;
        Ok(parse_release(&text)?.filter(|r| is_newer(&r.version, &self.cfg.current)))
    }

    /// Downloads the known release into `updates/` and verifies it. Returns the verified file.
    pub fn download(&self) -> Result<PathBuf> {
        let rel = self
            .inner
            .lock()
            .unwrap()
            .release
            .clone()
            .ok_or_else(|| anyhow!("no update is available"))?;
        if !self.begin(Phase::Downloading) {
            bail!("an update is already in progress");
        }
        let r = self.download_inner(&rel);
        match &r {
            Ok(_) => self.finish(None),
            Err(e) => self.finish(Some(format!("{e:#}"))),
        }
        r
    }

    fn download_inner(&self, rel: &Release) -> Result<PathBuf> {
        let dir = self.updates_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let want = match &rel.checksum {
            Some(Checksum::Asset(url)) => {
                let text = self.cfg.fetcher.get_text(url)?;
                parse_sha256_text(&text).ok_or_else(|| anyhow!("{SHA_ASSET} is not a sha256"))?
            }
            Some(Checksum::Digest(h)) => {
                parse_sha256_text(h).ok_or_else(|| anyhow!("the listed digest is not a sha256"))?
            }
            None => bail!("the release publishes no checksum ({SHA_ASSET}), not installing"),
        };
        let final_path = dir.join(format!("mcpm-{}.exe", rel.version));
        // A verified copy from an earlier attempt is reused.
        if final_path.is_file() && verify_download(&final_path, rel.exe_size, &want).is_ok() {
            return Ok(final_path);
        }
        let part = dir.join(format!("mcpm-{}.exe.part", rel.version));
        let _ = std::fs::remove_file(&part);
        let mut progress = |n: u64| self.inner.lock().unwrap().downloaded = n;
        let got = self
            .cfg
            .fetcher
            .download(&rel.exe_url, &part, &mut progress);
        let checked = got.and_then(|_| verify_download(&part, rel.exe_size, &want));
        if let Err(e) = checked {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        let _ = std::fs::remove_file(&final_path);
        std::fs::rename(&part, &final_path).context("keep the download")?;
        Ok(final_path)
    }

    /// Puts `staged` in place of the running exe (see [`swap_in`]).
    pub fn install(&self, staged: &Path) -> Result<()> {
        if !self.begin(Phase::Installing) {
            bail!("an update is already in progress");
        }
        let r = swap_in(&self.cfg.exe, staged);
        match &r {
            Ok(()) => self.finish(None),
            Err(e) => self.finish(Some(format!("{e:#}"))),
        }
        r
    }

    /// Download, verify and swap. Returns the version that was installed.
    pub fn apply(&self) -> Result<String> {
        let staged = self.download()?;
        self.install(&staged)?;
        let v = self
            .available_version()
            .ok_or_else(|| anyhow!("no update is available"))?;
        // The update is now the running exe's successor: no longer "available".
        self.inner.lock().unwrap().release = None;
        // The staged copy is no longer needed.
        let _ = std::fs::remove_file(&staged);
        Ok(v)
    }
}

// ---- file swap --------------------------------------------------------------------------------

/// Renames, retrying for a while: antivirus scans a fresh exe and holds it open, which fails a
/// rename with "being used by another process" for a few seconds.
fn rename_retrying(from: &Path, to: &Path, tries: u32) -> std::io::Result<()> {
    let mut last = None;
    for i in 0..tries {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            // 32 sharing violation, 5 access denied
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(5)) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(100 + 25 * u64::from(i)));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename kept failing")))
}

/// Puts the file `staged` in place of `exe`, which may be running: the new bytes are copied next
/// to it (`<exe>.new`, so the final step is a same-volume rename), `exe` is renamed to
/// `<exe>.old`, then `.new` takes the name. Any failure puts the old exe back.
pub fn swap_in(exe: &Path, staged: &Path) -> Result<()> {
    let next = beside(exe, ".new");
    let old = beside(exe, ".old");
    let _ = std::fs::remove_file(&next);
    std::fs::copy(staged, &next).context("copy the update next to the exe")?;
    if old.exists() && std::fs::remove_file(&old).is_err() {
        let _ = std::fs::remove_file(&next);
        bail!("the previous update is still being cleaned up, try again in a moment");
    }
    if let Err(e) = rename_retrying(exe, &old, 40) {
        let _ = std::fs::remove_file(&next);
        return Err(e).context("move the running exe aside");
    }
    if let Err(e) = rename_retrying(&next, exe, 40) {
        let _ = rename_retrying(&old, exe, 40);
        let _ = std::fs::remove_file(&next);
        return Err(e).context("put the update in place");
    }
    Ok(())
}

/// Removes what an earlier update left beside `exe` and old downloads. Best effort.
pub fn clean_up(exe: &Path, state_dir: &Path) {
    let _ = std::fs::remove_file(beside(exe, ".old"));
    let _ = std::fs::remove_file(beside(exe, ".new"));
    if let Ok(rd) = std::fs::read_dir(state_dir.join("updates")) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ---- process handover -------------------------------------------------------------------------

/// Starts `<exe> daemon <args> --wait-pid <pid>` detached: the new daemon waits for this
/// process to exit before it binds the port.
pub fn spawn_successor(exe: &Path, args: &[String], wait_pid: u32) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .args(args)
        .arg("--wait-pid")
        .arg(wait_pid.to_string())
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
        if cmd
            .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        cmd.creation_flags(flags).spawn().map(|_| ())
    }
    #[cfg(not(windows))]
    cmd.spawn().map(|_| ())
}

/// Blocks until process `pid` has exited (or `timeout` passed). Returns at once when it is gone
/// already.
#[cfg(windows)]
pub fn wait_for_pid(pid: u32, timeout: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return;
        }
        WaitForSingleObject(h, timeout.as_millis().min(u32::MAX as u128 - 1) as u32);
        CloseHandle(h);
    }
}

#[cfg(not(windows))]
pub fn wait_for_pid(_pid: u32, _timeout: Duration) {}

// ---- daemon service ---------------------------------------------------------------------------

pub struct ServiceCfg {
    /// Arguments the successor daemon is started with (`--no-tray`, `--port N`).
    pub respawn_args: Vec<String>,
    pub first_check_delay: Duration,
    pub check_every: Duration,
    /// How often a ready update re-checks for running tool calls.
    pub idle_poll: Duration,
}

impl Default for ServiceCfg {
    fn default() -> Self {
        Self {
            respawn_args: vec![],
            first_check_delay: Duration::from_secs(20),
            check_every: CHECK_EVERY,
            idle_poll: Duration::from_secs(15),
        }
    }
}

/// The updater bound to a running daemon: periodic checks, install when idle, hand over.
pub struct Service {
    pub updater: Arc<Updater>,
    pool: Pool,
    log: Arc<Logger>,
    shutdown: Arc<Notify>,
    cfg: ServiceCfg,
    installing: AtomicBool,
    /// Set once the successor is running and this daemon is on its way out.
    pub restarting: AtomicBool,
}

impl Service {
    pub fn new(
        updater: Arc<Updater>,
        pool: Pool,
        log: Arc<Logger>,
        shutdown: Arc<Notify>,
        cfg: ServiceCfg,
    ) -> Arc<Self> {
        Arc::new(Self {
            updater,
            pool,
            log,
            shutdown,
            cfg,
            installing: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> UpdateState {
        self.updater.state(self.pool.config().auto_update)
    }

    /// Asks for a newer release.
    pub async fn check(self: &Arc<Self>) -> UpdateState {
        let me = self.clone();
        let r = tokio::task::spawn_blocking(move || me.updater.check()).await;
        match r {
            Ok(Ok(Some(rel))) => self.log.info(&format!("update available: {}", rel.version)),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => self.log.warn(&format!("update check failed: {e:#}")),
            Err(e) => self.log.warn(&format!("update check crashed: {e}")),
        }
        self.state()
    }

    /// Downloads and installs the known update, starts the new daemon and asks this one to stop.
    /// Returns the installed version.
    pub async fn install(self: &Arc<Self>) -> Result<String> {
        if self.installing.swap(true, Ordering::SeqCst) {
            bail!("an update is already in progress");
        }
        let me = self.clone();
        let r = tokio::task::spawn_blocking(move || me.updater.apply())
            .await
            .map_err(|e| anyhow!("update crashed: {e}"))
            .and_then(|r| r);
        let r = match r {
            Ok(v) => self.hand_over(&v).map(|_| v),
            Err(e) => Err(e),
        };
        self.installing.store(false, Ordering::SeqCst);
        if let Err(e) = &r {
            self.log.error(&format!("update failed: {e:#}"));
        }
        r
    }

    fn hand_over(&self, version: &str) -> Result<()> {
        let exe = self.updater.exe().to_path_buf();
        spawn_successor(&exe, &self.cfg.respawn_args, std::process::id())
            .with_context(|| format!("start the new daemon {}", exe.display()))?;
        self.log.info(&format!(
            "installed {version}, handing over to the new daemon"
        ));
        self.restarting.store(true, Ordering::SeqCst);
        self.shutdown.notify_one();
        Ok(())
    }

    /// Checks at start and every `check_every`; installs by itself when the setting allows and
    /// no tool call is running.
    pub async fn run(self: Arc<Self>) {
        // Leftovers of the update that started this daemon (the old exe is gone by now).
        clean_up(self.updater.exe(), &self.updater.cfg.state_dir);
        tokio::time::sleep(self.cfg.first_check_delay).await;
        loop {
            self.check().await;
            if self.updater.available_version().is_some() {
                self.install_when_idle().await;
            }
            tokio::time::sleep(self.cfg.check_every).await;
        }
    }

    /// Installs once `autoUpdate` is on and nothing is in flight; gives up when the setting
    /// goes off or the update vanishes.
    async fn install_when_idle(self: &Arc<Self>) {
        loop {
            if self.updater.available_version().is_none() || !self.pool.config().auto_update {
                self.updater.set_waiting(false);
                return;
            }
            if self.pool.in_flight_calls() == 0 {
                self.updater.set_waiting(false);
                if let Err(e) = self.install().await {
                    self.log.warn(&format!("automatic update failed: {e:#}"));
                }
                return;
            }
            self.updater.set_waiting(true);
            tokio::time::sleep(self.cfg.idle_poll).await;
        }
    }
}

// ---- CLI --------------------------------------------------------------------------------------

/// What `mcpm update` did without a running daemon.
#[derive(Debug, PartialEq)]
pub enum CliOutcome {
    UpToDate(String),
    Available(String),
    Installed(String),
}

/// `mcpm update` when no daemon runs: check (and install unless `check_only`) in this process.
pub fn run_cli(check_only: bool, state_dir: &Path) -> Result<CliOutcome> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let up = Updater::new(UpdaterCfg {
        exe: std::env::current_exe()?,
        state_dir: state_dir.to_path_buf(),
        current: crate::VERSION.to_string(),
        base_url: api_base(),
        fetcher: Arc::new(HttpFetcher {
            handle: rt.handle().clone(),
        }),
    });
    let Some(rel) = up.check()? else {
        return Ok(CliOutcome::UpToDate(crate::VERSION.to_string()));
    };
    if check_only {
        return Ok(CliOutcome::Available(rel.version));
    }
    Ok(CliOutcome::Installed(up.apply()?))
}

// ---- tests ------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const FIXTURE: &str = r#"{
      "tag_name": "v0.2.0",
      "name": "MCP Manager 0.2.0",
      "draft": false,
      "prerelease": false,
      "published_at": "2026-11-20T10:00:00Z",
      "body": "Fixes things.\n\n- one\n- two",
      "assets": [
        {"name": "mcpm-0.2.0-win64.zip", "browser_download_url": "https://x/zip", "size": 10},
        {"name": "mcpm.exe", "browser_download_url": "https://x/mcpm.exe", "size": 7340032,
         "digest": "sha256:AABB"},
        {"name": "mcpm.exe.sha256", "browser_download_url": "https://x/mcpm.exe.sha256", "size": 80}
      ]
    }"#;

    #[test]
    fn versions_compare() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("v0.1.0-beta", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert_eq!(compare("1.0", "1.0.0"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn parses_the_release_fixture() {
        let r = parse_release(FIXTURE).unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.exe_url, "https://x/mcpm.exe");
        assert_eq!(r.exe_size, 7_340_032);
        assert_eq!(r.published, "2026-11-20T10:00:00Z");
        assert!(r.notes.starts_with("Fixes things."));
        // the .sha256 asset wins over the inline digest
        assert_eq!(
            r.checksum,
            Some(Checksum::Asset("https://x/mcpm.exe.sha256".into()))
        );
    }

    #[test]
    fn release_without_sha_asset_falls_back_to_the_digest() {
        let j = FIXTURE.replace("mcpm.exe.sha256", "other.txt");
        let r = parse_release(&j).unwrap().unwrap();
        assert_eq!(r.checksum, Some(Checksum::Digest("aabb".into())));
        let j = j.replace(r#""digest": "sha256:AABB""#, r#""digest": null"#);
        assert_eq!(parse_release(&j).unwrap().unwrap().checksum, None);
    }

    #[test]
    fn drafts_prereleases_and_exe_less_releases_are_skipped() {
        let draft = FIXTURE.replace(r#""draft": false"#, r#""draft": true"#);
        assert!(parse_release(&draft).unwrap().is_none());
        let pre = FIXTURE.replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert!(parse_release(&pre).unwrap().is_none());
        let none = FIXTURE.replace(r#""name": "mcpm.exe""#, r#""name": "something.exe""#);
        assert!(parse_release(&none).unwrap().is_none());
        assert!(parse_release("not json").is_err());
        assert!(parse_release("{}").is_err());
    }

    #[test]
    fn sha256_text_formats() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            parse_sha256_text(&format!("{h}  mcpm.exe\n")).as_deref(),
            Some(h)
        );
        assert_eq!(
            parse_sha256_text(&h.to_uppercase()).as_deref(),
            Some(h),
            "case is ignored"
        );
        assert_eq!(parse_sha256_text("nothex mcpm.exe"), None);
        assert_eq!(parse_sha256_text(""), None);
        assert_eq!(parse_sha256_text(&h[..63]), None);
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    // sha256("MZabc")
    fn sha_of(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn verify_download_checks_size_header_and_hash() {
        let d = tempfile::tempdir().unwrap();
        let p = write(d.path(), "a.exe", b"MZabc");
        let good = sha_of(b"MZabc");
        assert!(verify_download(&p, 5, &good).is_ok());
        assert!(verify_download(&p, 0, &good).is_ok(), "size 0 = unlisted");
        assert!(
            verify_download(&p, 6, &good)
                .unwrap_err()
                .to_string()
                .contains("bytes")
        );
        assert!(
            verify_download(&p, 5, &sha_of(b"other"))
                .unwrap_err()
                .to_string()
                .contains("sha256")
        );
        let q = write(d.path(), "b.exe", b"#!/bin/sh");
        assert!(
            verify_download(&q, 9, &sha_of(b"#!/bin/sh"))
                .unwrap_err()
                .to_string()
                .contains("not a Windows program")
        );
    }

    #[test]
    fn swap_moves_the_old_exe_aside_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let exe = write(d.path(), "mcpm.exe", b"MZold");
        let staged = write(d.path(), "staged.bin", b"MZnew");
        swap_in(&exe, &staged).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZnew");
        assert_eq!(std::fs::read(beside(&exe, ".old")).unwrap(), b"MZold");
        assert!(!beside(&exe, ".new").exists());
        assert!(staged.exists(), "the staged file is the caller's");
        // a second update replaces the rollback copy
        let staged2 = write(d.path(), "staged2.bin", b"MZnewer");
        swap_in(&exe, &staged2).unwrap();
        assert_eq!(std::fs::read(beside(&exe, ".old")).unwrap(), b"MZnew");
        // next start removes the leftovers
        clean_up(&exe, d.path());
        assert!(!beside(&exe, ".old").exists());
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZnewer");
    }

    #[test]
    fn a_failed_swap_leaves_the_exe_alone() {
        let d = tempfile::tempdir().unwrap();
        let exe = write(d.path(), "mcpm.exe", b"MZold");
        // staged file does not exist
        assert!(swap_in(&exe, &d.path().join("missing")).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZold");
        assert!(!beside(&exe, ".new").exists() && !beside(&exe, ".old").exists());
    }

    // ---- the state machine, with a fake network ----

    #[derive(Default)]
    struct Fake {
        texts: Mutex<HashMap<String, Result<String, String>>>,
        files: Mutex<HashMap<String, Vec<u8>>>,
        gets: Mutex<Vec<String>>,
    }

    impl Fetcher for Fake {
        fn get_text(&self, url: &str) -> Result<String> {
            self.gets.lock().unwrap().push(url.to_string());
            match self.texts.lock().unwrap().get(url) {
                Some(Ok(t)) => Ok(t.clone()),
                Some(Err(e)) => Err(anyhow!("{e}")),
                None => Err(anyhow!("404 {url}")),
            }
        }
        fn download(&self, url: &str, dest: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
            let b = self
                .files
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("404 {url}"))?;
            std::fs::write(dest, &b)?;
            progress(b.len() as u64);
            Ok(())
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        exe: PathBuf,
        fake: Arc<Fake>,
        up: Arc<Updater>,
    }

    fn release_json(version: &str, exe_bytes: &[u8], sha: Option<&str>) -> String {
        let mut assets = vec![format!(
            r#"{{"name":"mcpm.exe","browser_download_url":"https://h/exe","size":{}}}"#,
            exe_bytes.len()
        )];
        if sha.is_some() {
            assets.push(
                r#"{"name":"mcpm.exe.sha256","browser_download_url":"https://h/sha","size":70}"#
                    .into(),
            );
        }
        format!(
            r#"{{"tag_name":"v{version}","draft":false,"prerelease":false,"body":"notes","assets":[{}]}}"#,
            assets.join(",")
        )
    }

    fn env(current: &str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let exe = write(dir.path(), "mcpm.exe", b"MZ running");
        let fake = Arc::new(Fake::default());
        let up = Updater::new(UpdaterCfg {
            exe: exe.clone(),
            state_dir: dir.path().join("state"),
            current: current.into(),
            base_url: "https://api/".into(),
            fetcher: fake.clone(),
        });
        Env {
            _dir: dir,
            exe,
            fake,
            up,
        }
    }

    fn publish(e: &Env, version: &str, bytes: &[u8], sha: Option<String>) {
        e.fake.texts.lock().unwrap().insert(
            "https://api/releases/latest".into(),
            Ok(release_json(version, bytes, sha.as_deref())),
        );
        if let Some(s) = sha {
            e.fake
                .texts
                .lock()
                .unwrap()
                .insert("https://h/sha".into(), Ok(format!("{s}  mcpm.exe\n")));
        }
        e.fake
            .files
            .lock()
            .unwrap()
            .insert("https://h/exe".into(), bytes.to_vec());
    }

    #[test]
    fn check_finds_a_newer_release() {
        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        let s = e.up.state(true);
        assert!(!s.available && s.last_check.is_none() && s.phase == Phase::Idle);
        let rel = e.up.check().unwrap().unwrap();
        assert_eq!(rel.version, "0.2.0");
        let s = e.up.state(true);
        assert!(s.available);
        assert_eq!(s.latest.as_deref(), Some("0.2.0"));
        assert_eq!(s.notes.as_deref(), Some("notes"));
        assert!(s.last_check.is_some() && s.last_error.is_none());
        assert_eq!(
            e.fake.gets.lock().unwrap()[0],
            "https://api/releases/latest"
        );
    }

    #[test]
    fn check_ignores_same_and_older_releases() {
        let e = env("0.2.0");
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        assert!(e.up.check().unwrap().is_none());
        publish(&e, "0.1.9", b"MZnew", Some(sha_of(b"MZnew")));
        assert!(e.up.check().unwrap().is_none());
        let s = e.up.state(false);
        assert!(!s.available && s.last_check.is_some() && !s.auto_update);
    }

    #[test]
    fn check_error_is_recorded_and_the_machine_returns_to_idle() {
        let e = env("0.1.0");
        let r = e.up.check();
        assert!(r.is_err());
        let s = e.up.state(true);
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.last_error.unwrap().contains("404"));
        assert!(s.last_check.is_some());
        // a later success clears the error
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        e.up.check().unwrap();
        assert!(e.up.state(true).last_error.is_none());
    }

    #[test]
    fn a_failed_recheck_keeps_the_known_release() {
        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        e.up.check().unwrap();
        e.fake
            .texts
            .lock()
            .unwrap()
            .insert("https://api/releases/latest".into(), Err("offline".into()));
        assert!(e.up.check().is_err());
        assert!(
            e.up.state(true).available,
            "an offline check must not forget the update"
        );
    }

    #[test]
    fn apply_downloads_verifies_and_swaps() {
        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew bytes", Some(sha_of(b"MZnew bytes")));
        e.up.check().unwrap();
        assert_eq!(e.up.apply().unwrap(), "0.2.0");
        assert_eq!(std::fs::read(&e.exe).unwrap(), b"MZnew bytes");
        assert_eq!(
            std::fs::read(beside(&e.exe, ".old")).unwrap(),
            b"MZ running"
        );
        let s = e.up.state(true);
        assert!(!s.available, "installed: nothing left to install");
        assert_eq!(s.phase, Phase::Idle);
        assert!(
            std::fs::read_dir(e.up.updates_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn checksum_mismatch_installs_nothing() {
        let e = env("0.1.0");
        publish(
            &e,
            "0.2.0",
            b"MZnew bytes",
            Some(sha_of(b"MZsomething else")),
        );
        e.up.check().unwrap();
        let err = e.up.apply().unwrap_err().to_string();
        assert!(err.contains("sha256"), "{err}");
        assert_eq!(std::fs::read(&e.exe).unwrap(), b"MZ running");
        assert!(!beside(&e.exe, ".old").exists());
        assert!(
            std::fs::read_dir(e.up.updates_dir())
                .unwrap()
                .next()
                .is_none(),
            "no partial file"
        );
        let s = e.up.state(true);
        assert!(s.available, "still available, retry possible");
        assert!(s.last_error.unwrap().contains("sha256"));
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn size_mismatch_and_missing_checksum_are_refused() {
        let e = env("0.1.0");
        // the release lists 5 bytes, the server sends 6
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew!")));
        e.fake
            .files
            .lock()
            .unwrap()
            .insert("https://h/exe".into(), b"MZnew!".to_vec());
        e.up.check().unwrap();
        assert!(e.up.apply().unwrap_err().to_string().contains("bytes"));
        assert_eq!(std::fs::read(&e.exe).unwrap(), b"MZ running");

        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew", None);
        e.up.check().unwrap();
        let err = e.up.apply().unwrap_err().to_string();
        assert!(err.contains("no checksum"), "{err}");
        assert_eq!(std::fs::read(&e.exe).unwrap(), b"MZ running");
    }

    #[test]
    fn download_without_a_known_release_is_an_error() {
        let e = env("0.1.0");
        assert!(
            e.up.download()
                .unwrap_err()
                .to_string()
                .contains("no update")
        );
    }

    #[test]
    fn a_busy_machine_does_not_start_a_second_job() {
        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        e.up.check().unwrap();
        assert!(e.up.begin(Phase::Downloading));
        assert!(!e.up.begin(Phase::Checking));
        let gets = e.fake.gets.lock().unwrap().len();
        // check while busy returns the known release without asking the network
        assert!(e.up.check().unwrap().is_some());
        assert_eq!(e.fake.gets.lock().unwrap().len(), gets);
        assert!(e.up.download().is_err());
        e.up.finish(None);
        assert!(e.up.begin(Phase::Checking));
    }

    #[test]
    fn a_verified_download_is_reused() {
        let e = env("0.1.0");
        publish(&e, "0.2.0", b"MZnew", Some(sha_of(b"MZnew")));
        e.up.check().unwrap();
        let p = e.up.download().unwrap();
        // the exe asset disappears: the verified copy still serves
        e.fake.files.lock().unwrap().clear();
        assert_eq!(e.up.download().unwrap(), p);
    }

    #[test]
    fn api_base_env_override() {
        // not parallel-safe with other env users in this crate: only this test touches it
        // SAFETY: tests of this module do not run another reader of this variable.
        unsafe {
            std::env::set_var("MCPM_UPDATE_URL", "http://127.0.0.1:9/repo/");
        }
        assert_eq!(api_base(), "http://127.0.0.1:9/repo");
        unsafe {
            std::env::remove_var("MCPM_UPDATE_URL");
        }
        assert_eq!(api_base(), REPO_API);
    }
}
