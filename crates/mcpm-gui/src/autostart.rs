//! Autostart: HKCU `...\Run` value `mcpm` = `"<exe>" daemon`. No admin needed.
//!
//! `MCPM_RUN_KEY` redirects the registry key (tests and the rig harness use it so the real Run
//! key is never touched).

use std::path::Path;

pub const DEFAULT_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "mcpm";

/// The registry value that starts the daemon at logon.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" daemon", exe.display())
}

/// Registry subkey under HKCU to use.
pub fn run_key() -> String {
    match std::env::var("MCPM_RUN_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => DEFAULT_RUN_KEY.to_string(),
    }
}

/// Current value, if the autostart entry exists.
#[cfg(windows)]
pub fn current() -> anyhow::Result<Option<String>> {
    current_in(&run_key())
}

/// Adds or removes the autostart value. Idempotent.
#[cfg(windows)]
pub fn set(on: bool, exe: &Path) -> anyhow::Result<()> {
    set_in(&run_key(), on, exe)
}

#[cfg(windows)]
fn current_in(subkey: &str) -> anyhow::Result<Option<String>> {
    use windows_registry::CURRENT_USER;
    let Ok(key) = CURRENT_USER.open(subkey) else {
        return Ok(None);
    };
    Ok(key.get_string(VALUE_NAME).ok())
}

#[cfg(windows)]
fn set_in(subkey: &str, on: bool, exe: &Path) -> anyhow::Result<()> {
    use windows_registry::CURRENT_USER;
    if on {
        let key = CURRENT_USER.create(subkey)?;
        key.set_string(VALUE_NAME, command_for(exe))?;
    } else if let Ok(key) = CURRENT_USER.options().read().write().open(subkey)
        && key.get_string(VALUE_NAME).is_ok()
    {
        key.remove_value(VALUE_NAME)?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn current() -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn set(_on: bool, _exe: &Path) -> anyhow::Result<()> {
    anyhow::bail!("autostart is only implemented for Windows")
}

/// True when the stored value starts the given exe.
pub fn matches_exe(value: &str, exe: &Path) -> bool {
    value == command_for(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_is_quoted_exe_plus_daemon() {
        let exe = Path::new(r"C:\Program Files\mcpm\mcpm.exe");
        assert_eq!(command_for(exe), r#""C:\Program Files\mcpm\mcpm.exe" daemon"#);
        assert!(matches_exe(&command_for(exe), exe));
        assert!(!matches_exe(r"C:\other.exe daemon", exe));
    }

    /// Round trip on a throwaway key (never the real Run key).
    #[cfg(windows)]
    #[test]
    fn set_and_remove_round_trip_on_a_scratch_key() {
        let root = format!(r"Software\mcpm-unit-test-{}", std::process::id());
        let sub = format!(r"{root}\Run");
        let exe = Path::new(r"C:\x\mcpm.exe");
        assert_eq!(current_in(&sub).unwrap(), None);
        set_in(&sub, true, exe).unwrap();
        assert_eq!(current_in(&sub).unwrap(), Some(command_for(exe)));
        set_in(&sub, false, exe).unwrap();
        assert_eq!(current_in(&sub).unwrap(), None, "value must be gone after off");
        set_in(&sub, false, exe).unwrap(); // idempotent
        let _ = windows_registry::CURRENT_USER.remove_tree(&root);
    }

    #[test]
    fn default_key_is_the_run_key() {
        // Not mutating env here: tests run in parallel. Only the default branch is checked.
        if std::env::var_os("MCPM_RUN_KEY").is_none() {
            assert_eq!(run_key(), DEFAULT_RUN_KEY);
        }
    }
}
