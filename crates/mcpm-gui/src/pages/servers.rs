//! Servers page: library list with exposure badges, detail/edit pane, add form.

use eframe::egui::{self, RichText, Ui};
use mcpm_core::model::{InstanceMode, Transport};

use crate::app::App;
use crate::form::{Kind, ServerForm};
use crate::model::{RunState, ServerRow, format_time};
use crate::state::{ToolsState, exposed_to, toggle_exposure};
use crate::theme::{self, Palette};
use crate::worker::Job;

#[derive(Default)]
pub struct State {
    pub selected: Option<String>,
    /// Add-server pane is open with this form.
    pub adding: Option<ServerForm>,
    form: Option<ServerForm>,
    pristine: Option<ServerForm>,
    /// (server id, `servers_rev`) the form was last synced from.
    form_for: Option<(String, u64)>,
    reveal: bool,
    reveal_applied: bool,
    confirm_remove: bool,
    tools_filter: String,
}

pub fn show(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let open_detail = app.servers.adding.is_some()
        || app.servers.selected.as_ref().is_some_and(|id| app.data.server(id).is_some());
    if !open_detail && app.servers.selected.is_some() {
        app.servers.selected = None; // the server vanished
    }
    if open_detail {
        egui::Panel::right("server_detail")
            .default_size(430.0)
            .min_size(340.0)
            .resizable(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(6.0);
                    if app.servers.adding.is_some() {
                        add_pane(app, ui);
                    } else {
                        edit_pane(app, ui, pal);
                    }
                });
            });
    }
    list(app, ui, pal);
}

fn transport_summary(t: &Transport) -> String {
    let s = match t {
        Transport::Stdio { command, args, .. } => {
            let exe = command.rsplit(['\\', '/']).next().unwrap_or(command);
            match args.first() {
                Some(a) => format!("stdio · {exe} {a}"),
                None => format!("stdio · {exe}"),
            }
        }
        Transport::Http { url, .. } => format!("http · {}", url.trim_start_matches("https://").trim_start_matches("http://")),
    };
    if s.chars().count() > 56 {
        let cut: String = s.chars().take(55).collect();
        format!("{cut}…")
    } else {
        s
    }
}

fn list(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let mut jobs: Vec<Job> = vec![];
    let mut select: Option<String> = None;
    let mut add = false;
    ui.horizontal(|ui| {
        ui.heading("Servers");
        theme::right(ui, |ui| {
            if ui.button("＋ Add server").clicked() {
                add = true;
            }
            if ui.button("Refresh").clicked() {
                jobs.push(Job::Refresh);
            }
        });
    });
    ui.label(
        RichText::new("Click a harness badge to expose or hide a server there. Lit = visible to that harness.")
            .small()
            .color(pal.muted),
    );
    ui.add_space(4.0);

    let rows = app.data.servers.clone();
    let badges = app.data.badge_harnesses();
    let all_ids: Vec<String> = badges.iter().map(|(id, _)| id.clone()).collect();
    if rows.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No servers yet").heading());
            ui.label(
                RichText::new("Take over a harness on the Harnesses page to import its servers, or add one by hand.")
                    .color(pal.muted),
            );
        });
    }
    let mut toasts: Vec<String> = vec![];
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for row in &rows {
            let selected = app.servers.selected.as_deref() == Some(row.cfg.id.as_str());
            let fill = if selected {
                ui.visuals().selection.bg_fill.gamma_multiply(0.3)
            } else {
                ui.visuals().faint_bg_color
            };
            egui::Frame::group(ui.style()).fill(fill).show(ui, |ui| {
                ui.set_width(ui.available_width());
                server_row(ui, pal, row, selected, &badges, &all_ids, &mut jobs, &mut select, &mut toasts);
            });
        }
    });
    for t in toasts {
        app.error(t);
    }
    if let Some(id) = select {
        app.servers.adding = None;
        app.servers.selected = Some(id);
    }
    if add {
        app.servers.selected = None;
        app.servers.adding = Some(ServerForm::default());
    }
    for j in jobs {
        app.send(j);
    }
}

