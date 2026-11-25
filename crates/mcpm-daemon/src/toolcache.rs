//! Last known tool list of every server, kept in `tools-cache.json` in the state dir, so pinned
//! tools can be listed without starting their server. An entry only counts while the server's
//! transport is unchanged (fingerprint).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mcpm_core::model::ServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FILE: &str = "tools-cache.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    fingerprint: String,
    tools: Vec<Value>,
}

pub struct ToolCache {
    path: Option<PathBuf>,
    map: Mutex<BTreeMap<String, Entry>>,
}

/// FNV-1a over the serialized transport: stable across builds (unlike `DefaultHasher`).
pub fn fingerprint(cfg: &ServerConfig) -> String {
    let text = serde_json::to_string(&cfg.transport).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl ToolCache {
    /// Loads the cache from `home` (`None` = memory only). A missing or broken file is empty.
    pub fn load(home: Option<&Path>) -> ToolCache {
        let path = home.map(|h| h.join(FILE));
        let map = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        ToolCache {
            path,
            map: Mutex::new(map),
        }
    }

    /// Tools last seen for this server, if its transport has not changed since.
    pub fn get(&self, cfg: &ServerConfig) -> Option<Vec<Value>> {
        let m = self.map.lock().unwrap();
        m.get(&cfg.id)
            .filter(|e| e.fingerprint == fingerprint(cfg))
            .map(|e| e.tools.clone())
    }

    /// Stores the tools of a server; writes the file only when something changed.
    pub fn put(&self, cfg: &ServerConfig, tools: &[Value]) {
        let entry = Entry {
            fingerprint: fingerprint(cfg),
            tools: tools.to_vec(),
        };
        let snapshot = {
            let mut m = self.map.lock().unwrap();
            if m.get(&cfg.id) == Some(&entry) {
                return;
            }
            m.insert(cfg.id.clone(), entry);
            m.clone()
        };
        self.write(&snapshot);
    }

    /// Drops entries of servers that no longer exist.
    pub fn retain(&self, ids: &[&str]) {
        let snapshot = {
            let mut m = self.map.lock().unwrap();
            let before = m.len();
            m.retain(|k, _| ids.contains(&k.as_str()));
            if m.len() == before {
                return;
            }
            m.clone()
        };
        self.write(&snapshot);
    }

    fn write(&self, map: &BTreeMap<String, Entry>) {
        let Some(path) = &self.path else { return };
        let Ok(text) = serde_json::to_vec(map) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpm_core::model::{InstanceMode, Transport};
    use serde_json::json;

    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig {
            id: "s".into(),
            name: "s".into(),
            transport: Transport::Stdio {
                command: cmd.into(),
                args: vec![],
                env: Default::default(),
                cwd: None,
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec![],
            harnesses: vec![],
            start_timeout_secs: None,
            idle_timeout_mins: None,
            origin: None,
        }
    }

    #[test]
    fn persists_and_invalidates_on_transport_change() {
        let dir = std::env::temp_dir().join(format!("mcpm-tc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = ToolCache::load(Some(&dir));
        assert!(c.get(&cfg("a")).is_none());
        c.put(&cfg("a"), &[json!({ "name": "t" })]);
        let again = ToolCache::load(Some(&dir));
        assert_eq!(again.get(&cfg("a")).unwrap()[0]["name"], "t");
        assert!(again.get(&cfg("b")).is_none(), "other command = stale");
        again.retain(&[]);
        assert!(ToolCache::load(Some(&dir)).get(&cfg("a")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
