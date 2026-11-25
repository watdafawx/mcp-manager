//! One function per CLI subcommand. They print to stdout; errors bubble up to `main`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use mcpm_daemon::update::CliOutcome;
use mcpm_daemon::{AutostartCtl, DaemonOptions, TrayOptions, UpdateOptions};
use mcpm_gui::autostart;
use mcpm_gui::client;
use mcpm_gui::model::{HarnessRow, HarnessState, Plan, ServerRow, SnapRow, Status, UpdateInfo};
use serde_json::{Value, json};

use crate::backend::Backend;
use crate::cli::{AddArgs, OnOff, ServersCmd, build_server};
use crate::print;

/// Prints either the raw JSON or the text built from it.
fn out(json_mode: bool, raw: &Value, text: impl FnOnce() -> String) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(raw).unwrap_or_default());
    } else {
        println!("{}", text());
    }
}

/// The window is one per user: a second `mcpm gui` brings the open one to the front.
pub fn gui() -> Result<()> {
    if mcpm_daemon::tray::focus_gui_window() {
        return Ok(());
    }
    mcpm_gui::run(std::env::current_exe()?)
}

/// The tray's "Start with Windows" checkbox: the same Run key as `mcpm autostart`.
struct RegistryAutostart;

impl AutostartCtl for RegistryAutostart {
    fn is_on(&self) -> bool {
        autostart::current().ok().flatten().is_some()
    }
    fn set(&self, on: bool) -> Result<()> {
        autostart::set(on, &std::env::current_exe()?)
    }
}

/// `cargo run` / `target\debug` builds must never replace themselves with a release.
pub fn in_cargo_target(exe: &Path) -> bool {
    let parts: Vec<String> = exe
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == "target" && matches!(w[1].as_str(), "debug" | "release"))
        || parts
            .windows(3)
            .any(|w| w[0] == "target" && w[2] == "debug" || w[0] == "target" && w[2] == "release")
}

fn updates_disabled(exe: &Path) -> bool {
    let env_off =
        std::env::var("MCPM_NO_UPDATE").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0");
    env_off || in_cargo_target(exe)
}

pub fn daemon(port: Option<u16>, no_tray: bool, wait_pid: Option<u32>) -> Result<()> {
    // An update starts the new daemon while the old one is still shutting down.
    if let Some(pid) = wait_pid {
        mcpm_daemon::update::wait_for_pid(pid, Duration::from_secs(30));
    }
    let exe = std::env::current_exe()?;
    let mut opts = DaemonOptions::new(mcpm_daemon::storage::state_dir());
    opts.port = port;
    opts.core = Arc::new(mcpm_daemon::RealCoreOps::new(
        mcpm_daemon::storage::core_context(),
        exe.clone(),
    ));
    opts.tray = TrayOptions {
        enabled: !no_tray,
        exe: exe.clone(),
        autostart: Some(Arc::new(RegistryAutostart)),
    };
    let mut respawn_args = vec![];
    if no_tray {
        respawn_args.push("--no-tray".to_string());
    }
    if let Some(p) = port {
        respawn_args.extend(["--port".to_string(), p.to_string()]);
    }
    opts.update = UpdateOptions {
        enabled: !updates_disabled(&exe),
        exe,
        respawn_args,
        ..Default::default()
    };
    mcpm_daemon::run_blocking(opts)
}

pub fn update(json_mode: bool, check_only: bool) -> Result<()> {
    let b = Backend::open()?;
    let Some(conn) = b.conn() else {
        // No daemon: check (and install) in this process.
        let home = b.core().state_dir;
        let outcome = mcpm_daemon::update::run_cli(check_only, &home)?;
        let (state, text) = match &outcome {
            CliOutcome::UpToDate(v) => ("upToDate", format!("mcpm {v} is up to date")),
            CliOutcome::Available(v) => (
                "available",
                format!("update {v} is available (mcpm update installs it)"),
            ),
            CliOutcome::Installed(v) => (
                "installed",
                format!("installed {v}; it runs from the next start (mcpm gui / mcpm daemon)"),
            ),
        };
        out(
            json_mode,
            &json!({ "state": state, "message": text }),
            || text.clone(),
        );
        return Ok(());
    };
    let conn = conn.slow();
    let st = update_state(conn.post("/api/update/check", &json!({})))?;
    if !st.available || check_only {
        out(json_mode, &update_raw(&st), || print::update(&st));
        return Ok(());
    }
    let from = st.current.clone();
    let to = st.latest.clone().unwrap_or_default();
    conn.post("/api/update/install", &json!({}))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !json_mode {
        println!("installing {to} (was {from})…");
    }
    let home = b.core().state_dir;
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let Some(c) = client::locate(&home) else {
            continue; // the old daemon is gone, the new one is starting
        };
        if let Ok(v) = c.get("/api/status")
            && v.get("version")
                .and_then(Value::as_str)
                .is_some_and(|x| x != from)
        {
            let msg = format!("updated to {}", v["version"].as_str().unwrap_or("?"));
            out(
                json_mode,
                &json!({ "state": "installed", "message": msg }),
                || msg.clone(),
            );
            return Ok(());
        }
        if let Ok(st) = update_state(c.get("/api/update"))
            && let Some(e) = st.last_error
            && st.phase == "idle"
        {
            bail!("update failed: {e}");
        }
    }
    bail!("the new daemon did not come up within 3 minutes (see daemon.log in the state folder)")
}

