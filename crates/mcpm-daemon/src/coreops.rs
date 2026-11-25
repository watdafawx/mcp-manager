//! Operations owned by `mcpm-core` (harness registry, takeover, restore, snapshots). The daemon
//! exposes them over the control API so there is a single writer. They are defined against this
//! trait so the daemon builds before core lands; wire the real impl after merge.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}

pub type OpResult = Result<Value, OpError>;

/// Blocking file operations; the daemon runs them on the blocking pool. After `apply_takeover`
/// and `restore` the daemon reloads `config.json` and notifies connected sessions.
pub trait CoreOps: Send + Sync + 'static {
    /// Detected harnesses with state (not connected / connected alongside / taken over).
    fn harnesses(&self) -> OpResult;
    /// Dry run for one harness: entries to import/remove and the final file text.
    fn plan_takeover(&self, harness: &str) -> OpResult;
    /// `target` is a harness id or `"all"`.
    fn apply_takeover(&self, target: &str) -> OpResult;
    fn snapshots(&self) -> OpResult;
    fn delete_snapshot(&self, id: &str) -> OpResult;
    /// `req` is one of `{"harness": id}`, `{"all": true}`, `{"snapshot": id}`, optional `"keepInManager": bool`.
    fn restore(&self, req: &Value) -> OpResult;
}

/// Placeholder until `mcpm-core` provides the real implementation.
pub struct StubCoreOps;

impl CoreOps for StubCoreOps {
    fn harnesses(&self) -> OpResult {
        Err(OpError::NotImplemented("harnesses"))
    }
    fn plan_takeover(&self, _harness: &str) -> OpResult {
        Err(OpError::NotImplemented("plan_takeover"))
    }
    fn apply_takeover(&self, _target: &str) -> OpResult {
        Err(OpError::NotImplemented("apply_takeover"))
    }
    fn snapshots(&self) -> OpResult {
        Err(OpError::NotImplemented("snapshots"))
    }
    fn delete_snapshot(&self, _id: &str) -> OpResult {
        Err(OpError::NotImplemented("delete_snapshot"))
    }
    fn restore(&self, _req: &Value) -> OpResult {
        Err(OpError::NotImplemented("restore"))
    }
}
