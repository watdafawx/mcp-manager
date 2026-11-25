//! The window: left nav, status bar, toasts, and the four pages.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, RichText, Ui};

use crate::model::format_uptime;
use crate::pages::{harnesses, servers, settings, snapshots};
use crate::state::{AppData, ConnState, Level, Msg, Page};
use crate::theme::{self, Palette};
use crate::worker::{self, Job, Shared, WorkerCfg};

pub struct App {
    pub data: AppData,
    rx: Receiver<Msg>,
    tx: Sender<Job>,
    shared: Arc<Shared>,
    pub state_dir: PathBuf,
    pub page: Page,
    pub servers: servers::State,
    pub harnesses: harnesses::State,
    pub snapshots: snapshots::State,
    pub settings: settings::State,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, home: PathBuf, exe: PathBuf) -> Self {
        theme::apply(&cc.egui_ctx);
        let (out, rx) = channel();
        let shared = Shared::new();
        let ctx = cc.egui_ctx.clone();
        let tx = worker::spawn(WorkerCfg {
            home: home.clone(),
            exe,
            shared: shared.clone(),
            out,
            repaint: Arc::new(move || ctx.request_repaint()),
        });
        let _ = tx.send(Job::Start);
        Self {
            data: AppData::default(),
            rx,
            tx,
            shared,
            state_dir: home,
            page: Page::default(),
            servers: servers::State::default(),
            harnesses: harnesses::State::default(),
            snapshots: snapshots::State::default(),
            settings: settings::State::default(),
        }
    }

    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.data.toast(Level::Error, text);
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            self.data.apply(m);
        }
    }

    fn publish_context(&self) {
        if let Ok(mut c) = self.shared.ctx.lock() {
            c.page = self.page;
            c.selected = (self.page == Page::Servers)
                .then(|| self.servers.selected.clone())
                .flatten();
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pause polling while minimized.
        let minimized = ctx.input(|i| i.viewport().minimized == Some(true));
        self.shared
            .visible
            .store(!minimized, std::sync::atomic::Ordering::Relaxed);
        self.drain();
        if self.data.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.drain();
        self.data.prune_toasts(Instant::now());
        self.publish_context();
        let pal = Palette::of(ui);

        egui::Panel::left("nav")
            .exact_size(168.0)
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(10.0);
                ui.label(RichText::new("MCP Manager").heading());
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    for p in Page::ALL {
                        let label = match p {
                            Page::Servers => format!("Servers  {}", self.data.servers.len()),
                            Page::Harnesses => {
                                let n = self.data.harnesses.iter().filter(|h| h.detected).count();
                                format!("Harnesses  {n}")
                            }
                            Page::Snapshots => format!("Snapshots  {}", self.data.snapshots.len()),
                            Page::Settings => "Settings".to_string(),
                        };
                        let b = egui::Button::selectable(self.page == p, label)
                            .frame_when_inactive(false)
                            .min_size(egui::vec2(0.0, 30.0));
                        if ui.add(b).clicked() {
                            self.page = p;
                        }
                    }
                });
            });

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, &pal));

        egui::CentralPanel::default().show(ui, |ui| {
            if let ConnState::Offline(msg) = self.data.conn.clone() {
                self.offline_banner(ui, &pal, &msg);
            }
            match self.page {
                Page::Servers => servers::show(self, ui, &pal),
                Page::Harnesses => harnesses::show(self, ui, &pal),
                Page::Snapshots => snapshots::show(self, ui, &pal),
                Page::Settings => settings::show(self, ui, &pal),
            }
        });

        self.show_toasts(ui.ctx(), &pal);
        if !self.data.toasts.is_empty() {
            // Info toasts expire on their own: wake up to prune them.
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }
}

impl App {
    fn status_bar(&mut self, ui: &mut Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            let (color, text) = match (&self.data.conn, &self.data.status) {
                (ConnState::Online, Some(s)) => (
                    pal.ok,
                    format!(
                        "daemon {} · port {} · pid {} · up {}",
                        s.version,
                        s.port,
                        s.pid,
                        format_uptime(s.uptime_secs)
                    ),
                ),
                (ConnState::Starting, _) => (pal.warn, "connecting to the daemon…".to_string()),
                (ConnState::Online, None) => (pal.warn, "connecting to the daemon…".to_string()),
                (ConnState::Offline(_), _) => (pal.err, "daemon not running".to_string()),
            };
            theme::dot(ui, color);
            ui.label(RichText::new(text).small());
            theme::right(ui, |ui| {
                if self.data.busy > 0 {
                    ui.spinner();
                }
                if let Some(s) = &self.data.status {
                    ui.label(
                        RichText::new(format!(
                            "{} servers · {} running · {} sessions",
                            s.enabled_servers, s.running_instances, s.sessions
                        ))
                        .small()
                        .color(pal.muted),
                    );
                }
            });
        });
    }

    fn offline_banner(&mut self, ui: &mut Ui, pal: &Palette, msg: &str) {
        egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.0, pal.err))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Daemon not running.").strong().color(pal.err));
                    ui.label(RichText::new(msg).small().color(pal.muted));
                    if ui.button("Start daemon").clicked() {
                        self.send(Job::Start);
                    }
                });
            });
        ui.add_space(6.0);
    }

    fn show_toasts(&mut self, ctx: &egui::Context, pal: &Palette) {
        if self.data.toasts.is_empty() {
            return;
        }
        let mut dismiss = None;
        egui::Area::new(egui::Id::NULL.with("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, [-14.0, -40.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for t in &self.data.toasts {
                    let (accent, tag) = match t.level {
                        Level::Error => (pal.err, "error"),
                        Level::Info => (pal.ok, "ok"),
                    };
                    egui::Frame::popup(ui.style())
                        .stroke(egui::Stroke::new(1.5, accent))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(tag).small().strong().color(accent));
                                ui.label(&t.text);
                                if ui.small_button("×").clicked() {
                                    dismiss = Some(t.id);
                                }
                            });
                        });
                }
            });
        if let Some(id) = dismiss {
            self.data.dismiss_toast(id);
        }
    }
}

/// Shared look for a modal confirm window.
pub fn modal(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, add);
}

pub fn path_label(ui: &mut Ui, pal: &Palette, path: &str) {
    ui.label(RichText::new(path).monospace().small().color(pal.muted))
        .on_hover_text(path);
}

pub fn colored(text: &str, color: Color32) -> RichText {
    RichText::new(text).color(color)
}
