//! Core: model, manager config store, harness registry, config editing, discovery,
//! snapshots, takeover and restore. No tokio, no network. See SPEC.md.

pub mod discovery;
pub mod edit;
pub mod harness;
pub mod model;
pub mod paths;
pub mod restore;
pub mod snapshot;
pub mod store;
pub mod takeover;

/// Explicit filesystem context; safe to construct with temporary directories.
#[derive(Debug, Clone)]
pub struct Core {
    pub home: std::path::PathBuf,
    pub appdata: std::path::PathBuf,
    pub state_dir: std::path::PathBuf,
}

impl Core {
    /// Context for the current user profile (`USERPROFILE`/`HOME`, `APPDATA`); `MCPM_HOME` moves
    /// the state dir. Shared by the daemon, GUI and CLI.
    pub fn from_env() -> Self {
        use std::path::PathBuf;
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"));
        let state_dir = paths::state_dir(&home, &appdata);
        Self {
            home,
            appdata,
            state_dir,
        }
    }
}