#[allow(clippy::too_many_arguments)]
fn server_row(
    ui: &mut Ui,
    pal: &Palette,
    row: &ServerRow,
    selected: bool,
    badges: &[(String, String)],
    all_ids: &[String],
    jobs: &mut Vec<Job>,
    select: &mut Option<String>,
    toasts: &mut Vec<String>,
) {
    let cfg = &row.cfg;
    ui.horizontal(|ui| {
        let dot = theme::dot(ui, pal.run_state(row.state));
        match &row.last_error {
            Some(e) if row.state == RunState::Error => dot.on_hover_text(e),
            _ => dot.on_hover_text(row.state.label()),
        };
        if ui.selectable_label(selected, RichText::new(&cfg.name).strong()).clicked() {
            *select = Some(cfg.id.clone());
        }
        ui.label(RichText::new(transport_summary(&cfg.transport)).small().color(pal.muted));
        theme::right(ui, |ui| {
            if theme::switch(ui, cfg.enabled).on_hover_text(if cfg.enabled { "Disable" } else { "Enable" }).clicked() {
                jobs.push(Job::SetEnabled { id: cfg.id.clone(), on: !cfg.enabled });
            }
            if ui.add_enabled(cfg.enabled, egui::Button::new("Restart").small()).clicked() {
                jobs.push(Job::Restart(cfg.id.clone()));
            }
            let tools = match row.state {
                RunState::Running => format!("{} tools", row.tool_count),
                _ if row.tool_count > 0 => format!("{} tools", row.tool_count),
                _ => row.state.label().to_string(),
            };
            ui.label(RichText::new(tools).small().color(pal.muted));
        });
    });
    ui.horizontal_wrapped(|ui| {
        if badges.is_empty() {
            ui.label(RichText::new("no harness detected").small().color(pal.muted));
        }
        for (hid, label) in badges {
            let on = exposed_to(cfg, hid);
            if theme::badge(ui, label, on).clicked() {
                match toggle_exposure(&cfg.harnesses, all_ids, hid) {
                    Some(h) => {
                        let mut c = cfg.clone();
                        c.harnesses = h;
                        jobs.push(Job::Save { editing: true, cfg: Box::new(c) });
                    }
                    None => toasts.push(format!(
                        "{} must stay visible to at least one harness. Disable it with the switch instead.",
                        cfg.name
                    )),
                }
            }
        }
    });
}

// ---------- detail: edit ----------

