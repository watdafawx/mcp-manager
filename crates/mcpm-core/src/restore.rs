//! Exact and surgical snapshot restoration.
use crate::{
    Core, discovery, edit,
    model::*,
    snapshot::{self, Snapshot},
    store,
    takeover::read_optional,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
/// Outcome for a restored file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreResult {
    pub path: std::path::PathBuf,
    pub merged: bool,
}
/// Restores snapshot files and optionally retains imported manager servers.
pub fn restore(core: &Core, s: &Snapshot, keep_in_manager: bool) -> Result<Vec<RestoreResult>> {
    let mut config = store::load(&core.state_dir)?;
    let mut results = vec![];
    for (n, f) in s.files.iter().enumerate() {
        let original = snapshot::bytes(&core.state_dir, s, n)?;
        let current = read_optional(&f.file.path)?;
        let merged = snapshot::hash(current.as_deref().unwrap_or_default()) != f.after;
        let output = if !merged {
            f.existed.then_some(original.clone())
        } else {
            let old = edit::entries(std::str::from_utf8(&original)?, &f.file)?;
            let mut text = edit::set(
                std::str::from_utf8(current.as_deref().unwrap_or_default())?,
                &f.file,
                MANAGER_ENTRY,
                None,
            )?;
            let present = edit::entries(&text, &f.file)?;
            for (name, v) in old {
                if name != MANAGER_ENTRY && !present.contains_key(&name) {
                    text = edit::set(&text, &f.file, &name, Some(&v))?;
                }
            }
            Some(text.into_bytes())
        };
        // Preserve the current file before a surgical write as well.
        if output != current {
            snapshot::create(
                &core.state_dir,
                "restore",
                &[(
                    f.harness.clone(),
                    f.file.clone(),
                    current.clone(),
                    output.clone().unwrap_or_default(),
                )],
            )?;
            anyhow::ensure!(
                read_optional(&f.file.path)? == current,
                "config changed during restore; retry"
            );
            if let Some(b) = output {
                store::atomic_write(&f.file.path, &b)?;
            } else if f.file.path.exists() {
                std::fs::remove_file(&f.file.path)?;
            }
        }
        if !keep_in_manager {
            let env = std::env::vars().collect();
            let identities = edit::entries(std::str::from_utf8(&original)?, &f.file)?
                .into_values()
                .filter_map(|v| discovery::parse_entry(&f.harness, &v, &env))
                .map(|t| discovery::identity(&t))
                .collect::<Vec<_>>();
            config.servers.retain_mut(|server| {
                let imported = server
                    .origin
                    .as_ref()
                    .is_some_and(|o| o.harness == f.harness)
                    || identities.contains(&discovery::identity(&server.transport));
                if imported && !server.harnesses.is_empty() {
                    server.harnesses.retain(|h| h != &f.harness);
                    return !server.harnesses.is_empty();
                }
                true
            });
        }
        results.push(RestoreResult {
            path: f.file.path.clone(),
            merged,
        });
    }
    store::save(&core.state_dir, &config)?;
    Ok(results)
}
/// Restores the latest takeover snapshot for a harness.
pub fn restore_harness(core: &Core, harness: &str, keep: bool) -> Result<Vec<RestoreResult>> {
    let snapshot = snapshot::list(&core.state_dir)?
        .into_iter()
        .rev()
        .find(|s| s.operation == "takeover" && s.files.iter().any(|f| f.harness == harness))
        .ok_or_else(|| anyhow::anyhow!("no takeover snapshot for harness"))?;
    restore(core, &snapshot, keep)
}
/// Restores every harness, unwinding takeover snapshots newest first.
pub fn restore_all(core: &Core, keep: bool) -> Result<Vec<RestoreResult>> {
    let mut out = vec![];
    for s in snapshot::list(&core.state_dir)?
        .into_iter()
        .rev()
        .filter(|s| s.operation == "takeover")
    {
        out.extend(restore(core, &s, keep)?);
    }
    Ok(out)
}
