//! A bounded log file under the data root.
//!
//! A release daemon is a GUI-subsystem binary with no console, so anything it
//! needs to say after an exit must land in a file. The file is deliberately
//! simple: one append-only `daemon.log`, rotated once to `daemon.log.1` when it
//! grows past the bound. Nothing here can fail loudly — a daemon that cannot
//! write its log still has to serve.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 1024 * 1024;

pub struct DaemonLog {
    path: PathBuf,
    limit: u64,
    file: Mutex<File>,
}

impl DaemonLog {
    /// Open (or create) `logs/daemon.log` under a data root.
    pub fn open(data_root: &Path) -> std::io::Result<Self> {
        Self::open_with_limit(data_root, MAX_BYTES)
    }

    /// The same, with an explicit bound, so a test can force a rotation without
    /// writing a megabyte.
    pub fn open_with_limit(data_root: &Path, limit: u64) -> std::io::Result<Self> {
        let directory = agentify_proto::DataRoot::new(data_root).logs();
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("daemon.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            limit: limit.max(1),
            file: Mutex::new(file),
        })
    }

    /// Append one line, with a timestamp, and rotate if the file has grown past
    /// its bound. A write failure is ignored: logging must not take the daemon
    /// down.
    pub fn write(&self, message: &str) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = writeln!(file, "{} {message}", crate::clock::now_rfc3339());
        let _ = file.flush();
        let too_big = file
            .metadata()
            .map(|metadata| metadata.len() > self.limit)
            .unwrap_or(false);
        if too_big {
            self.rotate(&mut file);
        }
    }

    fn rotate(&self, file: &mut File) {
        let previous = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&self.path, &previous);
        if let Ok(fresh) = OpenOptions::new().create(true).append(true).open(&self.path) {
            *file = fresh;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_timestamped_and_appended() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = DaemonLog::open(temp.path()).expect("open log");
        log.write("first");
        log.write("second");
        let text = std::fs::read_to_string(temp.path().join("logs").join("daemon.log"))
            .expect("read log");
        assert!(text.contains("first"), "{text}");
        assert!(text.contains("second"), "{text}");
        assert_eq!(text.lines().count(), 2, "one line per write: {text}");
    }

    #[test]
    fn the_file_rotates_once_and_keeps_the_newest_lines() {
        let temp = tempfile::tempdir().expect("temp dir");
        let log = DaemonLog::open_with_limit(temp.path(), 60).expect("open log");
        log.write("a line long enough to pass the bound on its own");
        let logs = temp.path().join("logs");
        assert!(
            logs.join("daemon.log.1").is_file(),
            "the previous file is kept"
        );
        let current = std::fs::read_to_string(logs.join("daemon.log")).expect("read log");
        assert!(!current.contains("a line long enough"), "the current file starts fresh: {current}");
    }
}