fn edit_pane(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let Some(id) = app.servers.selected.clone() else { return };
    let Some(row) = app.data.server(&id).cloned() else { return };
    let rev = app.data.servers_rev;
    let st = &mut app.servers;

    // Sync the form with the server: switching servers resets it, new data replaces it unless
    // the user is mid-edit.
    let switched = st.form_for.as_ref().map(|(i, _)| i) != Some(&id);
    let dirty = st.form.is_some() && st.form != st.pristine;
    let stale = st.form_for.as_ref().is_some_and(|(_, r)| *r != rev);
    if switched || (stale && !dirty) || st.form.is_none() {
        let f = ServerForm::from_config(&row.cfg);
        st.form = Some(f.clone());
        st.pristine = Some(f);
        st.reveal_applied = false;
        if switched {
            st.reveal = false;
            st.confirm_remove = false;
            st.tools_filter.clear();
        }
    }
    st.form_for = Some((id.clone(), rev));
    if st.reveal
        && !st.reveal_applied
        && let Some(r) = app.data.revealed.get(&id)
    {
        for f in [st.form.as_mut(), st.pristine.as_mut()].into_iter().flatten() {
            f.load_transport(&r.transport);
        }
        st.reveal_applied = true;
    }

    let mut jobs: Vec<Job> = vec![];
    let mut errors: Vec<String> = vec![];
    let mut loading_tools = false;

    ui.horizontal(|ui| {
        theme::dot(ui, pal.run_state(row.state));
        ui.label(RichText::new(&row.cfg.name).heading());
        theme::right(ui, |ui| {
            if ui.small_button("✕").on_hover_text("Close").clicked() {
                st.selected = None;
            }
        });
    });
    ui.label(
        RichText::new(format!(
            "{} · {} · {} tool{} · {} active session{}",
            row.cfg.id,
            row.state.label(),
            row.tool_count,
            if row.tool_count == 1 { "" } else { "s" },
            row.active_sessions,
            if row.active_sessions == 1 { "" } else { "s" },
        ))
        .small()
        .color(pal.muted),
    );
    if let Some(e) = &row.last_error {
        ui.label(RichText::new(e).small().color(pal.err));
    }
    if let Some(o) = &row.cfg.origin {
        ui.label(
            RichText::new(format!("imported from {} ({})", o.harness, o.entry)).small().color(pal.muted),
        );
    }
    ui.add_space(6.0);

    let Some(form) = st.form.as_mut() else { return };
    form_fields(ui, form, true, st.reveal);

    // secrets
    ui.add_space(4.0);
    let mut reveal = st.reveal;
    let secrets_exist = !form.env.is_empty() || !form.headers.is_empty() || form.args.contains('=');
    if secrets_exist
        && ui
            .checkbox(&mut reveal, "Reveal secrets")
            .on_hover_text("Fetches the real env/header values from the daemon. Masked values (••••) are kept as they are on save.")
            .changed()
    {
        if reveal {
            jobs.push(Job::Reveal(id.clone()));
            st.reveal_applied = false;
        } else {
            for f in [st.form.as_mut(), st.pristine.as_mut()].into_iter().flatten() {
                f.load_transport(&row.cfg.transport);
            }
        }
        st.reveal = reveal;
    }

    // pinned tools
    let Some(form) = st.form.as_mut() else { return };
    theme::section(ui, "Pinned tools");
    ui.label(
        RichText::new("Pinned tools are exposed directly as <server>__<tool>, besides the mcpm_* meta-tools.")
            .small()
            .color(pal.muted),
    );
    let tools = app.data.tools.get(&id).cloned();
    match tools {
        None => {
            if ui.button("Load tools (starts the server)").clicked() {
                loading_tools = true;
            }
            pinned_leftovers(ui, form, &[]);
        }
        Some(ToolsState::Loading) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("starting the server…");
            });
        }
        Some(ToolsState::Failed(e)) => {
            ui.label(RichText::new(e).small().color(pal.err));
            if ui.button("Retry").clicked() {
                loading_tools = true;
            }
        }
        Some(ToolsState::Ready(list)) => {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut st.tools_filter)
                        .hint_text("filter tools")
                        .desired_width(180.0),
                );
                ui.label(RichText::new(format!("{} tools · {} pinned", list.len(), form.pinned.len())).small().color(pal.muted));
                if ui.small_button("Reload").clicked() {
                    loading_tools = true;
                }
            });
            let q = st.tools_filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(220.0).id_salt("tool_picker").show(ui, |ui| {
                for t in list.iter().filter(|t| q.is_empty() || t.name.to_lowercase().contains(&q)) {
                    let mut on = form.pinned.contains(&t.name);
                    let r = ui.checkbox(&mut on, &t.name);
                    let r = if t.description.is_empty() { r } else { r.on_hover_text(&t.description) };
                    if r.changed() {
                        if on {
                            form.pinned.push(t.name.clone());
                        } else {
                            form.pinned.retain(|p| p != &t.name);
                        }
                    }
                }
            });
            pinned_leftovers(ui, form, &list.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
        }
    }
    if loading_tools {
        app.data.tools.insert(id.clone(), ToolsState::Loading);
        jobs.push(Job::LoadTools(id.clone()));
    }

    // actions
    let st = &mut app.servers;
    ui.add_space(8.0);
    ui.separator();
    let dirty = st.form != st.pristine;
    ui.horizontal(|ui| {
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked()
            && let Some(form) = &st.form
        {
            match form.to_config() {
                Ok(mut cfg) => {
                    // Exposure, enable switch and origin are edited elsewhere: keep the live values.
                    cfg.harnesses = row.cfg.harnesses.clone();
                    cfg.enabled = row.cfg.enabled;
                    cfg.origin = row.cfg.origin.clone();
                    jobs.push(Job::Save { editing: true, cfg: Box::new(cfg) });
                    st.pristine = st.form.clone();
                    st.reveal = false;
                    app.data.revealed.remove(&id);
                }
                Err(e) => errors.push(e),
            }
        }
        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
            st.form = st.pristine.clone();
        }
        theme::right(ui, |ui| {
            if st.confirm_remove {
                if theme::danger_button(ui, pal, "Really remove").clicked() {
                    jobs.push(Job::Remove(id.clone()));
                    st.confirm_remove = false;
                    st.selected = None;
                }
                if ui.button("Keep").clicked() {
                    st.confirm_remove = false;
                }
            } else if ui.button("Remove…").clicked() {
                st.confirm_remove = true;
            }
        });
    });

    // recent calls
    theme::section(ui, "Recent calls");
    let calls: Vec<_> = app.data.calls.iter().filter(|c| c.server_id == id).take(10).collect();
    if calls.is_empty() {
        ui.label(RichText::new("none yet").small().color(pal.muted));
    } else {
        egui::Grid::new("recent_calls").num_columns(4).spacing([10.0, 3.0]).show(ui, |ui| {
            for c in calls {
                let when = format_time(c.timestamp);
                ui.label(RichText::new(when.get(11..).unwrap_or(&when)).monospace().small().color(pal.muted));
                ui.label(RichText::new(&c.tool).small());
                let r = ui.label(
                    RichText::new(if c.ok { "ok" } else { "failed" }).small().color(if c.ok { pal.ok } else { pal.err }),
                );
                if let Some(e) = &c.error {
                    r.on_hover_text(e);
                }
                ui.label(RichText::new(format!("{} ms", c.duration_ms)).small().color(pal.muted));
                ui.end_row();
            }
        });
    }

    for e in errors {
        app.error(e);
    }
    for j in jobs {
        app.send(j);
    }
}

