//! Where CLI operations run: through the daemon's control API when it is up (single writer),
//! otherwise directly on `mcpm-core` / `RealCoreOps`.

use anyhow::{Result, anyhow, bail};
use mcpm_core::{Core, model::ServerConfig};
use mcpm_daemon::coreops::{CoreOps, OpError};
use mcpm_daemon::{RealCoreOps, mask};
use mcpm_gui::client::{self, Conn};
use serde_json::{Value, json};

pub enum Backend {
    Daemon(Conn),
    Direct(Box<RealCoreOps>),
}

fn op<T>(r: Result<T, OpError>) -> Result<T> {
    r.map_err(|e| anyhow!("{e}"))
}

impl Backend {
    /// The running daemon if there is one, else direct access.
    pub fn open() -> Result<Self> {
        let core = Core::from_env();
        Ok(match client::locate(&core.state_dir) {
            Some(c) => Self::Daemon(c),
            None => Self::Direct(Box::new(RealCoreOps::new(core, std::env::current_exe()?))),
        })
    }

    pub fn conn(&self) -> Option<&Conn> {
        match self {
            Self::Daemon(c) => Some(c),
            Self::Direct(_) => None,
        }
    }

    pub fn core(&self) -> Core {
        Core::from_env()
    }

    pub fn harnesses(&self) -> Result<Value> {
        match self {
            Self::Daemon(c) => Ok(c.get("/api/harnesses")?),
            Self::Direct(o) => op(o.harnesses()),
        }
    }

    pub fn plan(&self, harness: &str) -> Result<Value> {
        match self {
            Self::Daemon(c) => Ok(c.post("/api/takeover/plan", &json!({ "harness": harness }))?),
            Self::Direct(o) => op(o.plan_takeover(harness)),
        }
    }

    /// `target` is a harness id or `"all"`.
    pub fn apply(&self, target: &str) -> Result<Value> {
        match self {
            Self::Daemon(c) => {
                let body = if target == "all" {
                    json!({ "all": true })
                } else {
                    json!({ "harness": target })
                };
                Ok(c.slow().post("/api/takeover/apply", &body)?)
            }
            Self::Direct(o) => op(o.apply_takeover(target)),
        }
    }

    pub fn snapshots(&self) -> Result<Value> {
        match self {
            Self::Daemon(c) => Ok(c.get("/api/snapshots")?),
            Self::Direct(o) => op(o.snapshots()),
        }
    }

    pub fn restore(&self, req: &Value) -> Result<Value> {
        match self {
            Self::Daemon(c) => Ok(c.slow().post("/api/restore", req)?),
            Self::Direct(o) => op(o.restore(req)),
        }
    }

    pub fn status(&self) -> Result<Option<Value>> {
        match self {
            Self::Daemon(c) => Ok(Some(c.get("/api/status")?)),
            Self::Direct(_) => Ok(None),
        }
    }

    pub fn servers(&self) -> Result<Value> {
        match self {
            Self::Daemon(c) => Ok(c.get("/api/servers")?),
            Self::Direct(_) => {
                let cfg = mcpm_core::store::load(&self.core().state_dir)?;
                let list = cfg
                    .servers
                    .iter()
                    .map(|s| {
                        let mut v =
                            serde_json::to_value(mask::mask_config(s)).unwrap_or(Value::Null);
                        v["status"] = json!(if s.enabled { "stopped" } else { "disabled" });
                        v
                    })
                    .collect();
                Ok(Value::Array(list))
            }
        }
    }

    pub fn add_server(&self, cfg: &ServerConfig) -> Result<()> {
        match self {
            Self::Daemon(c) => {
                c.post("/api/servers", &serde_json::to_value(cfg)?)?;
                Ok(())
            }
            Self::Direct(_) => self.edit_config(|c| {
                if c.servers.iter().any(|s| s.id == cfg.id) {
                    bail!("server `{}` already exists", cfg.id);
                }
                c.servers.push(cfg.clone());
                Ok(())
            }),
        }
    }

    pub fn remove_server(&self, id: &str) -> Result<()> {
        match self {
            Self::Daemon(c) => {
                c.delete(&format!("/api/servers/{id}"))?;
                Ok(())
            }
            Self::Direct(_) => self.edit_config(|c| {
                let before = c.servers.len();
                c.servers.retain(|s| s.id != id);
                if c.servers.len() == before {
                    bail!("unknown server `{id}`");
                }
                Ok(())
            }),
        }
    }

    pub fn set_enabled(&self, id: &str, on: bool) -> Result<()> {
        match self {
            Self::Daemon(c) => {
                let verb = if on { "enable" } else { "disable" };
                c.post(&format!("/api/servers/{id}/{verb}"), &json!({}))?;
                Ok(())
            }
            Self::Direct(_) => self.edit_config(|c| {
                let s = c
                    .servers
                    .iter_mut()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow!("unknown server `{id}`"))?;
                s.enabled = on;
                Ok(())
            }),
        }
    }

    /// Stops the daemon if one runs. Returns whether there was one.
    pub fn shutdown(&self) -> Result<bool> {
        let Some(c) = self.conn() else {
            return Ok(false);
        };
        c.post("/api/shutdown", &json!({}))?;
        let home = self.core().state_dir;
        for _ in 0..50 {
            if client::locate(&home).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(true)
    }

    fn edit_config(
        &self,
        f: impl FnOnce(&mut mcpm_core::model::ManagerConfig) -> Result<()>,
    ) -> Result<()> {
        let dir = self.core().state_dir;
        let mut cfg = mcpm_core::store::load(&dir)?;
        f(&mut cfg)?;
        mcpm_core::store::validate(&cfg)?;
        mcpm_core::store::save(&dir, &cfg)
    }
}
