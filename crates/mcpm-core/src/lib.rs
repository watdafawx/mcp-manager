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
