//! Child pool: lazy start, Shared vs Session instances, idle reaping, restart-on-crash, tool
//! cache with `list_changed` invalidation, call log ring buffer, live config changes.
//! Port of `relay-pool.cjs`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use mcpm_core::model::{InstanceMode, ManagerConfig, ServerConfig, Transport, valid_server_id};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::client::{Callbacks, McpClient};
use crate::logging::Logger;
use crate::mask;
use crate::storage;
use crate::timeutil;

pub const MAX_CALL_LOG: usize = 100;
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct PoolOptions {
    /// Session instances unused for this long are killed.
    pub idle_ttl: Duration,
    pub reap_interval: Duration,
    /// Write `config.json` on every change.
    pub persist: bool,
    /// Overall budget for spawn + initialize + first tools/list.
    pub connect_timeout: Duration,
    /// How long `tools/list` waits for pinned servers that are not running yet.
    pub pinned_wait: Duration,
    /// Wait after the first failed start; doubles per failure up to `retry_max`.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Failed starts in a row after which a server is not started again until the user
    /// restarts it or its config changes.
    pub max_start_failures: u32,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            idle_ttl: SESSION_IDLE_TTL,
            reap_interval: Duration::from_secs(60),
            persist: true,
            connect_timeout: Duration::from_secs(60),
            pinned_wait: Duration::from_secs(5),
            retry_base: Duration::from_secs(30),
            retry_max: Duration::from_secs(15 * 60),
            max_start_failures: 6,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallEntry {
    pub id: String,
    pub timestamp: u64,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub server_id: String,
    pub tool: String,
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PinnedTool {
    /// `<server>__<tool>`
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ToolListing {
    pub tools: Vec<Value>,
    pub errors: Vec<Value>,
}

#[derive(Default)]
pub struct ListToolsOpts {
    pub server: Option<String>,
    pub query: Option<String>,
    pub session: Option<String>,
    pub harness: Option<String>,
}

struct InstState {
    client: Option<Arc<McpClient>>,
    generation: u64,
    tools: Option<Arc<Vec<Value>>>,
    last_used: Instant,
    error: Option<String>,
    /// Failed starts in a row (reset by a successful start; a restart or config change
    /// replaces the instance).
    failures: u32,
    /// No new start attempt before this.
    retry_at: Option<Instant>,
}

struct Instance {
    connect_lock: Arc<tokio::sync::Mutex<()>>,
    state: Mutex<InstState>,
}

impl Instance {
    fn new() -> Self {
        Self {
            connect_lock: Arc::new(tokio::sync::Mutex::new(())),
            state: Mutex::new(InstState {
                client: None,
                generation: 0,
                tools: None,
                last_used: Instant::now(),
                error: None,
                failures: 0,
                retry_at: None,
            }),
        }
    }

    fn touch(&self) {
        self.state.lock().unwrap().last_used = Instant::now();
    }

    fn live_client(&self) -> Option<Arc<McpClient>> {
        self.state
            .lock()
            .unwrap()
            .client
            .clone()
            .filter(|c| c.is_alive())
    }

    fn close(&self) {
        let client = {
            let mut s = self.state.lock().unwrap();
            s.generation += 1; // late callbacks of the old client are ignored
            s.tools = None;
            s.client.take()
        };
        if let Some(c) = client {
            c.close();
        }
    }
}

struct Inner {
    home: PathBuf,
    cfg: RwLock<ManagerConfig>,
    instances: Mutex<HashMap<String, Arc<Instance>>>,
    calls: Mutex<VecDeque<CallEntry>>,
    /// Tool calls being served right now (the updater waits for zero).
    in_flight: AtomicUsize,
    events: broadcast::Sender<()>,
    opts: PoolOptions,
    log: Arc<Logger>,
}

/// Counts a running tool call for as long as it lives.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(n: &'a AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::Relaxed);
        Self(n)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

fn instance_key(cfg: &ServerConfig, session: Option<&str>) -> String {
    match cfg.mode {
        InstanceMode::Shared => cfg.id.clone(),
        InstanceMode::Session => format!(
            "{}::{}",
            cfg.id,
            session.filter(|s| !s.is_empty()).unwrap_or("default")
        ),
    }
}

fn belongs_to(key: &str, server_id: &str) -> bool {
    key == server_id
        || key
            .strip_prefix(server_id)
            .is_some_and(|r| r.starts_with("::"))
}

/// Validates and normalizes a server config (port of `normalizeServerConfig`).
pub fn normalize_server(mut cfg: ServerConfig) -> Result<ServerConfig> {
    cfg.id = cfg.id.trim().to_string();
    if !valid_server_id(&cfg.id) {
        bail!(
            "Invalid server id \"{}\" (use letters, digits, - or _, max 48)",
            cfg.id
        );
    }
    if cfg.name.trim().is_empty() {
        cfg.name = cfg.id.clone();
    }
    match &mut cfg.transport {
        Transport::Stdio { command, .. } => {
            *command = command.trim().to_string();
            if command.is_empty() {
                bail!("Server \"{}\": stdio transport requires a command", cfg.id);
            }
        }
        Transport::Http { url, .. } => {
            *url = url.trim().to_string();
            let l = url.to_ascii_lowercase();
            if !(l.starts_with("http://") || l.starts_with("https://")) {
                bail!(
                    "Server \"{}\": http transport requires an http(s) url",
                    cfg.id
                );
            }
        }
    }
    cfg.pinned_tools = cfg
        .pinned_tools
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let mut hs: Vec<String> = Vec::new();
    for h in &cfg.harnesses {
        let h = h.trim().to_ascii_lowercase();
        if !h.is_empty()
            && h.len() <= 32
            && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !hs.contains(&h)
        {
            hs.push(h);
        }
    }
    cfg.harnesses = hs;
    Ok(cfg)
}

impl Pool {
    pub fn new(home: PathBuf, cfg: ManagerConfig, opts: PoolOptions, log: Arc<Logger>) -> Pool {
        let (events, _) = broadcast::channel(16);
        Pool {
            inner: Arc::new(Inner {
                home,
                cfg: RwLock::new(cfg),
                instances: Mutex::new(HashMap::new()),
                calls: Mutex::new(VecDeque::new()),
                in_flight: AtomicUsize::new(0),
                events,
                opts,
                log,
            }),
        }
    }

    /// Fires on every config change and when a child's tool list changes; sessions turn it into
    /// `notifications/tools/list_changed`.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.inner.events.subscribe()
    }

    pub fn notify_changed(&self) {
        let _ = self.inner.events.send(());
    }

    pub fn config(&self) -> ManagerConfig {
        self.inner.cfg.read().unwrap().clone()
    }

    /// Tool calls being served right now.
    pub fn in_flight_calls(&self) -> usize {
        self.inner.in_flight.load(Ordering::Relaxed)
    }

    /// Switches automatic updates on or off and saves it.
    pub fn set_auto_update(&self, on: bool) {
        self.inner.cfg.write().unwrap().auto_update = on;
        self.save();
    }

    pub fn server(&self, id: &str) -> Option<ServerConfig> {
        self.inner
            .cfg
            .read()
            .unwrap()
            .servers
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    fn save(&self) {
        if !self.inner.opts.persist {
            return;
        }
        let cfg = self.config();
        if let Err(e) = storage::save_config(&self.inner.home, &cfg) {
            self.inner
                .log
                .warn(&format!("failed to save config: {e:#}"));
        }
    }

    // ----- config mutation ------------------------------------------------------------------

    pub fn add_server(&self, cfg: ServerConfig) -> Result<ServerConfig> {
        let cfg = normalize_server(cfg)?;
        {
            let mut g = self.inner.cfg.write().unwrap();
            if g.servers.iter().any(|s| s.id == cfg.id) {
                bail!("Server \"{}\" already exists", cfg.id);
            }
            g.servers.push(cfg.clone());
        }
        self.save();
        self.notify_changed();
        Ok(cfg)
    }

    /// Replaces an existing server (restarts its children).
    pub fn update_server(&self, cfg: ServerConfig) -> Result<ServerConfig> {
        let cfg = normalize_server(cfg)?;
        {
            let mut g = self.inner.cfg.write().unwrap();
            let slot = g
                .servers
                .iter_mut()
                .find(|s| s.id == cfg.id)
                .ok_or_else(|| anyhow!("Unknown MCP server \"{}\"", cfg.id))?;
            *slot = cfg.clone();
        }
        self.close_instances_for(&cfg.id);
        self.save();
        self.notify_changed();
        Ok(cfg)
    }

    pub fn remove_server(&self, id: &str) -> bool {
        let existed = {
            let mut g = self.inner.cfg.write().unwrap();
            let before = g.servers.len();
            g.servers.retain(|s| s.id != id);
            g.servers.len() != before
        };
        self.close_instances_for(id);
        if existed {
            self.save();
            self.notify_changed();
        }
        existed
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<ServerConfig> {
        let cfg = {
            let mut g = self.inner.cfg.write().unwrap();
            let s = g
                .servers
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or_else(|| anyhow!("Unknown MCP server \"{id}\""))?;
            s.enabled = enabled;
            s.clone()
        };
        if !enabled {
            self.close_instances_for(id);
        }
        self.save();
        self.notify_changed();
        Ok(cfg)
    }

    /// Swaps in a config that was changed on disk (takeover/restore): children of servers that
    /// changed or vanished are restarted.
    pub fn replace_config(&self, new: ManagerConfig) {
        let old = self.config();
        let changed: Vec<String> = old
            .servers
            .iter()
            .filter(|o| new.servers.iter().find(|n| n.id == o.id) != Some(*o))
            .map(|o| o.id.clone())
            .collect();
        *self.inner.cfg.write().unwrap() = new;
        for id in changed {
            self.close_instances_for(&id);
        }
        self.notify_changed();
    }

    pub fn reload_from_disk(&self) -> Result<()> {
        let cfg = storage::load_config(&self.inner.home)?;
        self.replace_config(cfg);
        Ok(())
    }

    // ----- instances --------------------------------------------------------------------------

    fn close_instances_for(&self, server_id: &str) {
        let removed: Vec<Arc<Instance>> = {
            let mut m = self.inner.instances.lock().unwrap();
            let keys: Vec<String> = m
                .keys()
                .filter(|k| belongs_to(k, server_id))
                .cloned()
                .collect();
            keys.iter().filter_map(|k| m.remove(k)).collect()
        };
        for i in removed {
            i.close();
        }
    }

    /// Kills the Session-mode instances of one MCP session (client sent DELETE or went away).
    pub fn close_session(&self, session: &str) {
        let suffix = format!("::{session}");
        let removed: Vec<Arc<Instance>> = {
            let mut m = self.inner.instances.lock().unwrap();
            let keys: Vec<String> = m.keys().filter(|k| k.ends_with(&suffix)).cloned().collect();
            keys.iter().filter_map(|k| m.remove(k)).collect()
        };
        for i in removed {
            i.close();
        }
    }

    /// Kills idle Session instances. Returns how many were closed.
    pub fn reap_idle(&self) -> usize {
        let ttl = self.inner.opts.idle_ttl;
        let removed: Vec<Arc<Instance>> = {
            let mut m = self.inner.instances.lock().unwrap();
            let keys: Vec<String> = m
                .iter()
                .filter(|(k, i)| {
                    k.contains("::") && i.state.lock().unwrap().last_used.elapsed() > ttl
                })
                .map(|(k, _)| k.clone())
                .collect();
            keys.iter().filter_map(|k| m.remove(k)).collect()
        };
        for i in &removed {
            i.close();
        }
        removed.len()
    }

    pub fn spawn_reaper(&self) -> tokio::task::JoinHandle<()> {
        let pool = self.clone();
        let every = self.inner.opts.reap_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let n = pool.reap_idle();
                if n > 0 {
                    pool.inner
                        .log
                        .info(&format!("reaped {n} idle session instance(s)"));
                }
            }
        })
    }

    pub fn shutdown(&self) {
        let all: Vec<Arc<Instance>> = self
            .inner
            .instances
            .lock()
            .unwrap()
            .drain()
            .map(|(_, i)| i)
            .collect();
        for i in all {
            i.close();
        }
    }

    fn secrets_of(&self, server_id: &str) -> Vec<String> {
        self.server(server_id)
            .map(|c| mask::secrets(&c))
            .unwrap_or_default()
    }

    fn mask_err(&self, server_id: &str, text: &str) -> String {
        mask::mask_text(text, &self.secrets_of(server_id))
    }

    /// Returns the instance + a live client, starting the child if needed (one connect at a time
    /// per instance; the connect itself runs detached so a cancelled caller does not abort it).
    async fn ensure(
        &self,
        server_id: &str,
        session: Option<&str>,
        harness: Option<&str>,
    ) -> Result<(Arc<Instance>, Arc<McpClient>)> {
        let cfg = self
            .server(server_id)
            .ok_or_else(|| anyhow!("Unknown MCP server \"{server_id}\""))?;
        if !cfg.enabled {
            bail!("MCP server \"{server_id}\" is disabled");
        }
        if let Some(h) = harness
            && !cfg.is_exposed_to(Some(h))
        {
            bail!("MCP server \"{server_id}\" is not exposed to {h}");
        }
        let key = instance_key(&cfg, session);
        let inst = self
            .inner
            .instances
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| Arc::new(Instance::new()))
            .clone();
        inst.touch();
        if let Some(c) = inst.live_client() {
            return Ok((inst, c));
        }
        let guard = inst.connect_lock.clone().lock_owned().await;
        if let Some(c) = inst.live_client() {
            return Ok((inst, c));
        }
        if let Some(msg) = self.start_blocked(&inst, &cfg.id) {
            bail!(msg);
        }
        let pool = self.clone();
        let inst2 = inst.clone();
        let task = tokio::spawn(async move {
            let _guard = guard;
            pool.connect_instance(&inst2, cfg).await
        });
        let client = task
            .await
            .map_err(|e| anyhow!("connect task failed: {e}"))??;
        Ok((inst, client))
    }

    /// Why a new start attempt must not happen yet (backoff after failed starts), if so.
    fn start_blocked(&self, inst: &Instance, id: &str) -> Option<String> {
        let s = inst.state.lock().unwrap();
        if s.failures == 0 {
            return None;
        }
        let err = s.error.as_deref().unwrap_or("unknown error");
        if s.failures >= self.inner.opts.max_start_failures {
            return Some(format!(
                "MCP server \"{id}\" failed to start {} times in a row and is stopped. Last error: {err}.                  Restart it in MCP Manager (or call mcpm_reload_server) to try again.",
                s.failures
            ));
        }
        let wait = s.retry_at?.checked_duration_since(Instant::now())?;
        Some(format!(
            "MCP server \"{id}\" failed to start ({err}). Next try in {}s.",
            wait.as_secs().max(1)
        ))
    }

    /// Backoff after `failures` failed starts in a row: base, 2x base, 4x base ... up to max.
    fn retry_delay(&self, failures: u32) -> Duration {
        let o = &self.inner.opts;
        let factor = 1u32 << failures.saturating_sub(1).min(16);
        o.retry_base.saturating_mul(factor).min(o.retry_max)
    }

    async fn connect_instance(
        &self,
        inst: &Arc<Instance>,
        cfg: ServerConfig,
    ) -> Result<Arc<McpClient>> {
        let generation = {
            let mut s = inst.state.lock().unwrap();
            s.generation += 1;
            s.error = None;
            s.client = None;
            s.generation
        };
        let weak = Arc::downgrade(inst);
        let events = self.inner.events.clone();
        let log = self.inner.log.clone();
        let sid = cfg.id.clone();
        let w1 = weak.clone();
        let w2 = weak;
        let cb = Callbacks {
            on_exit: Arc::new(move |msg| {
                if let Some(i) = w1.upgrade() {
                    let mut s = i.state.lock().unwrap();
                    if s.generation == generation {
                        s.client = None;
                        s.error = Some(msg.clone());
                    }
                }
                log.warn(&format!("server {sid}: child exited"));
            }),
            on_tools_changed: Arc::new(move || {
                if let Some(i) = w2.upgrade() {
                    let mut s = i.state.lock().unwrap();
                    if s.generation == generation {
                        s.tools = None;
                        drop(s);
                        let _ = events.send(());
                    }
                }
            }),
        };
        let secrets = mask::secrets(&cfg);
        let started = Instant::now();
        let per_request = crate::client::start_timeout(&cfg);
        // A custom start timeout also stretches the overall budget (initialize + tools/list).
        let budget = match cfg.start_timeout_secs {
            Some(_) => per_request * 2 + Duration::from_secs(10),
            None => self.inner.opts.connect_timeout,
        };
        let attempt = tokio::time::timeout(budget, async {
            let client = McpClient::connect(&cfg, cb).await?;
            match client.list_tools_within(per_request).await {
                Ok(tools) => Ok((client, tools)),
                Err(e) => {
                    client.close();
                    Err(e)
                }
            }
        })
        .await;
        match attempt {
            Ok(Ok((client, tools))) => {
                let client = Arc::new(client);
                let mut s = inst.state.lock().unwrap();
                if s.generation != generation {
                    // closed/replaced while connecting
                    drop(s);
                    client.close();
                    bail!("MCP server \"{}\" was changed while starting", cfg.id);
                }
                s.client = Some(client.clone());
                s.tools = Some(Arc::new(tools));
                s.failures = 0;
                s.retry_at = None;
                self.inner.log.info(&format!(
                    "server {} started in {}ms",
                    cfg.id,
                    started.elapsed().as_millis()
                ));
                Ok(client)
            }
            other => {
                let raw = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(_) => format!("timed out starting MCP server after {}s", budget.as_secs()),
                    Ok(Ok(_)) => unreachable!(),
                };
                let msg = mask::mask_text(&raw, &secrets);
                let mut s = inst.state.lock().unwrap();
                let mut next = String::new();
                if s.generation == generation {
                    s.error = Some(msg.clone());
                    s.failures += 1;
                    if s.failures >= self.inner.opts.max_start_failures {
                        s.retry_at = None;
                        next = format!(
                            "; stopped after {} failed starts (restart it to try again)",
                            s.failures
                        );
                    } else {
                        let wait = self.retry_delay(s.failures);
                        s.retry_at = Some(Instant::now() + wait);
                        next = format!("; next try in {}s at the earliest", wait.as_secs());
                    }
                }
                drop(s);
                self.inner
                    .log
                    .warn(&format!("server {} failed to start: {msg}{next}", cfg.id));
                self.notify_changed();
                Err(anyhow!(msg))
            }
        }
    }

    /// Cached tool list of an instance; refetches after a `list_changed`.
    async fn tools_of(
        &self,
        inst: &Arc<Instance>,
        client: &Arc<McpClient>,
    ) -> Result<Arc<Vec<Value>>> {
        let (cached, generation) = {
            let s = inst.state.lock().unwrap();
            (s.tools.clone(), s.generation)
        };
        if let Some(t) = cached {
            return Ok(t);
        }
        let tools = Arc::new(client.list_tools().await?);
        let mut s = inst.state.lock().unwrap();
        if s.generation == generation {
            s.tools = Some(tools.clone());
        }
        Ok(tools)
    }

    // ----- views --------------------------------------------------------------------------------

    /// Servers (masked) with live status. `harness` filters by exposure like the relay.
    pub fn list_servers(&self, harness: Option<&str>) -> Vec<Value> {
        let cfg = self.config();
        let inst = self.inner.instances.lock().unwrap();
        cfg.servers
            .iter()
            .filter(|s| harness.is_none() || s.is_exposed_to(harness))
            .map(|s| {
                let mut active = 0;
                let mut status = if s.enabled { "idle" } else { "disabled" };
                let mut tool_count = 0;
                let mut last_error: Option<String> = None;
                for (k, i) in inst.iter().filter(|(k, _)| belongs_to(k, &s.id)) {
                    let _ = k;
                    let st = i.state.lock().unwrap();
                    if st.client.as_ref().is_some_and(|c| c.is_alive()) {
                        active += 1;
                        status = "running";
                    } else if let Some(e) = &st.error {
                        last_error = Some(e.clone());
                        if st.failures >= self.inner.opts.max_start_failures {
                            if status != "running" {
                                status = "failed";
                            }
                        } else if status == "idle" {
                            status = "error";
                        }
                    }
                    if let Some(t) = &st.tools {
                        tool_count = tool_count.max(t.len());
                    }
                }
                let mut v = serde_json::to_value(mask::mask_config(s)).unwrap_or(Value::Null);
                v["status"] = json!(status);
                v["toolCount"] = json!(tool_count);
                v["activeSessions"] = json!(active);
                v["lastError"] = json!(last_error.map(|e| mask::mask_text(&e, &mask::secrets(s))));
                v
            })
            .collect()
    }

    /// Number of running child instances.
    pub fn running_instances(&self) -> usize {
        self.inner
            .instances
            .lock()
            .unwrap()
            .values()
            .filter(|i| i.state.lock().unwrap().client.is_some())
            .count()
    }

    pub async fn list_tools(&self, opts: ListToolsOpts) -> ToolListing {
        let cfg = self.config();
        let harness = opts.harness.as_deref();
        let targets: Vec<String> = match &opts.server {
            Some(s) => vec![s.clone()],
            None => cfg
                .servers
                .iter()
                .filter(|s| s.enabled && s.is_exposed_to(harness))
                .map(|s| s.id.clone())
                .collect(),
        };
        let query = opts
            .query
            .as_deref()
            .map(str::to_lowercase)
            .filter(|q| !q.is_empty());
        let mut handles = Vec::new();
        for id in targets {
            let pool = self.clone();
            let session = opts.session.clone();
            let harness = opts.harness.clone();
            handles.push(tokio::spawn(async move {
                let r = async {
                    let (inst, client) = pool
                        .ensure(&id, session.as_deref(), harness.as_deref())
                        .await?;
                    pool.tools_of(&inst, &client).await
                }
                .await;
                (id, r)
            }));
        }
        let mut out = ToolListing::default();
        for h in handles {
            let Ok((id, r)) = h.await else { continue };
            match r {
                Ok(tools) => {
                    for t in tools.iter() {
                        let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                        let desc = t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Some(q) = &query
                            && !format!("{id} {name} {desc}").to_lowercase().contains(q)
                        {
                            continue;
                        }
                        out.tools.push(json!({
                            "server": id,
                            "name": name,
                            "description": desc,
                            "inputSchema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                        }));
                    }
                }
                Err(e) => out
                    .errors
                    .push(json!({ "server": id, "error": self.mask_err(&id, &format!("{e:#}")) })),
            }
        }
        out
    }

    /// Pinned tools for `harness`. Servers with pinned tools that are not running yet are
    /// started (waiting up to `pinned_wait`); late starters send a list_changed when ready.
    pub async fn pinned_tools(
        &self,
        harness: Option<&str>,
        session: Option<&str>,
    ) -> Vec<PinnedTool> {
        let cfg = self.config();
        let wanted: Vec<&ServerConfig> = cfg
            .servers
            .iter()
            .filter(|s| s.enabled && !s.pinned_tools.is_empty() && s.is_exposed_to(harness))
            .collect();
        let mut handles = Vec::new();
        for s in &wanted {
            if self.cached_tools(s, session).is_some() {
                continue;
            }
            let pool = self.clone();
            let (id, session, harness) = (
                s.id.clone(),
                session.map(str::to_string),
                harness.map(str::to_string),
            );
            handles.push(tokio::spawn(async move {
                if pool
                    .ensure(&id, session.as_deref(), harness.as_deref())
                    .await
                    .is_ok()
                {
                    pool.notify_changed();
                }
            }));
        }
        let deadline = tokio::time::Instant::now() + self.inner.opts.pinned_wait;
        for h in handles {
            let _ = tokio::time::timeout_at(deadline, h).await;
        }
        let mut out = Vec::new();
        for s in wanted {
            let known = self.cached_tools(s, session);
            let names: Vec<String> = if s.pins_all() {
                known
                    .iter()
                    .flat_map(|k| k.iter())
                    .filter_map(|t| t.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            } else {
                s.pinned_tools.clone()
            };
            for tool in &names {
                let m = known.as_ref().and_then(|k| {
                    k.iter()
                        .find(|t| t.get("name").and_then(Value::as_str) == Some(tool))
                });
                let desc = m
                    .and_then(|t| t.get("description"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Pinned tool {tool} on {}", s.id));
                out.push(PinnedTool {
                    name: format!("{}__{}", s.id, tool),
                    description: format!("[{}] {desc}", s.id),
                    input_schema: m
                        .and_then(|t| t.get("inputSchema"))
                        .cloned()
                        .unwrap_or_else(
                            || json!({ "type": "object", "additionalProperties": true }),
                        ),
                    server: s.id.clone(),
                    tool: tool.clone(),
                });
            }
        }
        out
    }

    fn cached_tools(&self, cfg: &ServerConfig, session: Option<&str>) -> Option<Arc<Vec<Value>>> {
        let key = instance_key(cfg, session);
        let inst = self.inner.instances.lock().unwrap().get(&key).cloned()?;
        let s = inst.state.lock().unwrap();
        if s.client.as_ref().is_some_and(|c| c.is_alive()) {
            s.tools.clone()
        } else {
            None
        }
    }

    // ----- calls --------------------------------------------------------------------------------

    pub async fn call_tool(
        &self,
        server_id: &str,
        tool: &str,
        args: Value,
        session: Option<&str>,
        harness: Option<&str>,
    ) -> Result<Value> {
        let started = Instant::now();
        let ts = timeutil::now_ms();
        let _busy = InFlight::enter(&self.inner.in_flight);
        let res = async {
            let (inst, client) = self.ensure(server_id, session, harness).await?;
            let r = client
                .call_tool(tool, args)
                .await
                .map_err(|e| anyhow!(self.mask_err(server_id, &format!("{e:#}"))));
            inst.touch();
            r
        }
        .await;
        self.record(CallEntry {
            id: timeutil::uuid(),
            timestamp: ts,
            harness: harness.map(str::to_string),
            session_id: session.map(str::to_string),
            server_id: server_id.to_string(),
            tool: tool.to_string(),
            ok: res.is_ok(),
            duration_ms: started.elapsed().as_millis() as u64,
            error: res.as_ref().err().map(|e| e.to_string()),
        });
        res
    }

    fn record(&self, entry: CallEntry) {
        let mut log = self.inner.calls.lock().unwrap();
        log.push_front(entry);
        log.truncate(MAX_CALL_LOG);
    }

    pub fn recent_calls(&self, limit: usize) -> Vec<CallEntry> {
        self.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Restarts every server that has a running instance (Shared ones start again at once,
    /// Session ones on their next call). Returns how many were restarted.
    pub async fn restart_all(&self) -> usize {
        let ids: Vec<String> = {
            let m = self.inner.instances.lock().unwrap();
            self.config()
                .servers
                .iter()
                .filter(|s| {
                    m.iter().any(|(k, i)| {
                        belongs_to(k, &s.id) && i.state.lock().unwrap().client.is_some()
                    })
                })
                .map(|s| s.id.clone())
                .collect()
        };
        let mut n = 0;
        for id in ids {
            match self.restart_server(&id, None, None).await {
                Ok(_) => n += 1,
                Err(e) => self
                    .inner
                    .log
                    .warn(&format!("restart of {id} failed: {e:#}")),
            }
        }
        n
    }

    /// Kills every instance of the server and starts one again for the caller's session.
    pub async fn restart_server(
        &self,
        server_id: &str,
        session: Option<&str>,
        harness: Option<&str>,
    ) -> Result<Value> {
        let cfg = self
            .server(server_id)
            .ok_or_else(|| anyhow!("Unknown MCP server \"{server_id}\""))?;
        self.close_instances_for(server_id);
        if !cfg.enabled {
            return Ok(json!({ "id": server_id, "status": "disabled", "toolCount": 0 }));
        }
        let (inst, _client) = self.ensure(server_id, session, harness).await?;
        let count = inst
            .state
            .lock()
            .unwrap()
            .tools
            .as_ref()
            .map_or(0, |t| t.len());
        self.notify_changed();
        Ok(json!({ "id": server_id, "status": "running", "toolCount": count }))
    }

    /// Tools of one server for the control API (starts it if needed).
    pub async fn server_tools(&self, server_id: &str, session: Option<&str>) -> Result<Vec<Value>> {
        let (inst, client) = self.ensure(server_id, session, None).await?;
        Ok(self.tools_of(&inst, &client).await?.as_ref().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(id: &str) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: String::new(),
            transport: Transport::Http {
                url: "http://127.0.0.1:1/mcp".into(),
                headers: Default::default(),
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec![" a ".into(), "".into()],
            harnesses: vec!["Claude".into(), "claude".into(), "bad id".into()],
            start_timeout_secs: None,
            origin: None,
        }
    }

    #[test]
    fn normalizes() {
        let c = normalize_server(http("x")).unwrap();
        assert_eq!(c.name, "x");
        assert_eq!(c.pinned_tools, vec!["a"]);
        assert_eq!(c.harnesses, vec!["claude"]);
        assert!(normalize_server(http("-bad")).is_err());
    }

    #[test]
    fn key_ownership() {
        assert!(belongs_to("a", "a"));
        assert!(belongs_to("a::s1", "a"));
        assert!(!belongs_to("ab::s1", "a"));
    }
}