/// Pinned names the live list does not contain (server down or tool renamed).
fn pinned_leftovers(ui: &mut Ui, form: &mut ServerForm, live: &[String]) {
    let missing: Vec<String> = form.pinned.iter().filter(|p| !live.contains(p)).cloned().collect();
    for name in missing {
        let mut on = true;
        let note = if live.is_empty() { name.clone() } else { format!("{name}  (not in the live list)") };
        if ui.checkbox(&mut on, note).changed() && !on {
            form.pinned.retain(|p| p != &name);
        }
    }
}

// ---------- detail: add ----------

fn add_pane(app: &mut App, ui: &mut Ui) {
    let mut jobs = vec![];
    let mut errors = vec![];
    let mut close = false;
    let mut added: Option<String> = None;
    if let Some(form) = app.servers.adding.as_mut() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Add server").heading());
            theme::right(ui, |ui| {
                if ui.small_button("✕").on_hover_text("Close").clicked() {
                    close = true;
                }
            });
        });
        ui.add_space(6.0);
        form_fields(ui, form, false, true);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Add").clicked() {
                match form.to_config() {
                    Ok(cfg) => {
                        added = Some(cfg.id.clone());
                        jobs.push(Job::Save { editing: false, cfg: Box::new(cfg) });
                    }
                    Err(e) => errors.push(e),
                }
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    }
    if let Some(id) = added {
        app.servers.adding = None;
        app.servers.selected = Some(id);
    } else if close {
        app.servers.adding = None;
    }
    for e in errors {
        app.error(e);
    }
    for j in jobs {
        app.send(j);
    }
}

