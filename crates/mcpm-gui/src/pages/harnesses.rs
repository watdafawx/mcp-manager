//! Harnesses page: one card per harness, take over with a dry-run review, restore.

use eframe::egui::{self, RichText, Ui};

use crate::app::App;
use crate::app::{modal, path_label};
use crate::model::{HarnessRow, HarnessState, Plan};
use crate::state::PlanView;
use crate::theme::{self, Palette};
use crate::worker::{Job, RestoreTarget};

#[derive(Default)]
pub struct State {
    /// Waiting for a dry run of this target.
    planning: Option<String>,
    /// Which plan of a multi-harness dry run is shown.
    plan_tab: usize,
    /// Restore dialog: (target, keep servers in manager).
    restore: Option<(RestoreTarget, bool)>,
    show_undetected: bool,
}

/// Plans shown in the modal are capped so a multi-MB `~/.claude.json` stays responsive.
const MAX_TEXT_LINES: usize = 4000;

pub fn show(app: &mut App, ui: &mut Ui, pal: &Palette) {
    let mut jobs: Vec<Job> = vec![];
    let mut plan_for: Option<String> = None;
    let mut restore_all = false;
    ui.horizontal(|ui| {
        ui.heading("Harnesses");
        theme::right(ui, |ui| {
            let any_taken = app
                .data
                .harnesses
                .iter()
                .any(|h| h.state == HarnessState::TakenOver);
            if ui
                .add_enabled(any_taken, egui::Button::new("Restore all"))
                .clicked()
            {
                restore_all = true;
            }
            let any_free = app
                .data
                .harnesses
                .iter()
                .any(|h| h.detected && h.state != HarnessState::TakenOver);
            if ui
                .add_enabled(any_free, egui::Button::new("Take over all"))
                .clicked()
            {
                plan_for = Some("all".into());
            }
            if ui.button("Refresh").clicked() {
                jobs.push(Job::Refresh);
            }
        });
    });
    ui.label(
        RichText::new(
            "Taking over a harness moves its MCP servers into the manager and leaves a single `mcpm` entry. \
             Every change is snapshotted first and can be restored.",
        )
        .small()
        .color(pal.muted),
    );
    ui.add_space(6.0);

    let harnesses = app.data.harnesses.clone();
    let (detected, other): (Vec<&HarnessRow>, Vec<&HarnessRow>) =
        harnesses.iter().partition(|h| h.detected);
    let mut restore_for: Option<String> = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if harnesses.is_empty() {
                ui.label(RichText::new("No harness information yet.").color(pal.muted));
            }
            card_grid(
                ui,
                "detected",
                pal,
                &detected,
                &mut plan_for,
                &mut restore_for,
            );
            if !other.is_empty() {
                ui.add_space(6.0);
                let label = format!("{} not detected on this PC", other.len());
                egui::CollapsingHeader::new(label)
                    .id_salt("undetected")
                    .default_open(app.harnesses.show_undetected)
                    .show(ui, |ui| {
                        card_grid(ui, "other", pal, &other, &mut plan_for, &mut restore_for)
                    });
            }
        });

    if let Some(t) = plan_for {
        app.data.plan = None;
        app.harnesses.planning = Some(t.clone());
        app.harnesses.plan_tab = 0;
        jobs.push(Job::PlanTakeover(t));
    }
    if let Some(h) = restore_for {
        app.harnesses.restore = Some((RestoreTarget::Harness(h), false));
    }
    if restore_all {
        app.harnesses.restore = Some((RestoreTarget::All, false));
    }
    for j in jobs {
        app.send(j);
    }

    plan_window(app, ui.ctx(), pal);
    restore_window(app, ui.ctx(), pal);
}

