//! Settings page: daemon info, port, autostart, state folder, uninstall.

use eframe::egui::{self, RichText, Ui};

use crate::app::{App, modal};
use crate::autostart;
use crate::model::{HarnessState, WINDOW_VERSION, format_uptime};
use crate::state::ConnState;
use crate::theme::{self, Palette};
use crate::worker::Job;

#[derive(Default)]
pub struct State {
    /// Text of the port field; `None` = show the live port.
    port_text: Option<String>,
    confirm_uninstall: bool,
}

pub fn show(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let mut jobs: Vec<Job> = vec![];
    let mut open_dir = false;
    let mut errors: Vec<String> = vec![];
    ui.heading("Settings");
    ui.add_space(6.0);

    let online = app.data.conn == ConnState::Online;
    let status = app.data.status.clone();
    let harness_in_use = app
        .data
        .harnesses
        .iter()
        .any(|h| h.state != HarnessState::NotConnected);
    let exe = std::env::current_exe().unwrap_or_default();

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        card(ui, "Daemon", |ui| {
            egui::Grid::new("daemon_info").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).color(pal.muted));
                    ui.label(v);
                    ui.end_row();
                };
                match &status {
                    Some(s) => {
                        row(ui, "Version", s.version.clone());
                        row(ui, "Process", format!("pid {} · up {}", s.pid, format_uptime(s.uptime_secs)));
                        row(ui, "Servers", format!("{} ({} enabled, {} running)", s.servers, s.enabled_servers, s.running_instances));
                    }
                    None => row(ui, "State", "not running".into()),
                }
                row(ui, "State folder", app.state_dir.display().to_string());
            });
            ui.horizontal(|ui| {
                if ui.button("Open state folder").clicked() {
                    open_dir = true;
                }
                if online {
                    if ui.button("Stop daemon").on_hover_text("Servers stop with it. The window stays open.").clicked() {
                        jobs.push(Job::StopDaemon);
                    }
                } else if ui.button("Start daemon").clicked() {
                    jobs.push(Job::Start);
                }
            });
        });

        card(ui, "Port", |ui| {
            let live = status.as_ref().map(|s| s.port).unwrap_or(0);
            let mut text = app.settings.port_text.clone().unwrap_or_else(|| live.to_string());
            let mut applied = false;
            ui.horizontal(|ui| {
                ui.add_enabled(
                    online && !harness_in_use,
                    egui::TextEdit::singleline(&mut text).desired_width(80.0),
                );
                let parsed = text.trim().parse::<u16>().ok().filter(|p| *p >= 1024);
                let changed = parsed.is_some_and(|p| p != live);
                if ui.add_enabled(online && !harness_in_use && changed, egui::Button::new("Apply & restart daemon")).clicked()
                    && let Some(p) = parsed
                {
                    jobs.push(Job::SetPort(p));
                    applied = true;
                }
                if parsed.is_none() {
                    ui.label(RichText::new("1024–65535").small().color(pal.err));
                }
            });
            app.settings.port_text = if applied { None } else { Some(text) };
            if harness_in_use {
                ui.label(
                    RichText::new("Harness entries embed this port. Restore every harness first, then change it.")
                        .small()
                        .color(pal.warn),
                );
            } else {
                ui.label(RichText::new("Loopback only (127.0.0.1). Default 39494.").small().color(pal.muted));
            }
        });

        card(ui, "Autostart", |ui| {
            let on = app.data.autostart.is_some();
            ui.horizontal(|ui| {
                if theme::switch(ui, on).clicked() {
                    jobs.push(Job::SetAutostart(!on));
                }
                ui.label(if on { "Start the daemon when I sign in" } else { "Off" });
            });
            if let Some(v) = &app.data.autostart {
                let note = if autostart::matches_exe(v, &exe) { String::new() } else { "  (points to a different exe)".into() };
                ui.label(RichText::new(format!("{v}{note}")).monospace().small().color(pal.muted));
            }
            ui.label(RichText::new("Per-user Run key, no admin rights.").small().color(pal.muted));
        });

        card(ui, "Updates", |ui| {
            let update = app.data.update.clone();
            egui::Grid::new("update_info").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).color(pal.muted));
                    ui.label(v);
                    ui.end_row();
                };
                row(ui, "This window", WINDOW_VERSION.to_string());
                if let Some(s) = &status {
                    row(ui, "Daemon", s.version.clone());
                }
                if let Some(u) = &update {
                    row(
                        ui,
                        "Latest release",
                        u.latest.clone().unwrap_or_else(|| "not checked yet".into()),
                    );
                    row(ui, "Last check", u.last_check.clone().unwrap_or_else(|| "never".into()));
                }
            });
            if let Some(e) = update.as_ref().and_then(|u| u.last_error.as_ref()) {
                ui.label(RichText::new(format!("Last check failed: {e}")).small().color(pal.err));
            }
            let enabled = online && update.is_some();
            let auto = status.as_ref().is_some_and(|s| s.auto_update);
            let busy = update.as_ref().is_some_and(|u| u.phase != "idle");
            ui.horizontal(|ui| {
                if ui.add_enabled(enabled, egui::Button::new("Check now")).clicked() {
                    jobs.push(Job::CheckUpdate);
                }
                if let Some(u) = update.as_ref().filter(|u| u.available)
                    && let Some(v) = &u.latest
                    && ui
                        .add_enabled(enabled && !busy, egui::Button::new(format!("Install {v}")))
                        .clicked()
                {
                    jobs.push(Job::InstallUpdate);
                }
                if busy {
                    ui.spinner();
                }
            });
            ui.horizontal(|ui| {
                if enabled && theme::switch(ui, auto).clicked() {
                    jobs.push(Job::SetAutoUpdate(!auto));
                }
                ui.label(if auto { "Install updates automatically" } else { "Ask first (tray and this page show the update)" });
            });
            if online && update.is_none() {
                ui.label(
                    RichText::new("Updates are off in this daemon (a development build, or MCPM_NO_UPDATE is set).")
                        .small()
                        .color(pal.muted),
                );
            } else {
                ui.label(
                    RichText::new("Checks every 6 hours. An automatic install waits until no tool call is running.")
                        .small()
                        .color(pal.muted),
                );
            }
        });

        card(ui, "Uninstall", |ui| {
            ui.label("Restores every harness config from its snapshot, turns autostart off and stops the daemon. Snapshots and config.json stay on disk.");
            if theme::danger_button(ui, pal, "Uninstall everything…").clicked() {
                app.settings.confirm_uninstall = true;
            }
        });
    });

    if app.settings.confirm_uninstall {
        let mut done = false;
        modal(ui.ctx(), "Uninstall everything?", |ui| {
            ui.set_max_width(440.0);
            ui.label("1. Restore every taken-over harness (imported servers are removed from the manager).");
            ui.label("2. Remove the autostart entry.");
            ui.label("3. Stop the daemon, then close this window.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::danger_button(ui, pal, "Uninstall").clicked() {
                    jobs.push(Job::Uninstall);
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
        });
        if done {
            app.settings.confirm_uninstall = false;
        }
    }

    if open_dir && let Err(e) = open_folder(&app.state_dir) {
        errors.push(format!("cannot open the folder: {e}"));
    }
    for e in errors {
        app.error(e);
    }
    for j in jobs {
        app.send(j);
    }
}

fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(15.0));
        ui.add_space(2.0);
        add(ui);
    });
    ui.add_space(4.0);
}

fn open_folder(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(not(windows))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(dir)
        .spawn()
        .map(|_| ())
}
