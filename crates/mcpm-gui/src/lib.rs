//! eframe/egui window and the control-API client it (and the CLI) use. See SPEC.md.
//!
//! The window never talks to the network itself: a worker thread owns the daemon connection
//! (`client`), runs jobs and polls status, and reports through [`state::Msg`].
pub mod app;
pub mod autostart;
pub mod client;
pub mod diff;
pub mod form;
pub mod model;
pub mod pages;
pub mod state;
pub mod theme;
pub mod worker;

use std::path::PathBuf;

/// Opens the window and blocks until it is closed. Starts the daemon when needed; closing the
/// window leaves the daemon running.
pub fn run(exe: PathBuf) -> anyhow::Result<()> {
    let home = client::state_dir();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MCP Manager")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([820.0, 520.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "MCP Manager",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, home, exe)))),
    )
    .map_err(|e| anyhow::anyhow!("window failed: {e}"))
}