fn update_state(r: Result<Value, client::ApiError>) -> Result<UpdateInfo> {
    match r {
        Ok(v) => Ok(serde_json::from_value(v)?),
        Err(e) if e.status == Some(501) => bail!(
            "updates are off in the running daemon (a development build, or MCPM_NO_UPDATE is set)"
        ),
        Err(e) => bail!("{e}"),
    }
}

fn update_raw(st: &UpdateInfo) -> Value {
    json!({
        "current": st.current, "latest": st.latest, "available": st.available,
        "lastCheck": st.last_check, "lastError": st.last_error, "autoUpdate": st.auto_update,
    })
}

pub fn status(json_mode: bool) -> Result<()> {
    let b = Backend::open()?;
    let auto = autostart::current().unwrap_or(None);
    let cfg = mcpm_core::store::load(&b.core().state_dir)?;
    match b.status()? {
        Some(raw) => {
            let s: Status = serde_json::from_value(raw.clone())?;
            let mut raw = raw;
            raw["autostart"] = json!(auto);
            out(json_mode, &raw, || {
                print::status(&s, auto.as_deref(), cfg.port)
            });
        }
        None => {
            let raw = json!({
                "running": false, "port": cfg.port, "servers": cfg.servers.len(),
                "stateDir": b.core().state_dir, "autostart": auto,
            });
            out(json_mode, &raw, || {
                print::status_offline(
                    &b.core().state_dir.display().to_string(),
                    cfg.port,
                    cfg.servers.len(),
                    auto.as_deref(),
                )
            });
        }
    }
    Ok(())
}

pub fn harnesses(json_mode: bool) -> Result<()> {
    let raw = Backend::open()?.harnesses()?;
    out(json_mode, &raw, || {
        print::harnesses(&HarnessRow::from_list(&raw))
    });
    Ok(())
}

pub fn takeover(
    json_mode: bool,
    harness: Option<String>,
    all: bool,
    dry_run: bool,
    text: bool,
) -> Result<()> {
    let b = Backend::open()?;
    if dry_run {
        let ids = target_ids(&b, harness, all)?;
        let mut raws = vec![];
        let mut plans = vec![];
        for id in ids {
            let raw = b.plan(&id)?;
            plans.push(
                Plan::from_value(&raw)
                    .ok_or_else(|| anyhow::anyhow!("unexpected plan reply for {id}"))?,
            );
            raws.push(strip_plan_bytes(raw));
        }
        let raw = Value::Array(raws);
        out(json_mode, &raw, || {
            if plans.is_empty() {
                "nothing to take over".to_string()
            } else {
                let mut s = plans
                    .iter()
                    .map(|p| print::plan(p, text))
                    .collect::<Vec<_>>()
                    .join("\n");
                s.push_str("\n(dry run: nothing written)");
                s
            }
        });
        return Ok(());
    }
    let target = if all {
        "all".to_string()
    } else {
        harness.unwrap_or_default()
    };
    let raw = b.apply(&target)?;
    out(json_mode, &raw, || {
        let snaps = match &raw {
            Value::Array(_) => SnapRow::from_list(&raw),
            v => SnapRow::from_value(v).into_iter().collect(),
        };
        if snaps.is_empty() {
            "nothing to take over".to_string()
        } else {
            format!(
                "took over {}\n{}",
                snaps
                    .iter()
                    .flat_map(|s| s.harnesses())
                    .collect::<Vec<_>>()
                    .join(", "),
                print::snapshots(&snaps)
            )
        }
    });
    Ok(())
}

/// Plans carry the original file bytes as a number array: useless in `--json` output.
fn strip_plan_bytes(mut plan: Value) -> Value {
    if let Some(files) = plan.get_mut("files").and_then(Value::as_array_mut) {
        for f in files {
            if let Some(o) = f.as_object_mut() {
                o.remove("original");
            }
        }
    }
    plan
}

