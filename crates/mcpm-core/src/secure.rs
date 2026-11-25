//! Locks the state dir to the current user: it holds copies of harness secrets
//! (config.json env/headers, snapshot backups) and the control token.
//!
//! Windows: a protected DACL granting full control to the current user and SYSTEM only,
//! inherited by every file and folder inside (existing children are updated too).
//! Unix: mode 0700 on the directory.

use anyhow::Result;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// Creates `dir` if needed and restricts it to the current user. Cheap after the first
/// call per directory in a process.
pub fn secure_dir(dir: &Path) -> Result<()> {
    static DONE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    std::fs::create_dir_all(dir)?;
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let done = DONE.get_or_init(Default::default);
    if done.lock().map(|d| d.contains(&key)).unwrap_or(false) {
        return Ok(());
    }
    apply(dir)?;
    if let Ok(mut d) = done.lock() {
        d.insert(key);
    }
    Ok(())
}

#[cfg(unix)]
fn apply(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
fn apply(dir: &Path) -> Result<()> {
    win::protect(dir)
}

#[cfg(not(any(unix, windows)))]
fn apply(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
mod win {
    use anyhow::{Result, bail};
    use std::{os::windows::ffi::OsStrExt, path::Path, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// String SID of the user running this process.
    fn current_user_sid() -> Result<String> {
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                bail!("OpenProcessToken: {}", std::io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                bail!("GetTokenInformation: {}", std::io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                bail!(
                    "ConvertSidToStringSidW: {}",
                    std::io::Error::last_os_error()
                );
            }
            let n = (0..).take_while(|&i| *sid_str.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
            LocalFree(sid_str.cast());
            Ok(s)
        }
    }

    pub fn protect(dir: &Path) -> Result<()> {
        let sid = current_user_sid()?;
        // P = protected (no inheritance from parent); OICI = files and folders inherit.
        let sddl = wide(&format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)"));
        let path: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                bail!(
                    "build security descriptor: {}",
                    std::io::Error::last_os_error()
                );
            }
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl: *mut ACL = ptr::null_mut();
            let got = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
            // SetNamedSecurityInfoW also propagates the inheritable ACEs to existing children.
            let rc = if got != 0 && present != 0 {
                SetNamedSecurityInfoW(
                    path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null(),
                )
            } else {
                u32::MAX
            };
            LocalFree(sd.cast());
            if rc != ERROR_SUCCESS {
                bail!("lock {} to current user: error {rc}", dir.display());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn current_sid_for_test() -> String {
        current_user_sid().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_dir_and_new_children() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state");
        std::fs::create_dir_all(state.join("old")).unwrap();
        std::fs::write(state.join("old").join("x.bak"), "secret").unwrap();
        secure_dir(&state).unwrap();
        std::fs::write(state.join("config.json"), "{}").unwrap();
        secure_dir(&state).unwrap(); // idempotent

        #[cfg(windows)]
        for p in [state.join("config.json"), state.join("old").join("x.bak")] {
            let out = std::process::Command::new("icacls")
                .arg(&p)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            // Well-known groups that a normal profile folder would otherwise grant.
            for bad in [
                "Everyone",
                "BUILTIN\\Users",
                "Authenticated Users",
                "BUILTIN\\Administrators",
            ] {
                assert!(!text.contains(bad), "{bad} still has access: {text}");
            }
            assert!(text.contains("NT AUTHORITY\\SYSTEM"), "{text}");
            assert!(!win::current_sid_for_test().is_empty());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }
}
