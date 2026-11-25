//! Look: follows the system light/dark preference (egui default), a muted palette on top.

use eframe::egui::{
    self, Align, Color32, CursorIcon, Layout, Pos2, Response, RichText, Sense, Stroke, StrokeKind,
    Ui, vec2,
};

use crate::model::RunState;

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub idle: Color32,
    pub muted: Color32,
    pub danger_fill: Color32,
    pub add: Color32,
    pub del: Color32,
    pub add_bg: Color32,
    pub del_bg: Color32,
}

impl Palette {
    pub fn of(ui: &Ui) -> Self {
        Self::for_mode(ui.visuals().dark_mode)
    }

    pub fn for_mode(dark: bool) -> Self {
        let c = Color32::from_rgb;
        if dark {
            Self {
                ok: c(116, 184, 132),
                warn: c(214, 174, 98),
                err: c(224, 116, 108),
                idle: c(150, 156, 166),
                muted: c(142, 148, 158),
                danger_fill: c(120, 52, 50),
                add: c(138, 200, 150),
                del: c(228, 130, 122),
                add_bg: Color32::from_rgba_unmultiplied(80, 150, 100, 40),
                del_bg: Color32::from_rgba_unmultiplied(200, 90, 84, 40),
            }
        } else {
            Self {
                ok: c(44, 128, 74),
                warn: c(158, 108, 20),
                err: c(176, 56, 50),
                idle: c(120, 126, 136),
                muted: c(104, 110, 120),
                danger_fill: c(196, 84, 78),
                add: c(36, 110, 62),
                del: c(168, 52, 46),
                add_bg: Color32::from_rgba_unmultiplied(60, 160, 90, 38),
                del_bg: Color32::from_rgba_unmultiplied(210, 80, 74, 36),
            }
        }
    }

    pub fn run_state(&self, s: RunState) -> Color32 {
        match s {
            RunState::Running => self.ok,
            RunState::Error => self.err,
            RunState::Idle => self.warn,
            RunState::Disabled | RunState::Stopped => self.idle,
        }
    }
}

/// One-time style tweaks (spacing and rounding only; colours come from the system theme).
pub fn apply(ctx: &egui::Context) {
    // Default follows the system. `MCPM_THEME=light|dark` forces one (screenshots, odd setups).
    match std::env::var("MCPM_THEME").ok().as_deref().map(str::trim) {
        Some("light") => ctx.set_theme(egui::ThemePreference::Light),
        Some("dark") => ctx.set_theme(egui::ThemePreference::Dark),
        _ => {}
    }
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
        s.spacing.scroll.floating = true;
        // egui's defaults are small on a 4K/200% screen: one step up everywhere.
        use egui::{FontId, TextStyle};
        for (style, size) in [
            (TextStyle::Small, 12.0),
            (TextStyle::Body, 14.5),
            (TextStyle::Button, 14.5),
            (TextStyle::Monospace, 13.5),
            (TextStyle::Heading, 21.0),
        ] {
            if let Some(f) = s.text_styles.get_mut(&style) {
                *f = FontId::new(size, f.family.clone());
            }
        }
    });
}

/// Filled status dot.
pub fn dot(ui: &mut Ui, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
    resp
}

/// On/off switch. Returns the response; `clicked()` means "flip it".
pub fn switch(ui: &mut Ui, on: bool) -> Response {
    let size = vec2(34.0, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, on);
        // White knob on a track that stays readable in both themes.
        let (off, onc) = if ui.visuals().dark_mode {
            (
                ui.visuals().widgets.inactive.bg_fill,
                ui.visuals().selection.bg_fill,
            )
        } else {
            (Color32::from_gray(185), Color32::from_rgb(0, 118, 168))
        };
        let fill = off.lerp_to_gamma(onc, t);
        let r = rect.height() / 2.0;
        ui.painter()
            .rect(rect, r, fill, Stroke::NONE, StrokeKind::Inside);
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        ui.painter()
            .circle_filled(Pos2::new(x, rect.center().y), r - 2.5, Color32::WHITE);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Clickable pill: lit = exposed / on.
pub fn badge(ui: &mut Ui, text: &str, on: bool) -> Response {
    let rich = RichText::new(text).small();
    let rich = if on { rich.strong() } else { rich.weak() };
    ui.add(
        egui::Button::new(rich)
            .selected(on)
            .corner_radius(10.0)
            .min_size(vec2(0.0, 20.0)),
    )
    .on_hover_cursor(CursorIcon::PointingHand)
}

/// A muted section title.
pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).strong());
}

/// Button painted in the danger colour.
pub fn danger_button(ui: &mut Ui, pal: &Palette, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(pal.danger_fill))
}

/// Right-aligned strip inside a horizontal row.
pub fn right<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Center), add)
        .inner
}
