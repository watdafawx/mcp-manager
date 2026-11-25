//! Shared types. The daemon and GUI code against these; change them only with care.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 39494;
pub const MANAGER_ENTRY: &str = "mcpm";
/// Entry written by the old T3 Patcher `mcp-relay` plugin; never imported.
pub const LEGACY_RELAY_ENTRY: &str = "t3-relay";
/// `pinned_tools` entry that exposes every tool of a server directly.
pub const PIN_ALL: &str = "*";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerConfig {
    pub version: u32,
    pub port: u16,
    #[serde(default)]
    pub autostart: bool,
    /// Download and install new releases by itself (when no tool call is running).
    #[serde(default = "yes")]
    pub auto_update: bool,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            version: 1,
            port: DEFAULT_PORT,
            autostart: true,
            auto_update: true,
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub transport: Transport,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: InstanceMode,
    #[serde(default)]
    pub pinned_tools: Vec<String>,
    /// Harness ids this server is exposed to. Empty = every harness.
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// Seconds to wait for `initialize` and the first `tools/list` (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_timeout_secs: Option<u32>,
    /// Stop the child after this many minutes without a call (default 15; 0 = never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_mins: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

impl ServerConfig {
    /// True when every tool of this server is exposed directly (`pinned_tools` holds `*`).
    pub fn pins_all(&self) -> bool {
        self.pinned_tools.iter().any(|t| t == PIN_ALL)
    }

    /// True when `tool` is exposed directly as `<id>__<tool>`.
    pub fn pins(&self, tool: &str) -> bool {
        self.pinned_tools.iter().any(|t| t == PIN_ALL || t == tool)
    }

    pub fn is_exposed_to(&self, harness: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        match harness {
            None => true,
            Some(_) if self.harnesses.is_empty() => true,
            Some(h) => self.harnesses.iter().any(|x| x == h || x == "all"),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstanceMode {
    /// One child process shared by every caller.
    #[default]
    Shared,
    /// One child per MCP session (stateful servers such as browsers).
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Scope {
    Global,
    Project,
}

/// Where a server was imported from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub harness: String,
    pub path: String,
    pub entry: String,
    pub scope: Scope,
    /// Snapshot id of the takeover that imported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// Validates a server id: ^[a-z0-9][a-z0-9_-]{0,47}$ (case-insensitive).
pub fn valid_server_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 48
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_update_defaults_on_and_round_trips() {
        // A config written before the setting existed keeps updating.
        let old: ManagerConfig = serde_json::from_str(r#"{"version":1,"port":40000}"#).unwrap();
        assert!(old.auto_update);
        let c = ManagerConfig {
            auto_update: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""autoUpdate":false"#));
        assert!(
            !serde_json::from_str::<ManagerConfig>(&json)
                .unwrap()
                .auto_update
        );
    }

    #[test]
    fn pins() {
        let mut s = ServerConfig {
            id: "x".into(),
            name: "x".into(),
            transport: Transport::Http {
                url: "http://h".into(),
                headers: Default::default(),
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec!["a".into()],
            harnesses: vec![],
            start_timeout_secs: None,
            idle_timeout_mins: None,
            origin: None,
        };
        assert!(s.pins("a") && !s.pins("b") && !s.pins_all());
        s.pinned_tools = vec![PIN_ALL.into()];
        assert!(s.pins("a") && s.pins("b") && s.pins_all());
    }

    #[test]
    fn ids() {
        assert!(valid_server_id("playwright"));
        assert!(valid_server_id("A_b-9"));
        assert!(!valid_server_id("-x"));
        assert!(!valid_server_id(""));
        assert!(!valid_server_id(&"a".repeat(49)));
    }

    #[test]
    fn exposure() {
        let mut s = ServerConfig {
            id: "x".into(),
            name: "x".into(),
            transport: Transport::Http {
                url: "http://h".into(),
                headers: Default::default(),
            },
            enabled: true,
            mode: InstanceMode::Shared,
            pinned_tools: vec![],
            harnesses: vec![],
            start_timeout_secs: None,
            idle_timeout_mins: None,
            origin: None,
        };
        assert!(s.is_exposed_to(Some("codex")));
        s.harnesses = vec!["claude".into()];
        assert!(!s.is_exposed_to(Some("codex")));
        assert!(s.is_exposed_to(Some("claude")));
        assert!(s.is_exposed_to(None));
        s.enabled = false;
        assert!(!s.is_exposed_to(Some("claude")));
    }

    #[test]
    fn roundtrip() {
        let cfg = ManagerConfig::default();
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<ManagerConfig>(&text).unwrap(), cfg);
    }
}
