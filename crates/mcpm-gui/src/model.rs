//! Typed views of the control API's JSON. Shared by the GUI and the CLI printers.

use std::path::PathBuf;

use mcpm_core::model::ServerConfig;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub port: u16,
    pub started_at: String,
    pub uptime_secs: u64,
    pub servers: usize,
    pub enabled_servers: usize,
    pub running_instances: usize,
    pub sessions: usize,
    pub home: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
    Error,
    Disabled,
    /// Daemon is down; CLI read the config file directly.
    Stopped,
}

impl RunState {
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "error" => Self::Error,
            "disabled" => Self::Disabled,
            "stopped" => Self::Stopped,
            _ => Self::Idle,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Error => "error",
            Self::Disabled => "disabled",
            Self::Stopped => "stopped",
        }
    }
}

/// One entry of `GET /api/servers`: the masked config plus live state.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerRow {
    pub cfg: ServerConfig,
    pub state: RunState,
    pub tool_count: usize,
    pub active_sessions: usize,
    pub last_error: Option<String>,
}

impl ServerRow {
    pub fn from_value(v: &Value) -> Option<Self> {
        let cfg: ServerConfig = serde_json::from_value(v.clone()).ok()?;
        let state = v
            .get("status")
            .and_then(Value::as_str)
            .map(RunState::parse)
            .unwrap_or(if cfg.enabled { RunState::Idle } else { RunState::Disabled });
        Some(Self {
            tool_count: v.get("toolCount").and_then(Value::as_u64).unwrap_or(0) as usize,
            active_sessions: v.get("activeSessions").and_then(Value::as_u64).unwrap_or(0) as usize,
            last_error: v.get("lastError").and_then(Value::as_str).map(str::to_string),
            cfg,
            state,
        })
    }

