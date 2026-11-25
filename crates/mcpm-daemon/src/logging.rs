//! Rolling log file: `logs/daemon.log`, rolled to `daemon.log.1` at 1 MB (so max 2 x 1 MB).
//! Never log request bodies, env values or headers.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::timeutil;

const MAX_BYTES: u64 = 1024 * 1024;

pub struct Logger {
    file: Option<Mutex<LogFile>>,
}

struct LogFile {
    path: PathBuf,
    file: File,
    size: u64,
}

impl Logger {
    /// Discards everything.
    pub fn null() -> Self {
        Self { file: None }
    }

    pub fn open(dir: &Path) -> Self {
        let path = dir.join("daemon.log");
        let opened = std::fs::create_dir_all(dir)
            .and_then(|_| OpenOptions::new().create(true).append(true).open(&path))
            .map(|file| {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                Mutex::new(LogFile { path, file, size })
            });
        Self { file: opened.ok() }
    }

    pub fn log(&self, level: &str, msg: &str) {
        let Some(lock) = &self.file else { return };
        let Ok(mut f) = lock.lock() else { return };
        let line = format!("{} {level:<5} {msg}\n", timeutil::iso(timeutil::now_ms()));
        if f.size + line.len() as u64 > MAX_BYTES {
            let old = PathBuf::from(format!("{}.1", f.path.display()));
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&f.path, &old);
            if let Ok(nf) = OpenOptions::new().create(true).append(true).open(&f.path) {
                f.file = nf;
                f.size = 0;
            }
        }
        if f.file.write_all(line.as_bytes()).is_ok() {
            f.size += line.len() as u64;
        }
    }

    pub fn info(&self, msg: &str) {
        self.log("INFO", msg);
    }
    pub fn warn(&self, msg: &str) {
        self.log("WARN", msg);
    }
    pub fn error(&self, msg: &str) {
        self.log("ERROR", msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_at_limit() {
        let dir = std::env::temp_dir().join(format!("mcpm-log-{}", crate::timeutil::uuid()));
        let log = Logger::open(&dir);
        let big = "x".repeat(100_000);
        for _ in 0..12 {
            log.info(&big);
        }
        assert!(dir.join("daemon.log.1").exists());
        assert!(std::fs::metadata(dir.join("daemon.log")).unwrap().len() <= MAX_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