/// Harness ids a takeover would touch.
fn target_ids(b: &Backend, harness: Option<String>, all: bool) -> Result<Vec<String>> {
    if !all {
        return Ok(harness.into_iter().collect());
    }
    let rows = HarnessRow::from_list(&b.harnesses()?);
    Ok(rows
        .into_iter()
        .filter(|h| h.detected && h.state != HarnessState::TakenOver)
        .map(|h| h.id)
        .collect())
}

pub fn restore(
    json_mode: bool,
    harness: Option<String>,
    all: bool,
    snapshot: Option<String>,
    keep: bool,
) -> Result<()> {
    let b = Backend::open()?;
    let mut req = if all {
        json!({ "all": true })
    } else if let Some(s) = snapshot {
        json!({ "snapshot": s })
    } else if let Some(h) = harness {
        json!({ "harness": h })
    } else {
        bail!("give a harness id, --all or --snapshot <id>");
    };
    req["keepInManager"] = json!(keep);
    let raw = b.restore(&req)?;
    out(json_mode, &raw, || restore_text(&raw));
    Ok(())
}

fn restore_text(raw: &Value) -> String {
    let list = raw.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "nothing to restore".into();
    }
    list.iter()
        .map(|r| {
            let merged = r.get("merged").and_then(Value::as_bool) == Some(true);
            let path = r.get("path").and_then(Value::as_str).unwrap_or("?");
            format!(
                "restored {path}{}",
                if merged {
                    " (merged: the harness had edited it since)"
                } else {
                    ""
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn snapshots(json_mode: bool) -> Result<()> {
    let raw = Backend::open()?.snapshots()?;
    out(json_mode, &raw, || {
        print::snapshots(&SnapRow::from_list(&raw))
    });
    Ok(())
}

pub fn servers(json_mode: bool, action: Option<ServersCmd>) -> Result<()> {
    let b = Backend::open()?;
    match action.unwrap_or(ServersCmd::List) {
        ServersCmd::List => {
            let raw = b.servers()?;
            out(json_mode, &raw, || {
                let mut s = print::servers(&ServerRow::from_list(&raw));
                if b.conn().is_none() {
                    s.push_str("\n(daemon not running: state is read from config.json)");
                }
                s
            });
        }
        ServersCmd::Add(a) => add(json_mode, &b, &a)?,
        ServersCmd::Remove { id } => {
            b.remove_server(&id)?;
            done(json_mode, "removed", &id);
        }
        ServersCmd::Enable { id } => {
            b.set_enabled(&id, true)?;
            done(json_mode, "enabled", &id);
        }
        ServersCmd::Disable { id } => {
            b.set_enabled(&id, false)?;
            done(json_mode, "disabled", &id);
        }
    }
    Ok(())
}

fn add(json_mode: bool, b: &Backend, a: &AddArgs) -> Result<()> {
    let cfg = build_server(a)?;
    b.add_server(&cfg)?;
    done(json_mode, "added", &cfg.id);
    Ok(())
}

fn done(json_mode: bool, what: &str, id: &str) {
    if json_mode {
        println!("{}", json!({ "ok": true, "action": what, "id": id }));
    } else {
        println!("{what} {id}");
    }
}

pub fn autostart(json_mode: bool, state: OnOff) -> Result<()> {
    let exe = std::env::current_exe()?;
    match state {
        OnOff::On => autostart::set(true, &exe)?,
        OnOff::Off => autostart::set(false, &exe)?,
        OnOff::Status => {}
    }
    let cur = autostart::current()?;
    let raw = json!({ "autostart": cur.is_some(), "value": cur });
    out(json_mode, &raw, || match &cur {
        Some(v) => format!("autostart: on\n{v}"),
        None => "autostart: off".to_string(),
    });
    Ok(())
}

pub fn uninstall(json_mode: bool) -> Result<()> {
    let b = Backend::open()?;
    let mut steps: Vec<String> = vec![];
    let restored = b.restore(&json!({ "all": true, "keepInManager": false }))?;
    steps.push(restore_text(&restored));
    autostart::set(false, &std::env::current_exe()?)?;
    steps.push("autostart off".into());
    steps.push(if b.shutdown()? {
        "daemon stopped".into()
    } else {
        "daemon was not running".into()
    });
    let raw = json!({ "ok": true, "restored": restored, "steps": steps });
    out(json_mode, &raw, || steps.join("\n"));
    Ok(())
}