/// Equal-width cards in as many columns as fit (a plain wrapped layout lets cards grow without bound).
fn card_grid(
    ui: &mut Ui,
    id: &str,
    pal: &Palette,
    list: &[&HarnessRow],
    plan_for: &mut Option<String>,
    restore_for: &mut Option<String>,
) {
    const MIN_W: f32 = 430.0;
    const GAP: f32 = 10.0;
    let avail = ui.available_width().max(MIN_W);
    let cols = (((avail + GAP) / (MIN_W + GAP)).floor() as usize).max(1);
    let width = (avail - GAP * (cols as f32 - 1.0)) / cols as f32;
    egui::Grid::new(id)
        .num_columns(cols)
        .spacing([GAP, GAP])
        .show(ui, |ui| {
            for (i, h) in list.iter().enumerate() {
                card(ui, pal, h, width, plan_for, restore_for);
                if (i + 1) % cols == 0 {
                    ui.end_row();
                }
            }
        });
}

fn card(
    ui: &mut Ui,
    pal: &Palette,
    h: &HarnessRow,
    width: f32,
    plan_for: &mut Option<String>,
    restore_for: &mut Option<String>,
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        // Grid cells lay out left-to-right; the card stacks its rows.
        ui.set_width(width - 20.0);
        ui.set_min_height(110.0);
        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
            card_body(ui, pal, h, plan_for, restore_for)
        });
    });
}

fn card_body(
    ui: &mut Ui,
    pal: &Palette,
    h: &HarnessRow,
    plan_for: &mut Option<String>,
    restore_for: &mut Option<String>,
) {
    {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&h.label).strong().size(15.0));
            ui.label(RichText::new(&h.id).small().color(pal.muted));
            theme::right(ui, |ui| {
                let (color, text) = match (h.detected, h.state) {
                    (false, _) => (pal.idle, "not detected"),
                    (_, HarnessState::TakenOver) => (pal.ok, "taken over"),
                    (_, HarnessState::Connected) => (pal.warn, "connected alongside"),
                    (_, HarnessState::NotConnected) => (pal.idle, "not connected"),
                };
                ui.label(RichText::new(text).small().color(color));
                theme::dot(ui, color);
            });
        });
        path_label(ui, pal, h.main_path());
        let foreign = h.foreign_entries();
        let entries = if foreign.is_empty() {
            "no MCP entries found".to_string()
        } else {
            let shown: Vec<&str> = foreign.iter().copied().take(6).collect();
            let more = foreign.len().saturating_sub(shown.len());
            let tail = if more > 0 {
                format!(" +{more} more")
            } else {
                String::new()
            };
            format!(
                "{} entr{} found: {}{tail}",
                foreign.len(),
                if foreign.len() == 1 { "y" } else { "ies" },
                shown.join(", ")
            )
        };
        ui.label(RichText::new(entries).small());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can_take = h.detected && h.state != HarnessState::TakenOver;
            if ui
                .add_enabled(can_take, egui::Button::new("Take over…"))
                .clicked()
            {
                *plan_for = Some(h.id.clone());
            }
            if ui
                .add_enabled(
                    h.state == HarnessState::TakenOver,
                    egui::Button::new("Restore…"),
                )
                .clicked()
            {
                *restore_for = Some(h.id.clone());
            }
        });
    }
}

fn plan_window(app: &mut App, ctx: &egui::Context, pal: &Palette) {
    let waiting = app.harnesses.planning.clone();
    let Some(target) = waiting else { return };
    let Some(view) = app.data.plan.clone().filter(|v| v.target == target) else {
        modal(ctx, "Take over", |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("planning (nothing is written yet)…");
            });
            if ui.button("Cancel").clicked() {
                app.harnesses.planning = None;
            }
        });
        return;
    };
    let mut apply = false;
    let mut cancel = false;
    let title = if view.target == "all" {
        "Take over all harnesses".to_string()
    } else {
        format!("Take over {}", view.target)
    };
    let tab = &mut app.harnesses.plan_tab;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(true)
        .default_size([720.0, 520.0])
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            plan_body(ui, pal, &view, tab);
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                let nothing = view.plans.is_empty();
                if ui
                    .add_enabled(
                        !nothing,
                        egui::Button::new(RichText::new("Apply takeover").strong()),
                    )
                    .on_hover_text("Snapshots every file first, then writes once per file")
                    .clicked()
                {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
    if apply {
        app.send(Job::ApplyTakeover(view.target.clone()));
    }
    if apply || cancel {
        app.harnesses.planning = None;
        app.data.plan = None;
    }
}

