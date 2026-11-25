//! TEMPORARY STUB (branch feat/daemon). Minimal manager-config load/save so the daemon can run
//! before `feat/core` lands. On merge: delete this file, drop `pub mod store_stub;` from lib.rs and
//! point `mcpm-daemon/src/storage.rs` at the real store. Nothing else depends on it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::ManagerConfig;

/// State dir: `MCPM_HOME`, else `%APPDATA%\mcpm`, else `$XDG_CONFIG_HOME/mcpm`, else `~/.config/mcpm`.
pub fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("MCPM_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("APPDATA").filter(|p| !p.is_empty()) {
        return PathBuf::from(p).join("mcpm");
    }
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(p).join("mcpm");
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".config").join("mcpm")
}

/// Loads `<dir>/config.json`; a missing file gives the default config.
pub fn load_config(dir: &Path) -> Result<ManagerConfig> {
    let path = dir.join("config.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ManagerConfig::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Atomically writes `<dir>/config.json` (temp file + rename).
pub fn save_config(dir: &Path, cfg: &ManagerConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("config.json");
    let tmp = dir.join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
