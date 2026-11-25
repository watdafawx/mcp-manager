//! MCP Manager daemon: child pool (stdio + Streamable HTTP clients), the MCP endpoint harnesses
//! talk to, the `/api/*` control API and the stdio bridge. See SPEC.md "Daemon".
//!
//! Protocol note: the JSON-RPC layer is hand-rolled on `serde_json::Value` instead of `rmcp`.
//! A proxy has to pass child results through untouched (typed models drop unknown fields), the
//! per-session child isolation needs the `Mcp-Session-Id` that rmcp 3.x removes for new protocol
//! versions, and this keeps the idle footprint tiny.

pub mod api;
pub mod bridge;
pub mod client;
pub mod coreops;
pub mod daemon;
pub mod http_client;
pub mod jobobject;
pub mod logging;
pub mod mask;
pub mod mcp;
pub mod pool;
pub mod rpc;
pub mod server;
pub mod storage;
pub mod timeutil;

pub use coreops::{CoreOps, OpError, StubCoreOps};
pub use daemon::{DaemonOptions, RunningDaemon, Started, run_blocking};
pub use pool::{Pool, PoolOptions};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
