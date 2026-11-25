//! State and authentication paths.
use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Resolves state location, honoring MCPM_HOME and platform conventions.
pub fn state_dir(home: &Path, appdata: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("MCPM_HOME") {
        return p.into();
    }
    if cfg!(windows) {
        appdata.join("mcpm")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("mcpm")
    }
}

/// Reads or securely creates a random 32-byte hexadecimal control token.
pub fn token(state: &Path) -> Result<String> {
    fs::create_dir_all(state)?;
    let path = state.join("token");
    if !path.exists() {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random token: {e}"))?;
        let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                super::store::restrict(&path)?;
                f.write_all(value.as_bytes())?;
                f.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let value = fs::read_to_string(path)?;
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid control token"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_token() {
        let d = tempfile::tempdir().unwrap();
        let a = super::token(d.path()).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(a, super::token(d.path()).unwrap());
    }
}
