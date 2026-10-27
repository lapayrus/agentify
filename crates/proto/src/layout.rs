//! The one non-roaming data root, and everything that lives under it.
//!
//! Every Agentify process resolves the same root — `AGENTIFY_HOME` when it is
//! set, `%LOCALAPPDATA%\Agentify\` on Windows, `~/Library/Application
//! Support/Agentify/` on macOS — and nothing durable lives anywhere else. That
//! is what makes installing over a version safe: the installer owns the program
//! directory, this module owns the data, and the two never overlap.
//!
//! ```text
//! %LOCALAPPDATA%\Agentify\
//!   state.db            embedded SQLite: Projects, Environments, Approvals, audit
//!   discovery.json      loopback port and Agent token, ACL'd to the current user
//!   logs\               daemon, ingress, and per-Project logs
//!   checkpoints\        Checkpoint archives
//!   projects\<id>\      one directory per Project, holding its workspace
//!   wsl\rootfs\         cached base rootfs tars, verified against a pinned digest
//!   wsl\env\<env>\      runtime disks: one WSL distro per Environment
//!   ingress\            Caddy config, admin endpoint, and the local CA's storage
//!   tools\caddy.exe     the Caddy binary the installer staged
//! ```
//!
//! The `wsl\` subtree is named for the adapter that owns it: a runtime disk is
//! adapter-specific and never read by another adapter.

use std::path::{Path, PathBuf};

use crate::paths::resolve_data_root;

/// Per-Project directories, each holding one Project's workspace.
pub const PROJECTS_DIR: &str = "projects";

/// Daemon, ingress, and per-Project logs.
pub const LOGS_DIR: &str = "logs";

/// Checkpoint archives.
pub const CHECKPOINTS_DIR: &str = "checkpoints";

/// The runtime-backed adapter's subtree: rootfs cache and runtime disks.
pub const WSL_DIR: &str = "wsl";

/// Cached base rootfs tars, verified against the pinned digest.
pub const ROOTFS_DIR: &str = "rootfs";

/// One runtime disk per Environment.
pub const RUNTIMES_DIR: &str = "env";

/// The Caddy binary and its data.
pub const INGRESS_DIR: &str = "ingress";

/// Helper binaries the installer staged: Caddy today, others later.
pub const TOOLS_DIR: &str = "tools";

/// The data root, with a path for everything it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRoot {
    root: PathBuf,
}

impl DataRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The active data root, resolved the way every process resolves it.
    pub fn resolve() -> Self {
        Self::new(resolve_data_root())
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The state database.
    pub fn state_db(&self) -> PathBuf {
        crate::state_db_path(&self.root)
    }

    /// The discovery file: a loopback port and an Agent token.
    pub fn discovery_file(&self) -> PathBuf {
        crate::discovery_path(&self.root)
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join(LOGS_DIR)
    }

    pub fn checkpoints(&self) -> PathBuf {
        self.root.join(CHECKPOINTS_DIR)
    }

    pub fn projects(&self) -> PathBuf {
        self.root.join(PROJECTS_DIR)
    }

    /// Where the runtime adapter keeps its rootfs cache and runtime disks.
    pub fn runtimes(&self) -> PathBuf {
        self.root.join(WSL_DIR)
    }

    /// The rootfs cache.
    pub fn rootfs_cache(&self) -> PathBuf {
        self.runtimes().join(ROOTFS_DIR)
    }

    /// One runtime disk per Environment.
    pub fn runtime_disks(&self) -> PathBuf {
        self.runtimes().join(RUNTIMES_DIR)
    }

    pub fn ingress(&self) -> PathBuf {
        self.root.join(INGRESS_DIR)
    }

    pub fn tools(&self) -> PathBuf {
        self.root.join(TOOLS_DIR)
    }

    /// Every directory the daemon owns, in creation order.
    pub fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.root.clone(),
            self.logs(),
            self.checkpoints(),
            self.projects(),
            self.rootfs_cache(),
            self.runtime_disks(),
            self.ingress(),
            self.tools(),
        ]
    }

    /// Create the tree. Idempotent: a reinstall must not disturb what is here.
    pub fn ensure(&self) -> std::io::Result<()> {
        for directory in self.directories() {
            std::fs::create_dir_all(&directory)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_directory_sits_under_the_root() {
        let root = DataRoot::new(PathBuf::from("C:\\Users\\someone\\AppData\\Local\\Agentify"));
        for directory in root.directories() {
            assert!(
                directory.starts_with(root.path()),
                "{} escapes {}",
                directory.display(),
                root.path().display()
            );
        }
        assert_eq!(root.logs(), root.path().join("logs"));
        assert_eq!(root.checkpoints(), root.path().join("checkpoints"));
        assert_eq!(root.rootfs_cache(), root.path().join("wsl").join("rootfs"));
        assert_eq!(root.runtime_disks(), root.path().join("wsl").join("env"));
        assert_eq!(root.state_db(), root.path().join("state.db"));
        assert_eq!(root.discovery_file(), root.path().join("discovery.json"));
    }

    #[test]
    fn ensure_creates_the_tree_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = DataRoot::new(temp.path().join("Agentify"));
        root.ensure().expect("first ensure");
        root.ensure().expect("second ensure");
        for directory in root.directories() {
            assert!(directory.is_dir(), "{} was not created", directory.display());
        }
    }

    #[test]
    fn the_default_root_is_outside_any_program_directory() {
        // A reinstall replaces the program directory; the data root must be a
        // sibling of it, never a child, or Projects and state would be deleted
        // with the old version's files.
        let root = resolve_data_root();
        let lowered = root.to_string_lossy().to_ascii_lowercase();
        assert!(
            !lowered.contains("program files"),
            "the data root must not live in a program directory: {}",
            root.display()
        );
    }
}
