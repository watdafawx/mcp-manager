//! Snapshots page: list, diff of the stored bytes against the current file, restore, delete.

use eframe::egui::{self, RichText, Ui};

use crate::app::{App, modal, path_label};
use crate::diff::{self, Row, Tag};
use crate::model::{SnapRow, format_time};
use crate::theme::{self, Palette};
use crate::worker::{Job, RestoreTarget};

#[derive(Default)]
pub struct State {
    selected: Option<String>,
    view: Option<DiffView>,
    confirm_delete: Option<String>,
    /// Restore dialog: (snapshot id, keep in manager).
    restore: Option<(String, bool)>,
}

struct DiffView {
    id: String,
    file: usize,
    summary: String,
    rows: Vec<Row>,
}

/// Reads the stored bytes (checksum verified by core) and the current file, then diffs them.
fn build_view(state_dir: &std::path::Path, snap: &SnapRow, file: usize) -> Result<DiffView, String> {
    let s = mcpm_core::snapshot::load(state_dir, &snap.id).map_err(|e| format!("{e:#}"))?;
    let stored = mcpm_core::snapshot::bytes(state_dir, &s, file).map_err(|e| format!("{e:#}"))?;
    let path = &snap.files[file].path;
    let current = std::fs::read(path).ok();
    let stored_text = String::from_utf8_lossy(&stored);
    let current_text = current.as_deref().map(String::from_utf8_lossy).unwrap_or_default();
    let lines = diff::diff_lines(&stored_text, &current_text);
    let summary = match (snap.files[file].existed, current.is_some()) {
        (false, true) => "the file did not exist before; it exists now".to_string(),
        (true, false) => "the file existed before; it is gone now".to_string(),
        _ if diff::is_identical(&lines) => "stored copy and current file are identical".to_string(),
        _ => {
            let add = lines.iter().filter(|l| l.tag == Tag::Add).count();
            let del = lines.iter().filter(|l| l.tag == Tag::Del).count();
            format!("{del} line{} only in the snapshot, {add} only in the current file", if del == 1 { "" } else { "s" })
        }
    };
    Ok(DiffView { id: snap.id.clone(), file, summary, rows: diff::fold_context(&lines, 3) })
}

pub fn show(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let mut jobs: Vec<Job> = vec![];
    ui.horizontal(|ui| {
        ui.heading("Snapshots");
        theme::right(ui, |ui| {
            if ui.button("Refresh").clicked() {
                jobs.push(Job::Refresh);
            }
        });
    });
    ui.label(
        RichText::new("Every write to a harness config keeps the original bytes here. Snapshots stay until you delete them.")
            .small()
            .color(pal.muted),
    );
    ui.add_space(6.0);

    let snaps = app.data.snapshots.clone();
    if snaps.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.label(RichText::new("No snapshots yet").heading()));
    }
    let mut view_req: Option<(SnapRow, usize)> = None;
    let state_dir = app.state_dir.clone();
    let st = &mut app.snapshots;
    let mut errors: Vec<String> = vec![];

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for s in &snaps {
            let open = st.selected.as_deref() == Some(s.id.as_str());
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let head = format!("{}   {}", format_time(s.timestamp), s.operation);
                    if ui.selectable_label(open, RichText::new(head).strong()).clicked() {
                        st.selected = if open { None } else { Some(s.id.clone()) };
                        st.view = None;
                    }
                    ui.label(RichText::new(s.harnesses().join(", ")).small().color(pal.muted));
                    theme::right(ui, |ui| {
                        ui.label(
                            RichText::new(format!("{} file{}", s.files.len(), if s.files.len() == 1 { "" } else { "s" }))
                                .small()
                                .color(pal.muted),
                        );
                    });
                });
                if !open {
                    return;
                }
                ui.label(RichText::new(&s.id).monospace().small().color(pal.muted));
                for (i, f) in s.files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        path_label(ui, pal, &f.path.display().to_string());
                        if !f.existed {
                            ui.label(RichText::new("(did not exist)").small().color(pal.muted));
                        }
                        if ui.small_button("Compare with current").clicked() {
                            view_req = Some((s.clone(), i));
                        }
                    });
                }
                if let Some(v) = st.view.as_ref().filter(|v| v.id == s.id) {
                    diff_ui(ui, pal, v);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Restore…").clicked() {
                        st.restore = Some((s.id.clone(), false));
                    }
                    if st.confirm_delete.as_deref() == Some(s.id.as_str()) {
                        if theme::danger_button(ui, pal, "Really delete").clicked() {
                            jobs.push(Job::DeleteSnapshot(s.id.clone()));
                            st.confirm_delete = None;
                            st.selected = None;
                            st.view = None;
                        }
                        if ui.button("Keep").clicked() {
                            st.confirm_delete = None;
                        }
                    } else if ui.button("Delete…").clicked() {
                        st.confirm_delete = Some(s.id.clone());
                    }
                });
            });
        }
    });

    if let Some((snap, file)) = view_req {
        match build_view(&state_dir, &snap, file) {
            Ok(v) => app.snapshots.view = Some(v),
            Err(e) => errors.push(format!("cannot read the snapshot: {e}")),
        }
    }
    for e in errors {
        app.error(e);
    }
    restore_window(app, ui.ctx(), pal);
    for j in jobs {
        app.send(j);
    }
}

fn diff_ui(ui: &mut Ui, pal: &Palette, v: &DiffView) {
    ui.add_space(4.0);
    ui.label(RichText::new(format!("{} — {}", v.summary, "stored (-) vs current (+)")).small());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::Frame::canvas(ui.style()).show(ui, |ui| {
        egui::ScrollArea::both()
            .id_salt(("snap_diff", &v.id, v.file))
            .max_height(300.0)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, v.rows.len(), |ui, range| {
                for r in &v.rows[range] {
                    match r {
                        Row::Fold(n) => {
                            ui.label(RichText::new(format!("   … {n} unchanged lines")).monospace().color(pal.muted));
                        }
                        Row::Line(l) => {
                            let (prefix, color, bg) = match l.tag {
                                Tag::Same => (" ", ui.visuals().text_color(), egui::Color32::TRANSPARENT),
                                Tag::Add => ("+", pal.add, pal.add_bg),
                                Tag::Del => ("-", pal.del, pal.del_bg),
                            };
                            ui.label(
                                RichText::new(format!("{prefix} {}", l.text)).monospace().color(color).background_color(bg),
                            );
                        }
                    }
                }
            });
    });
}

fn restore_window(app: &mut App, ctx: &egui::Context, pal: &Palette) {
    let Some((id, mut keep)) = app.snapshots.restore.clone() else { return };
    let mut done = false;
    let mut go = false;
    modal(ctx, "Restore snapshot", |ui| {
        ui.set_max_width(440.0);
        ui.label(format!("Put the files of snapshot {id} back."));
        ui.label(
            RichText::new("If a harness changed the file since, only the `mcpm` entry is removed and the original entries are re-added.")
                .small()
                .color(pal.muted),
        );
        ui.checkbox(&mut keep, "Keep servers in manager");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if theme::danger_button(ui, pal, "Restore").clicked() {
                go = true;
                done = true;
            }
            if ui.button("Cancel").clicked() {
                done = true;
            }
        });
    });
    if go {
        app.send(Job::Restore { target: RestoreTarget::Snapshot(id.clone()), keep });
    }
    app.snapshots.restore = if done { None } else { Some((id, keep)) };
}