// ---------- shared form ----------

fn form_fields(ui: &mut Ui, form: &mut ServerForm, editing: bool, secrets_visible: bool) {
    egui::Grid::new("server_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        ui.label("ID");
        if editing {
            ui.label(RichText::new(&form.id).monospace());
        } else {
            ui.add(egui::TextEdit::singleline(&mut form.id).hint_text("e.g. playwright").desired_width(f32::INFINITY));
        }
        ui.end_row();

        ui.label("Name");
        ui.add(egui::TextEdit::singleline(&mut form.name).hint_text("shown in the list").desired_width(f32::INFINITY));
        ui.end_row();

        ui.label("Transport");
        if editing {
            ui.label(match form.kind {
                Kind::Stdio => "stdio (child process)",
                Kind::Http => "Streamable HTTP",
            });
        } else {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut form.kind, Kind::Stdio, "stdio");
                ui.selectable_value(&mut form.kind, Kind::Http, "HTTP");
            });
        }
        ui.end_row();

        match form.kind {
            Kind::Stdio => {
                ui.label("Command");
                ui.add(egui::TextEdit::singleline(&mut form.command).hint_text("npx").desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Arguments");
                ui.add(
                    egui::TextEdit::multiline(&mut form.args)
                        .hint_text("one per line")
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Working dir");
                ui.add(egui::TextEdit::singleline(&mut form.cwd).hint_text("optional").desired_width(f32::INFINITY));
                ui.end_row();
            }
            Kind::Http => {
                ui.label("URL");
                ui.add(
                    egui::TextEdit::singleline(&mut form.url)
                        .hint_text("https://host/mcp")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            }
        }

        ui.label("Mode");
        egui::ComboBox::from_id_salt("mode")
            .selected_text(match form.mode {
                InstanceMode::Shared => "Shared · one child for everyone",
                InstanceMode::Session => "Session · one child per session",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.mode, InstanceMode::Shared, "Shared · one child for everyone");
                ui.selectable_value(&mut form.mode, InstanceMode::Session, "Session · one child per session");
            });
        ui.end_row();
    });
    ui.add_space(4.0);
    match form.kind {
        Kind::Stdio => kv_table(ui, "Environment", "env", &mut form.env, "NAME", "value", secrets_visible),
        Kind::Http => kv_table(ui, "Headers", "headers", &mut form.headers, "Header", "value", secrets_visible),
    }
}

fn kv_table(ui: &mut Ui, title: &str, salt: &str, rows: &mut Vec<(String, String)>, kh: &str, vh: &str, _reveal: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong());
        if ui.small_button("＋").on_hover_text("Add a row").clicked() {
            rows.push((String::new(), String::new()));
        }
    });
    let mut remove = None;
    for (i, (k, v)) in rows.iter_mut().enumerate() {
        ui.push_id((salt, i), |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(k).hint_text(kh).desired_width(130.0));
                let w = (ui.available_width() - 30.0).max(60.0);
                ui.add(egui::TextEdit::singleline(v).hint_text(vh).desired_width(w));
                if ui.small_button("✕").clicked() {
                    remove = Some(i);
                }
            });
        });
    }
    if let Some(i) = remove {
        rows.remove(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_short() {
        let t = Transport::Stdio {
            command: r"C:\Program Files\nodejs\npx.cmd".into(),
            args: vec!["-y".into()],
            env: Default::default(),
            cwd: None,
        };
        assert_eq!(transport_summary(&t), "stdio · npx.cmd -y");
        let h = Transport::Http { url: format!("https://example.com/{}", "a".repeat(80)), headers: Default::default() };
        assert!(transport_summary(&h).chars().count() <= 56);
    }
}
