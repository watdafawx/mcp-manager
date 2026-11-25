//! Windows Job Object with KILL_ON_JOB_CLOSE: one per child process, so closing it (explicitly,
//! or when the daemon dies) kills the whole tree (`npx` -> `node` ...), not just the shim.
//! No-op elsewhere.

#[cfg(windows)]
mod imp {
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub struct KillJob(HANDLE);

    // SAFETY: a job handle is just a kernel object reference, usable from any thread.
    unsafe impl Send for KillJob {}
    unsafe impl Sync for KillJob {}

    impl KillJob {
        /// Creates a job that kills its processes on close and assigns `process` to it.
        pub fn assign(process: RawHandle) -> io::Result<KillJob> {
            // SAFETY: plain Win32 calls with valid arguments; the handle is closed on error/drop.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(job, process as HANDLE) == 0 {
                    let e = io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(e);
                }
                Ok(KillJob(job))
            }
        }
    }

    impl Drop for KillJob {
        fn drop(&mut self) {
            // SAFETY: handle owned by this struct, closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::KillJob;

/// Non-Windows placeholder (children are killed with `kill_on_drop` only).
#[cfg(not(windows))]
pub struct KillJob;
