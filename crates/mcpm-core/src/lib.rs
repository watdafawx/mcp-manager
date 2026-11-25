//! Core: model, manager config store, harness registry, config editing, discovery,
//! snapshots, takeover and restore. No tokio, no network. See SPEC.md.

pub mod model;
/// Temporary stub from feat/daemon; replaced by the real store on merge.
pub mod store_stub;
