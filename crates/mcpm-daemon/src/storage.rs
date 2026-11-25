//! Adapter over the core store plus daemon-owned files (token, daemon.json, logs).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use mcpm_core::store::{load as load_config, save as save_config};

/// Filesystem context for core operations, from the user's profile (`MCPM_HOME` honoured).
pub fn core_context() -> mcpm_core::Core {
    mcpm_core::Core::from_env()
}

/// State dir: `MCPM_HOME`, else `%APPDATA%\mcpm` (see `mcpm_core::paths::state_dir`).
pub fn state_dir() -> PathBuf {
    core_context().state_dir
}

pub fn token_path(home: &Path) -> PathBuf {
    home.join("token")
}

pub fn daemon_json_path(home: &Path) -> PathBuf {
    home.join("daemon.json")
}

pub fn log_dir(home: &Path) -> PathBuf {
    home.join("logs")
}

/// Reads the control token, creating a random 32-byte hex one on first run.
pub fn load_or_create_token(home: &Path) -> Result<String> {
    mcpm_core::paths::token(home).with_context(|| format!("token in {}", home.display()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: String,
}

pub fn read_daemon_info(home: &Path) -> Option<DaemonInfo> {
    let bytes = std::fs::read(daemon_json_path(home)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_daemon_info(home: &Path, info: &DaemonInfo) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = daemon_json_path(home);
    let tmp = home.join("daemon.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Removes daemon.json only if it still describes `pid`.
pub fn remove_daemon_info(home: &Path, pid: u32) {
    if read_daemon_info(home).is_some_and(|i| i.pid == pid) {
        let _ = std::fs::remove_file(daemon_json_path(home));
    }
}