    pub fn from_list(v: &Value) -> Vec<Self> {
        v.as_array().map(|a| a.iter().filter_map(Self::from_value).collect()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessState {
    #[default]
    NotConnected,
    Connected,
    TakenOver,
}

impl HarnessState {
    pub fn parse(s: &str) -> Self {
        match s {
            "takenOver" => Self::TakenOver,
            "connected" => Self::Connected,
            _ => Self::NotConnected,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::NotConnected => "not connected",
            Self::Connected => "connected alongside",
            Self::TakenOver => "taken over",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessFileInfo {
    pub path: String,
    pub entries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessRow {
    pub id: String,
    pub label: String,
    pub detected: bool,
    pub state: HarnessState,
    pub files: Vec<HarnessFileInfo>,
}

impl HarnessRow {
    pub fn from_value(v: &Value) -> Option<Self> {
        let id = v.get("id")?.as_str()?.to_string();
        let files = v
            .get("files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|f| HarnessFileInfo {
                        path: f.get("path").and_then(Value::as_str).unwrap_or("").to_string(),
                        entries: f
                            .get("entries")
                            .and_then(Value::as_array)
                            .map(|e| e.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            label: v.get("label").and_then(Value::as_str).unwrap_or(&id).to_string(),
            detected: v.get("detected").and_then(Value::as_bool).unwrap_or(false),
            state: HarnessState::parse(v.get("state").and_then(Value::as_str).unwrap_or("")),
            files,
            id,
        })
    }

    pub fn from_list(v: &Value) -> Vec<Self> {
        v.as_array().map(|a| a.iter().filter_map(Self::from_value).collect()).unwrap_or_default()
    }

    /// Entries other than our own, across every file.
    pub fn foreign_entries(&self) -> Vec<&str> {
        self.files
            .iter()
            .flat_map(|f| f.entries.iter())
            .map(String::as_str)
            .filter(|n| *n != mcpm_core::model::MANAGER_ENTRY)
            .collect()
    }

    /// Path to show: the first file that has entries, else the first file.
    pub fn main_path(&self) -> &str {
        self.files.first().map(|f| f.path.as_str()).unwrap_or("")
    }
}

/// One file of a takeover dry run.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFile {
    pub path: String,
    pub imports: Vec<ServerConfig>,
    pub remove: Vec<String>,
    pub final_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub harness: String,
    pub files: Vec<PlanFile>,
}

impl Plan {
    pub fn from_value(v: &Value) -> Option<Self> {
        let harness = v.get("harness")?.as_str()?.to_string();
        let files = v
            .get("files")?
            .as_array()?
            .iter()
            .map(|f| PlanFile {
                path: f.pointer("/file/path").and_then(Value::as_str).unwrap_or("").to_string(),
                imports: f
                    .get("imports")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|s| serde_json::from_value(s.clone()).ok()).collect())
                    .unwrap_or_default(),
                remove: f
                    .get("remove")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                final_text: f.get("final_text").and_then(Value::as_str).unwrap_or("").to_string(),
            })
            .collect();
        Some(Self { harness, files })
    }

    pub fn import_count(&self) -> usize {
        self.files.iter().map(|f| f.imports.len()).sum()
    }
    pub fn remove_count(&self) -> usize {
        self.files.iter().map(|f| f.remove.len()).sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapFile {
    pub harness: String,
    pub path: PathBuf,
    pub existed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapRow {
    pub id: String,
    /// Unix epoch milliseconds.
    pub timestamp: u64,
    pub operation: String,
    pub files: Vec<SnapFile>,
}

impl SnapRow {
    pub fn from_value(v: &Value) -> Option<Self> {
        let files = v
            .get("files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|f| SnapFile {
                        harness: f.get("harness").and_then(Value::as_str).unwrap_or("").to_string(),
                        path: PathBuf::from(f.pointer("/file/path").and_then(Value::as_str).unwrap_or("")),
                        existed: f.get("existed").and_then(Value::as_bool).unwrap_or(true),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            timestamp: v.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
            operation: v.get("operation").and_then(Value::as_str).unwrap_or("").to_string(),
            files,
        })
    }

    /// Newest first.
    pub fn from_list(v: &Value) -> Vec<Self> {
        let mut out: Vec<Self> =
            v.as_array().map(|a| a.iter().filter_map(Self::from_value).collect()).unwrap_or_default();
        out.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
        out
    }

    pub fn harnesses(&self) -> Vec<&str> {
        let mut h: Vec<&str> = self.files.iter().map(|f| f.harness.as_str()).collect();
        h.dedup();
        h
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallRow {
    pub timestamp: u64,
    pub harness: Option<String>,
    pub server_id: String,
    pub tool: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub error: Option<String>,
}

impl CallRow {
    pub fn from_list(v: &Value) -> Vec<Self> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|c| Self {
                        timestamp: c.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
                        harness: c.get("harness").and_then(Value::as_str).map(str::to_string),
                        server_id: c.get("serverId").and_then(Value::as_str).unwrap_or("").to_string(),
                        tool: c.get("tool").and_then(Value::as_str).unwrap_or("").to_string(),
                        ok: c.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        duration_ms: c.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
                        error: c.get("error").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

impl ToolInfo {
    pub fn from_list(v: &Value) -> Vec<Self> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some(Self {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// `unix ms` -> `YYYY-MM-DD HH:MM:SS` (UTC), no time crate needed.
pub fn format_time(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub fn format_uptime(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn server_row_parses_live_fields() {
        let v = json!({
            "id": "fs", "name": "Files", "enabled": true, "mode": "shared",
            "transport": { "type": "stdio", "command": "npx", "args": ["-y", "x"], "env": {"K": "••••"} },
            "harnesses": ["claude"], "status": "running", "toolCount": 7, "activeSessions": 1, "lastError": null
        });
        let r = ServerRow::from_value(&v).unwrap();
        assert_eq!((r.cfg.id.as_str(), r.state, r.tool_count), ("fs", RunState::Running, 7));
        assert!(r.last_error.is_none());
        assert_eq!(ServerRow::from_list(&json!([v, {"bad": 1}])).len(), 1);
    }

    #[test]
    fn harness_state_and_entries() {
        let v = json!({"id":"cursor","label":"Cursor","detected":true,"state":"takenOver",
            "files":[{"path":"C:/x/mcp.json","entries":["mcpm","fs"]}]});
        let h = HarnessRow::from_value(&v).unwrap();
        assert_eq!(h.state, HarnessState::TakenOver);
        assert_eq!(h.foreign_entries(), vec!["fs"]);
        assert_eq!(h.main_path(), "C:/x/mcp.json");
    }

    #[test]
    fn plan_ignores_original_bytes() {
        let v = json!({"harness":"cursor","files":[{
            "file":{"path":"p"}, "original":[1,2,3],
            "imports":[{"id":"fs","name":"fs","transport":{"type":"http","url":"http://x"}}],
            "remove":["fs"], "final_text":"{}"}]});
        let p = Plan::from_value(&v).unwrap();
        assert_eq!((p.import_count(), p.remove_count()), (1, 1));
        assert_eq!(p.files[0].final_text, "{}");
    }

    #[test]
    fn snapshots_sorted_newest_first() {
        let v = json!([
            {"id":"a","timestamp":1,"operation":"takeover","files":[]},
            {"id":"b","timestamp":9,"operation":"restore","files":[{"harness":"x","file":{"path":"p"},"existed":false}]}
        ]);
        let s = SnapRow::from_list(&v);
        assert_eq!(s[0].id, "b");
        assert!(!s[0].files[0].existed);
    }

    #[test]
    fn time_formats() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(1_700_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(format_uptime(75), "1m 15s");
        assert_eq!(format_uptime(7300), "2h 1m");
    }
}
