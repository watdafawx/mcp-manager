//! Atomic manager configuration persistence.
use crate::model::*;
use anyhow::{Result, ensure};
use std::{fs, io::Write, path::Path};

pub(crate) fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    // TODO Windows: install a protected current-user-only DACL using Win32.
    let _ = path;
    Ok(())
}

/// Writes bytes through a same-directory temporary file and atomic replacement.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random temp: {e}"))?;
    let temp = parent.join(format!(".mcpm-{:x}.tmp", u128::from_le_bytes(random)));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        restrict(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Loads manager config, returning defaults only when absent.
pub fn load(state: &Path) -> Result<ManagerConfig> {
    match fs::read(state.join("config.json")) {
        Ok(b) => {
            let c = serde_json::from_slice(&b)?;
            validate(&c)?;
            Ok(c)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ManagerConfig::default()),
        Err(e) => Err(e.into()),
    }
}
/// Validates server identifiers and transport requirements at the persistence boundary.
pub fn validate(config: &ManagerConfig) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for s in &config.servers {
        ensure!(
            valid_server_id(&s.id) && ids.insert(s.id.to_lowercase()),
            "invalid or duplicate server id"
        );
        match &s.transport {
            Transport::Stdio { command, .. } => {
                ensure!(!command.trim().is_empty(), "empty command")
            }
            Transport::Http { url, .. } => ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "invalid HTTP URL"
            ),
        }
    }
    Ok(())
}
/// Validates and atomically persists manager configuration.
pub fn save(state: &Path, config: &ManagerConfig) -> Result<()> {
    validate(config)?;
    atomic_write(
        &state.join("config.json"),
        &serde_json::to_vec_pretty(config)?,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let c = super::load(d.path()).unwrap();
        super::save(d.path(), &c).unwrap();
        assert_eq!(super::load(d.path()).unwrap(), c);
        std::fs::write(d.path().join("config.json"), "bad").unwrap();
        assert!(super::load(d.path()).is_err());
    }
}
