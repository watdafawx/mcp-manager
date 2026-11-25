//! `CoreOps` backed by `mcpm-core`: harness status, takeover, snapshots and restore.

use std::path::PathBuf;

use mcpm_core::{Core, edit, harness, model::MANAGER_ENTRY, restore, snapshot, takeover};
use serde_json::{Value, json};

use crate::coreops::{CoreOps, OpError, OpResult};

/// Real core operations for one user profile. `exe` is written into stdio bridge entries.
pub struct RealCoreOps {
    pub core: Core,
    pub exe: PathBuf,
}

impl RealCoreOps {
    pub fn new(core: Core, exe: PathBuf) -> Self {
        Self { core, exe }
    }

    fn find(&self, id: &str) -> Result<harness::HarnessDef, OpError> {
        harness::registry(&self.core.home, &self.core.appdata)
            .into_iter()
            .find(|h| h.id == id)
            .ok_or_else(|| OpError::Invalid(format!("unknown harness `{id}`")))
    }

    fn harness_status(&self, h: &harness::HarnessDef, snaps: &[snapshot::Snapshot]) -> Value {
        let mut files = vec![];
        let mut has_manager = false;
        for f in &h.files {
            let Ok(text) = std::fs::read_to_string(&f.path) else { continue };
            let names: Vec<String> = edit::entries(&text, f)
                .map(|e| e.into_keys().collect())
                .unwrap_or_default();
            has_manager |= names.iter().any(|n| n == MANAGER_ENTRY);
            files.push(json!({ "path": f.path, "entries": names }));
        }
        let taken = snaps
            .iter()
            .any(|s| s.operation == "takeover" && s.files.iter().any(|f| f.harness == h.id));
        let state = match (has_manager, taken) {
            (true, true) => "takenOver",
            (true, false) => "connected",
            (false, _) => "notConnected",
        };
        json!({
            "id": h.id,
            "label": h.label,
            "detected": harness::detected(h),
            "state": state,
            "files": files,
        })
    }
}

fn fail(e: anyhow::Error) -> OpError {
    OpError::Failed(format!("{e:#}"))
}

fn to_value<T: serde::Serialize>(v: T) -> OpResult {
    serde_json::to_value(v).map_err(|e| OpError::Failed(e.to_string()))
}

impl CoreOps for RealCoreOps {
    fn harnesses(&self) -> OpResult {
        let snaps = snapshot::list(&self.core.state_dir).map_err(fail)?;
        let list: Vec<Value> = harness::registry(&self.core.home, &self.core.appdata)
            .iter()
            .map(|h| self.harness_status(h, &snaps))
            .collect();
        Ok(Value::Array(list))
    }

    fn plan_takeover(&self, id: &str) -> OpResult {
        let h = self.find(id)?;
        to_value(takeover::plan_takeover(&self.core, &h, &self.exe).map_err(fail)?)
    }

    fn apply_takeover(&self, target: &str) -> OpResult {
        if target == "all" {
            return to_value(takeover::takeover_all(&self.core, &self.exe).map_err(fail)?);
        }
        let h = self.find(target)?;
        to_value(takeover::takeover(&self.core, &h, &self.exe).map_err(fail)?)
    }

    fn snapshots(&self) -> OpResult {
        to_value(snapshot::list(&self.core.state_dir).map_err(fail)?)
    }

    fn delete_snapshot(&self, id: &str) -> OpResult {
        snapshot::delete(&self.core.state_dir, id).map_err(fail)?;
        Ok(json!({ "deleted": id }))
    }

    fn restore(&self, req: &Value) -> OpResult {
        let keep = req.get("keepInManager").and_then(Value::as_bool).unwrap_or(false);
        let results = if req.get("all").and_then(Value::as_bool) == Some(true) {
            restore::restore_all(&self.core, keep)
        } else if let Some(h) = req.get("harness").and_then(Value::as_str) {
            restore::restore_harness(&self.core, h, keep)
        } else if let Some(id) = req.get("snapshot").and_then(Value::as_str) {
            snapshot::load(&self.core.state_dir, id).and_then(|s| restore::restore(&self.core, &s, keep))
        } else {
            return Err(OpError::Invalid("restore needs `harness`, `all` or `snapshot`".into()));
        };
        to_value(results.map_err(fail)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(d: &std::path::Path) -> RealCoreOps {
        let core = Core { home: d.join("home"), appdata: d.join("app"), state_dir: d.join("state") };
        std::fs::create_dir_all(&core.home).unwrap();
        RealCoreOps::new(core, PathBuf::from("mcpm.exe"))
    }

    #[test]
    fn takeover_then_restore_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let o = ops(d.path());
        let path = o.core.home.join(".cursor").join("mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "{\n  \"mcpServers\": { \"fs\": { \"command\": \"npx\", \"args\": [\"fs\"] } }\n}\n";
        std::fs::write(&path, original).unwrap();

        let plan = o.plan_takeover("cursor").unwrap();
        assert_eq!(plan["files"][0]["remove"], json!(["fs"]));
        o.apply_takeover("cursor").unwrap();
        let list = o.harnesses().unwrap();
        let cursor = list.as_array().unwrap().iter().find(|h| h["id"] == "cursor").unwrap();
        assert_eq!(cursor["state"], "takenOver");
        assert_eq!(mcpm_core::store::load(&o.core.state_dir).unwrap().servers.len(), 1);

        o.restore(&json!({ "harness": "cursor" })).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(mcpm_core::store::load(&o.core.state_dir).unwrap().servers.is_empty());
        assert!(o.plan_takeover("nope").is_err());
    }
}