fn plan_body(ui: &mut Ui, pal: &Palette, view: &PlanView, tab: &mut usize) {
    for (id, e) in &view.errors {
        ui.label(RichText::new(format!("{id}: {e}")).color(pal.err));
    }
    if view.plans.is_empty() {
        ui.label("Nothing to take over.");
        return;
    }
    *tab = (*tab).min(view.plans.len() - 1);
    if view.plans.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, p) in view.plans.iter().enumerate() {
                ui.selectable_value(tab, i, format!("{} ({})", p.harness, p.import_count()));
            }
        });
    }
    let plan: &Plan = &view.plans[*tab];
    ui.label(
        RichText::new(format!(
            "{}: import {} server{}, remove {} entr{} from the harness file, add `mcpm`.",
            plan.harness,
            plan.import_count(),
            if plan.import_count() == 1 { "" } else { "s" },
            plan.remove_count(),
            if plan.remove_count() == 1 { "y" } else { "ies" },
        ))
        .strong(),
    );
    for f in &plan.files {
        path_label(ui, pal, &f.path);
        if f.imports.is_empty() {
            ui.label(
                RichText::new("no servers to import from this file")
                    .small()
                    .color(pal.muted),
            );
        } else {
            let names: Vec<String> = f.imports.iter().map(|s| s.name.clone()).collect();
            ui.label(RichText::new(format!("imports: {}", names.join(", "))).small());
        }
        if !f.remove.is_empty() {
            ui.label(
                RichText::new(format!("removes: {}", f.remove.join(", ")))
                    .small()
                    .color(pal.del),
            );
        }
    }
    ui.add_space(4.0);
    ui.label(RichText::new("Resulting file").small().color(pal.muted));
    for (i, f) in plan.files.iter().enumerate() {
        let lines: Vec<&str> = f.final_text.lines().collect();
        let shown = lines.len().min(MAX_TEXT_LINES);
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::Frame::canvas(ui.style()).show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt(("plan_text", i))
                .max_height(260.0)
                .auto_shrink([false, false])
                .show_rows(ui, row_h, shown, |ui, range| {
                    for l in &lines[range] {
                        ui.label(RichText::new(*l).monospace());
                    }
                });
        });
        if lines.len() > shown {
            ui.label(
                RichText::new(format!("… {} more lines not shown", lines.len() - shown))
                    .small()
                    .color(pal.muted),
            );
        }
    }
}

fn restore_window(app: &mut App, ctx: &egui::Context, pal: &Palette) {
    let Some((target, mut keep)) = app.harnesses.restore.clone() else {
        return;
    };
    let title = match &target {
        RestoreTarget::Harness(h) => format!("Restore {h}"),
        RestoreTarget::All => "Restore all harnesses".to_string(),
        RestoreTarget::Snapshot(_) => "Restore snapshot".to_string(),
    };
    let mut done = false;
    let mut go = false;
    modal(ctx, &title, |ui| {
        ui.set_max_width(440.0);
        ui.label("The harness config goes back to what it was before the takeover. If the harness edited the file since, only the `mcpm` entry is removed and the original entries are put back.");
        ui.add_space(4.0);
        ui.checkbox(&mut keep, "Keep servers in manager")
            .on_hover_text("Leave the imported servers in the manager instead of deleting them with the harness link");
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
        app.send(Job::Restore {
            target: target.clone(),
            keep,
        });
    }
    app.harnesses.restore = if done { None } else { Some((target, keep)) };
}
