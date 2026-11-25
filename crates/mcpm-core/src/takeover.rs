//! Reviewable takeover plans and durable application.
use crate::{
    Core, discovery, edit,
    harness::{HarnessDef, HarnessFile},
    model::*,
    snapshot, store,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
/// Proposed changes to one harness file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePlan {
    pub file: HarnessFile,
    pub original: Option<Vec<u8>>,
    pub imports: Vec<ServerConfig>,
    pub remove: Vec<String>,
    pub final_text: String,
}
/// Dry-run takeover diff; applying rejects stale source bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverPlan {
    pub harness: String,
    pub files: Vec<FilePlan>,
}
/// Plans global-file takeover without writing anything.
pub fn plan_takeover(
    core: &Core,
    harness: &HarnessDef,
    exe: &std::path::Path,
) -> Result<TakeoverPlan> {
    let config = store::load(&core.state_dir)?;
    let env = std::env::vars().collect();
    let mut files = vec![];
    let existing = harness.files.iter().any(|f| f.path.exists());
    for (index, file) in harness.files.iter().enumerate() {
        if file.scope == Scope::Project
            || (existing && !file.path.exists())
            || (!existing && index > 0)
        {
            continue;
        }
        let original = read_optional(&file.path)?;
        let text = std::str::from_utf8(original.as_deref().unwrap_or_default())?;
        let mut final_text = text.to_string();
        let mut imports = vec![];
        let mut remove = vec![];
        for (name, value) in edit::entries(text, file)? {
            if name == MANAGER_ENTRY || name == LEGACY_RELAY_ENTRY {
                continue;
            }
            // Unsupported entries must remain in their harness, never silently disappear.
            if let Some(server) =
                discovery::server(&name, &value, &harness.id, &file.path, Scope::Global, &env)
            {
                imports.push(server);
                remove.push(name.clone());
                final_text = edit::set(&final_text, file, &name, None)?;
            }
        }
        final_text = edit::set(
            &final_text,
            file,
            MANAGER_ENTRY,
            Some(&edit::manager_entry(&harness.id, config.port, exe)),
        )?;
        files.push(FilePlan {
            file: file.clone(),
            original,
            imports,
            remove,
            final_text,
        });
    }
    Ok(TakeoverPlan {
        harness: harness.id.clone(),
        files,
    })
}
pub(crate) fn read_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Applies a reviewed plan, saving imports before removing harness entries.
pub fn apply_takeover(core: &Core, plan: &TakeoverPlan) -> Result<snapshot::Snapshot> {
    for f in &plan.files {
        ensure!(
            read_optional(&f.file.path)? == f.original,
            "config changed; re-plan takeover"
        );
    }
    let files = plan
        .files
        .iter()
        .map(|f| {
            (
                plan.harness.clone(),
                f.file.clone(),
                f.original.clone(),
                f.final_text.as_bytes().to_vec(),
            )
        })
        .collect::<Vec<_>>();
    let snapshot = snapshot::create(&core.state_dir, "takeover", &files)?;
    let mut config = store::load(&core.state_dir)?;
    for f in &plan.files {
        for server in &f.imports {
            let mut server = server.clone();
            if let Some(o) = &mut server.origin {
                o.snapshot = Some(snapshot.id.clone());
            }
            discovery::merge(&mut config.servers, server);
        }
    }
    store::save(&core.state_dir, &config)?;
    for f in &plan.files {
        ensure!(
            read_optional(&f.file.path)? == f.original,
            "config changed; imports saved, re-plan takeover"
        );
        store::atomic_write(&f.file.path, f.final_text.as_bytes())?;
    }
    Ok(snapshot)
}
/// Takes over each detected harness, returning all operation snapshots.
pub fn takeover_all(core: &Core, exe: &std::path::Path) -> Result<Vec<snapshot::Snapshot>> {
    let mut out = vec![];
    for h in crate::harness::registry(&core.home, &core.appdata)
        .into_iter()
        .filter(crate::harness::detected)
    {
        out.push(takeover(core, &h, exe)?);
    }
    Ok(out)
}
/// Plans and applies takeover, retrying once when a harness rewrites its file.
pub fn takeover(
    core: &Core,
    harness: &HarnessDef,
    exe: &std::path::Path,
) -> Result<snapshot::Snapshot> {
    let plan = plan_takeover(core, harness, exe)?;
    match apply_takeover(core, &plan) {
        Err(error) if error.to_string().starts_with("config changed") => {
            apply_takeover(core, &plan_takeover(core, harness, exe)?)
        }
        result => result,
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn stale_plan() {
        let d = tempfile::tempdir().unwrap();
        let core = crate::Core {
            home: d.path().into(),
            appdata: d.path().join("app"),
            state_dir: d.path().join("state"),
        };
        let h = crate::harness::registry(&core.home, &core.appdata).remove(0);
        let p = super::plan_takeover(&core, &h, std::path::Path::new("mcpm.exe")).unwrap();
        std::fs::write(&h.files[0].path, "{}").unwrap();
        assert!(super::apply_takeover(&core, &p).is_err());
    }
}
