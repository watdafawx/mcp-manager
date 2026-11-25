//! Immutable raw-byte operation snapshots.
use crate::harness::HarnessFile;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
/// Snapshot record for one touched file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub harness: String,
    pub file: HarnessFile,
    pub existed: bool,
    pub before: String,
    pub after: String,
    pub backup: String,
}
/// Immutable operation manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub timestamp: u128,
    pub operation: String,
    pub files: Vec<SnapshotFile>,
}
/// Hashes raw file bytes.
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn directory(state: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid snapshot id"
    );
    Ok(state.join("snapshots").join(id))
}
/// Harness id, file definition, optional original bytes, and planned replacement bytes.
pub type SnapshotInput = (String, HarnessFile, Option<Vec<u8>>, Vec<u8>);
/// Creates a durable snapshot before writes, including their planned post-write hashes.
pub fn create(state: &Path, operation: &str, files: &[SnapshotInput]) -> Result<Snapshot> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random snapshot: {e}"))?;
    let id = format!("{timestamp}-{:x}", u64::from_le_bytes(random));
    let dir = directory(state, &id)?;
    crate::secure::secure_dir(state)?;
    fs::create_dir_all(dir.join("files"))?;
    let mut manifest = Snapshot {
        id,
        timestamp,
        operation: operation.into(),
        files: vec![],
    };
    for (n, (h, f, before, after)) in files.iter().enumerate() {
        let backup = format!("files/{n}.bak");
        crate::store::atomic_write(&dir.join(&backup), before.as_deref().unwrap_or_default())?;
        manifest.files.push(SnapshotFile {
            harness: h.clone(),
            file: f.clone(),
            existed: before.is_some(),
            before: hash(before.as_deref().unwrap_or_default()),
            after: hash(after),
            backup,
        });
    }
    crate::store::atomic_write(
        &dir.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
/// Loads a manifest by validated identifier.
pub fn load(state: &Path, id: &str) -> Result<Snapshot> {
    let s: Snapshot =
        serde_json::from_slice(&fs::read(directory(state, id)?.join("manifest.json"))?)?;
    ensure!(s.id == id, "snapshot identity mismatch");
    Ok(s)
}
/// Lists snapshots in timestamp order.
pub fn list(state: &Path) -> Result<Vec<Snapshot>> {
    let mut out = vec![];
    let dir = state.join("snapshots");
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(load(state, &entry.file_name().to_string_lossy())?);
        }
    }
    out.sort_by_key(|s| s.timestamp);
    Ok(out)
}
/// Loads and verifies a snapshot's original raw bytes.
pub fn bytes(state: &Path, s: &Snapshot, n: usize) -> Result<Vec<u8>> {
    let f = s
        .files
        .get(n)
        .ok_or_else(|| anyhow::anyhow!("missing snapshot file"))?;
    let b = fs::read(directory(state, &s.id)?.join(format!("files/{n}.bak")))?;
    ensure!(hash(&b) == f.before, "snapshot backup checksum mismatch");
    Ok(b)
}
/// Deletes only the snapshot directory selected by a validated identifier.
pub fn delete(state: &Path, id: &str) -> Result<()> {
    fs::remove_dir_all(directory(state, id)?)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn rejects_traversal() {
        let d = tempfile::tempdir().unwrap();
        assert!(super::delete(d.path(), "../outside").is_err());
        assert_eq!(
            super::hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
