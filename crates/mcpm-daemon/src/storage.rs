//! Adapter over the core store. When `feat/core` lands, switch the re-export below to the real
//! store and delete `mcpm-core/src/store_stub.rs`. Everything daemon-owned (token, daemon.json)
//! lives here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use mcpm_core::store_stub::{load_config, save_config, state_dir};

use crate::timeutil;

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
    let path = token_path(home);
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    std::fs::create_dir_all(home)?;
    let token = timeutil::random_hex(32);
    // TODO(core): restrict the file ACL to the current user (core owns ACL handling).
    std::fs::write(&path, &token).with_context(|| format!("write {}", path.display()))?;
    Ok(token)
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
