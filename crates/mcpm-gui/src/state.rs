//! Window-independent GUI state: everything the worker thread reports lands here through
//! [`AppData::apply`], so the reducer is unit-testable without egui.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use mcpm_core::model::ServerConfig;

use crate::model::{
    CallRow, HarnessRow, Plan, ServerRow, SnapRow, Status, ToolInfo, UpdateInfo, compare_versions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Servers,
    Harnesses,
    Snapshots,
    Settings,
}

impl Page {
    pub const ALL: [Page; 4] = [
        Page::Servers,
        Page::Harnesses,
        Page::Snapshots,
        Page::Settings,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Page::Servers => "Servers",
            Page::Harnesses => "Harnesses",
            Page::Snapshots => "Snapshots",
            Page::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConnState {
    #[default]
    Starting,
    Online,
    Offline(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

const INFO_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum ToolsState {
    Loading,
    Ready(Vec<ToolInfo>),
    Failed(String),
}

/// Dry-run result shown before a takeover is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanView {
    /// Harness id or `"all"`.
    pub target: String,
    pub plans: Vec<Plan>,
    /// Harnesses whose dry run failed: `(id, message)`.
    pub errors: Vec<(String, String)>,
}

/// The notice strip above the pages about versions and updates.
#[derive(Debug, Clone, PartialEq)]
pub enum Banner {
    /// The daemon runs a newer build than this window (an update was installed).
    DaemonNewer { daemon: String },
    /// The daemon runs an older build than this window.
    DaemonOlder { daemon: String },
    /// A newer release exists.
    UpdateAvailable { version: String, auto: bool },
}

/// What the worker thread tells the window.
#[derive(Debug, Clone)]
pub enum Msg {
    Starting,
    Status(Status),
    Offline(String),
    Servers(Vec<ServerRow>),
    Harnesses(Vec<HarnessRow>),
    Snapshots(Vec<SnapRow>),
    Calls(Vec<CallRow>),
    Tools {
        id: String,
        result: Result<Vec<ToolInfo>, String>,
    },
    Revealed {
        id: String,
        cfg: Box<ServerConfig>,
    },
    Plan(PlanView),
    Toast(Level, String),
    Busy(i32),
    /// Registry autostart value (`None` = off).
    Autostart(Option<String>),
    /// `GET /api/update`; `None` = this daemon has updates switched off.
    Update(Option<UpdateInfo>),
    /// Uninstall finished: the window should close.
    Quit,
}

#[derive(Debug, Default)]
pub struct AppData {
    pub conn: ConnState,
    pub status: Option<Status>,
    pub servers: Vec<ServerRow>,
    pub harnesses: Vec<HarnessRow>,
    pub snapshots: Vec<SnapRow>,
    pub calls: Vec<CallRow>,
    pub tools: HashMap<String, ToolsState>,
    /// Unmasked configs fetched on request, by server id.
    pub revealed: HashMap<String, ServerConfig>,
    pub plan: Option<PlanView>,
    pub toasts: Vec<Toast>,
    pub busy: i32,
    pub autostart: Option<String>,
    pub update: Option<UpdateInfo>,
    pub quit: bool,
    /// Bumped on every applied message that changes server data (UI uses it to resync forms).
    pub servers_rev: u64,
    next_toast: u64,
}

impl AppData {
    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Starting => self.conn = ConnState::Starting,
            Msg::Status(s) => {
                self.conn = ConnState::Online;
                self.status = Some(s);
            }
            Msg::Offline(e) => {
                self.conn = ConnState::Offline(e);
                self.status = None;
            }
            Msg::Servers(s) => {
                // Tool lists and revealed secrets belong to servers that still exist.
                self.tools.retain(|id, _| s.iter().any(|r| &r.cfg.id == id));
                self.revealed
                    .retain(|id, _| s.iter().any(|r| &r.cfg.id == id));
                self.servers = s;
                self.servers_rev += 1;
            }
            Msg::Harnesses(h) => self.harnesses = h,
            Msg::Snapshots(s) => self.snapshots = s,
            Msg::Calls(c) => self.calls = c,
            Msg::Tools { id, result } => {
                self.tools.insert(
                    id,
                    match result {
                        Ok(t) => ToolsState::Ready(t),
                        Err(e) => ToolsState::Failed(e),
                    },
                );
            }
            Msg::Revealed { id, cfg } => {
                self.revealed.insert(id, *cfg);
            }
            Msg::Plan(p) => self.plan = Some(p),
            Msg::Toast(level, text) => self.toast(level, text),
            Msg::Busy(d) => self.busy = (self.busy + d).max(0),
            Msg::Autostart(a) => self.autostart = a,
            Msg::Update(u) => self.update = u,
            Msg::Quit => self.quit = true,
        }
    }

    pub fn toast(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        // The same error repeating every poll must not stack up.
        if self
            .toasts
            .iter()
            .any(|t| t.level == level && t.text == text)
        {
            return;
        }
        self.next_toast += 1;
        self.toasts.push(Toast {
            id: self.next_toast,
            level,
            text,
            at: Instant::now(),
        });
        let excess = self.toasts.len().saturating_sub(5);
        self.toasts.drain(..excess);
    }

    pub fn dismiss_toast(&mut self, id: u64) {
        self.toasts.retain(|t| t.id != id);
    }

    /// Info toasts fade after a few seconds; errors stay until dismissed.
    pub fn prune_toasts(&mut self, now: Instant) {
        self.toasts
            .retain(|t| t.level == Level::Error || now.duration_since(t.at) < INFO_TTL);
    }

    /// What to tell the user about versions; `window` is this build's version.
    pub fn banner(&self, window: &str) -> Option<Banner> {
        let s = self
            .status
            .as_ref()
            .filter(|_| self.conn == ConnState::Online)?;
        match compare_versions(window, &s.version) {
            Some(std::cmp::Ordering::Greater) => Some(Banner::DaemonNewer {
                daemon: s.version.clone(),
            }),
            Some(std::cmp::Ordering::Less) => Some(Banner::DaemonOlder {
                daemon: s.version.clone(),
            }),
            _ => s
                .update_available
                .clone()
                .map(|version| Banner::UpdateAvailable {
                    version,
                    auto: s.auto_update,
                }),
        }
    }

    pub fn server(&self, id: &str) -> Option<&ServerRow> {
        self.servers.iter().find(|s| s.cfg.id == id)
    }

    pub fn harness(&self, id: &str) -> Option<&HarnessRow> {
        self.harnesses.iter().find(|h| h.id == id)
    }

    /// Harness ids to show as exposure badges: detected ones, plus any a server already lists.
    pub fn badge_harnesses(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .harnesses
            .iter()
            .filter(|h| h.detected)
            .map(|h| (h.id.clone(), h.label.clone()))
            .collect();
        for s in &self.servers {
            for h in &s.cfg.harnesses {
                if h != "all" && !out.iter().any(|(id, _)| id == h) {
                    let label = self
                        .harness(h)
                        .map(|x| x.label.clone())
                        .unwrap_or_else(|| h.clone());
                    out.push((h.clone(), label));
                }
            }
        }
        out
    }
}

/// Exposure after clicking `clicked`'s badge. `all` = every harness id the badges show.
/// Returns `None` if the click would leave the server exposed to nothing (not representable:
/// an empty list means "every harness").
pub fn toggle_exposure(current: &[String], all: &[String], clicked: &str) -> Option<Vec<String>> {
    let exposed_everywhere = current.is_empty() || current.iter().any(|h| h == "all");
    let mut set: Vec<String> = if exposed_everywhere {
        all.to_vec()
    } else {
        current.to_vec()
    };
    if let Some(pos) = set.iter().position(|h| h == clicked) {
        set.remove(pos);
        if set.is_empty() {
            return None;
        }
    } else {
        set.push(clicked.to_string());
    }
    // Everything selected again = back to the open-ended "every harness".
    if all.iter().all(|a| set.contains(a)) {
        return Some(vec![]);
    }
    Some(set)
}

/// Whether a server's badge for `harness` is lit.
pub fn exposed_to(cfg: &ServerConfig, harness: &str) -> bool {
    cfg.harnesses.is_empty() || cfg.harnesses.iter().any(|h| h == harness || h == "all")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, harnesses: &[&str]) -> ServerRow {
        ServerRow::from_value(&json!({
            "id": id, "name": id, "enabled": true,
            "transport": {"type": "http", "url": "http://x"},
            "harnesses": harnesses, "status": "idle"
        }))
        .unwrap()
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn status_makes_online_and_offline_clears_it() {
        let mut d = AppData::default();
        assert_eq!(d.conn, ConnState::Starting);
        d.apply(Msg::Status(Status {
            port: 1,
            ..Default::default()
        }));
        assert_eq!(d.conn, ConnState::Online);
        d.apply(Msg::Offline("down".into()));
        assert!(d.status.is_none());
        assert_eq!(d.conn, ConnState::Offline("down".into()));
    }

    #[test]
    fn servers_update_drops_stale_tool_lists() {
        let mut d = AppData::default();
        d.apply(Msg::Tools {
            id: "a".into(),
            result: Ok(vec![]),
        });
        d.apply(Msg::Tools {
            id: "b".into(),
            result: Err("x".into()),
        });
        d.apply(Msg::Servers(vec![row("a", &[])]));
        assert!(d.tools.contains_key("a") && !d.tools.contains_key("b"));
        assert_eq!(d.servers_rev, 1);
    }

    #[test]
    fn toasts_dedupe_prune_and_dismiss() {
        let mut d = AppData::default();
        d.toast(Level::Error, "boom");
        d.toast(Level::Error, "boom");
        d.toast(Level::Info, "ok");
        assert_eq!(d.toasts.len(), 2);
        d.prune_toasts(Instant::now() + Duration::from_secs(60));
        assert_eq!(d.toasts.len(), 1, "errors stay, infos expire");
        let id = d.toasts[0].id;
        d.dismiss_toast(id);
        assert!(d.toasts.is_empty());
    }

    fn online(version: &str, update: Option<&str>) -> AppData {
        let mut d = AppData::default();
        d.apply(Msg::Status(Status {
            version: version.into(),
            update_available: update.map(String::from),
            auto_update: true,
            ..Default::default()
        }));
        d
    }

    #[test]
    fn banner_by_version() {
        assert_eq!(AppData::default().banner("0.2.0"), None, "offline: none");
        assert_eq!(online("0.2.0", None).banner("0.2.0"), None);
        assert_eq!(
            online("0.3.0", None).banner("0.2.0"),
            Some(Banner::DaemonNewer {
                daemon: "0.3.0".into()
            })
        );
        assert_eq!(
            online("0.1.0", None).banner("0.2.0"),
            Some(Banner::DaemonOlder {
                daemon: "0.1.0".into()
            })
        );
        assert_eq!(
            online("0.2.0", Some("0.3.0")).banner("0.2.0"),
            Some(Banner::UpdateAvailable {
                version: "0.3.0".into(),
                auto: true
            })
        );
        // A version mismatch outranks "update available".
        assert!(matches!(
            online("0.3.0", Some("0.4.0")).banner("0.2.0"),
            Some(Banner::DaemonNewer { .. })
        ));
        // An unparsable daemon version never nags.
        assert_eq!(online("dev", None).banner("0.2.0"), None);
    }

    #[test]
    fn update_info_is_kept_and_cleared() {
        let mut d = AppData::default();
        d.apply(Msg::Update(Some(UpdateInfo {
            current: "0.2.0".into(),
            ..Default::default()
        })));
        assert_eq!(d.update.as_ref().unwrap().current, "0.2.0");
        d.apply(Msg::Update(None));
        assert!(d.update.is_none());
    }

    #[test]
    fn busy_never_goes_negative() {
        let mut d = AppData::default();
        d.apply(Msg::Busy(-1));
        d.apply(Msg::Busy(1));
        d.apply(Msg::Busy(1));
        d.apply(Msg::Busy(-1));
        assert_eq!(d.busy, 1);
    }

    #[test]
    fn badges_cover_detected_and_listed_harnesses() {
        let mut d = AppData::default();
        d.apply(Msg::Harnesses(HarnessRow::from_list(&json!([
            {"id":"claude","label":"Claude Code","detected":true,"state":"connected","files":[]},
            {"id":"cursor","label":"Cursor","detected":false,"state":"notConnected","files":[]}
        ]))));
        d.apply(Msg::Servers(vec![row("a", &["codex"])]));
        let b: Vec<String> = d.badge_harnesses().into_iter().map(|(id, _)| id).collect();
        assert_eq!(b, vec!["claude", "codex"]);
    }

    #[test]
    fn exposure_toggle_rules() {
        let all = ids(&["claude", "codex", "cursor"]);
        // from "everywhere": turning one off materialises the rest
        assert_eq!(
            toggle_exposure(&[], &all, "codex"),
            Some(ids(&["claude", "cursor"]))
        );
        // turning it back on restores the open-ended "every harness"
        assert_eq!(
            toggle_exposure(&ids(&["claude", "cursor"]), &all, "codex"),
            Some(vec![])
        );
        // explicit list, add one that is not everything
        assert_eq!(
            toggle_exposure(&ids(&["claude"]), &all, "codex"),
            Some(ids(&["claude", "codex"]))
        );
        // last one cannot be removed
        assert_eq!(toggle_exposure(&ids(&["claude"]), &all, "claude"), None);
        // legacy "all" marker behaves like empty
        assert_eq!(
            toggle_exposure(&ids(&["all"]), &all, "cursor"),
            Some(ids(&["claude", "codex"]))
        );
    }

    #[test]
    fn exposed_to_matches_core_semantics() {
        let r = row("a", &["claude"]);
        assert!(exposed_to(&r.cfg, "claude") && !exposed_to(&r.cfg, "codex"));
        assert!(exposed_to(&row("b", &[]).cfg, "codex"));
    }
}
