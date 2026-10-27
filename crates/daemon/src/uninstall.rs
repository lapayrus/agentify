//! Uninstall, the one place Agentify deliberately stops owning the machine.
//!
//! Non-destructive uninstall removes the login item, the trusted local root, and
//! the Caddy data the daemon rebuilds on demand, and leaves Projects, runtimes,
//! Checkpoints, and the state database alone. A confirmed purge (`--purge`)
//! additionally unregisters every WSL distro this install minted and removes the
//! whole data root, then verifies it is gone.
//!
//! Everything that has an observable OS effect is injected, so no automated test
//! can reach the real certificate store or WSL: the trust installer, the login
//! item, and the runtime remover are all seams.

use std::path::Path;

use anyhow::{Context, Result};

use agentify_proto::DataRoot;

/// What an uninstall actually did, so a caller can report it and a test can
/// assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallReport {
    pub removed_trust: bool,
    pub removed_login_item: bool,
    pub removed_caddy: bool,
    pub removed_path_entry: bool,
    pub removed_runtimes: Vec<String>,
    pub removed_data_root: bool,
    pub notes: Vec<String>,
}

/// Removes every runtime this install owns. Production goes through the WSL
/// adapter; a test substitutes a fake that never touches `wsl.exe`.
pub trait RuntimeRemover: Send + Sync {
    fn remove_all(&self) -> Result<Vec<String>>;
}

/// The production runtime remover: unregister every `agentify-*` distro.
pub struct WslRuntimeRemover;

impl RuntimeRemover for WslRuntimeRemover {
    fn remove_all(&self) -> Result<Vec<String>> {
        agentify_backends::WslBackend::from_env()
            .unregister_all()
            .context("unregistering the WSL distros")
    }
}

/// Removes the daemon's login item. Production removes the Scheduled Task; a
/// test substitutes a fake that records the call.
pub trait LoginItemRemover: Send + Sync {
    fn remove_login_item(&self) -> Result<()>;
}

impl LoginItemRemover for crate::login_item::LoginItem {
    fn remove_login_item(&self) -> Result<()> {
        crate::login_item::LoginItem::remove(self)
    }
}

/// Production wiring: the shipped login item, the Windows trust installer, the
/// WSL runtime remover, and the per-user PATH entry.
///
/// `machine` is the first uninstall question: whether the additions that belong
/// to this installation rather than to its Projects go. `purge` is the second:
/// whether the data root — Projects and their runtimes — goes.
pub fn run_system(data_root: &Path, purge: bool, machine: bool) -> Result<UninstallReport> {
    let login_item = crate::login_item::LoginItem::for_daemon(crate::login_item::DEFAULT_TASK_NAME)?;
    let mut report = run(
        data_root,
        purge,
        machine,
        &login_item,
        &agentify_ingress::WindowsTrustInstaller::new(),
        &WslRuntimeRemover,
    )?;

    // The PATH entry is machine-level too. It is removed here, in the production
    // wiring, so the unit tests never touch this machine's registry.
    if machine {
        match std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            Some(directory) => match crate::path_entry::remove(&directory) {
                Ok(changed) => report.removed_path_entry = changed,
                Err(err) => report
                    .notes
                    .push(format!("removing the PATH entry failed: {err:#}")),
            },
            None => report
                .notes
                .push("could not locate the program directory for the PATH entry".to_string()),
        }
    } else {
        report
            .notes
            .push("the machine-level additions were kept".to_string());
    }

    Ok(report)
}

/// The uninstall itself, against the injected seams.
pub fn run(
    data_root: &Path,
    purge: bool,
    machine: bool,
    login_item: &dyn LoginItemRemover,
    trust: &dyn agentify_ingress::TrustInstaller,
    remover: &dyn RuntimeRemover,
) -> Result<UninstallReport> {
    let root = DataRoot::new(data_root);

    let removed_login_item = machine && login_item.remove_login_item().is_ok();

    // Trust removal is best-effort: the Firefox enterprise-roots step needs
    // elevation, so it comes back as a manual command rather than being run.
    let (removed_trust, notes) = if machine {
        match trust.remove() {
            Ok(report) => {
                let mut notes = report.notes;
                for step in &report.manual {
                    notes.push(format!("run manually: {}", step.describe()));
                }
                (true, notes)
            }
            Err(err) => (false, vec![format!("trust removal failed: {err}")]),
        }
    } else {
        (false, vec!["the machine-level additions were kept".to_string()])
    };

    // Caddy data is rebuilt on the next start, so it goes with the machine-level
    // additions. The Projects, their runtimes, and the state database stay in
    // the first path.
    let removed_caddy =
        machine && remove_dir(root.ingress()) && remove_file(root.tools().join("caddy.exe"));

    // The discovery file is runtime state, not user data: it goes whenever any
    // part of the installation is being removed. A graceful exit already removes
    // it; a crash, a killed daemon, or a stale file does not.
    if machine || purge {
        let _ = remove_file(root.discovery_file());
    }

    let mut report = UninstallReport {
        removed_trust,
        removed_login_item,
        removed_caddy,
        removed_path_entry: false,
        removed_runtimes: Vec::new(),
        removed_data_root: false,
        notes,
    };

    if purge {
        // Runtimes first, while their data root still exists and the adapter can
        // read it.
        report.removed_runtimes = remover.remove_all().context("removing runtimes")?;

        for directory in [
            root.projects(),
            root.runtimes(),
            root.checkpoints(),
            root.logs(),
            root.tools(),
            root.ingress(),
        ] {
            let _ = remove_dir(directory);
        }
        let _ = remove_file(root.state_db());

        report.removed_data_root = remove_dir(root.path().to_path_buf());
        if report.removed_data_root {
            report.notes.push("the data root was removed".to_string());
        } else {
            report.notes.push(format!(
                "the data root {} still exists after purge",
                root.path().display()
            ));
        }
    }

    Ok(report)
}

/// A directory is "removed" when it is no longer there; one that never existed
/// is just as good.
fn remove_dir(path: std::path::PathBuf) -> bool {
    if !path.exists() {
        return true;
    }
    std::fs::remove_dir_all(&path).is_ok() && !path.exists()
}

/// A file is "removed" when it is no longer there; one that never existed is
/// just as good.
fn remove_file(path: std::path::PathBuf) -> bool {
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A login item that only records that it was asked to go.
    struct FakeLoginItem;

    impl LoginItemRemover for FakeLoginItem {
        fn remove_login_item(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A trust installer that never touches the certificate store.
    struct FakeTrust;

    impl agentify_ingress::TrustInstaller for FakeTrust {
        fn commands(&self, _root_cert: &Path) -> agentify_ingress::TrustPrepare {
            agentify_ingress::TrustPrepare::default()
        }

        fn removal_commands(&self) -> agentify_ingress::TrustPrepare {
            agentify_ingress::TrustPrepare::default()
        }

        fn install(
            &self,
            _root_cert: &Path,
        ) -> std::result::Result<agentify_ingress::TrustReport, agentify_ingress::TrustError>
        {
            Ok(agentify_ingress::TrustReport {
                state: agentify_ingress::TrustState::Installed,
                applied: Vec::new(),
                manual: Vec::new(),
                notes: Vec::new(),
            })
        }

        fn remove(
            &self,
        ) -> std::result::Result<agentify_ingress::TrustReport, agentify_ingress::TrustError>
        {
            Ok(agentify_ingress::TrustReport {
                state: agentify_ingress::TrustState::Absent,
                applied: Vec::new(),
                manual: Vec::new(),
                notes: vec!["removed".to_string()],
            })
        }
    }

    /// A runtime remover that reports a scripted list and never runs `wsl.exe`.
    struct FakeRuntimeRemover {
        removed: Vec<String>,
    }

    impl RuntimeRemover for FakeRuntimeRemover {
        fn remove_all(&self) -> Result<Vec<String>> {
            Ok(self.removed.clone())
        }
    }

    /// A login item that must not be touched: the test kept machine additions.
    struct PanicLoginItem;

    impl LoginItemRemover for PanicLoginItem {
        fn remove_login_item(&self) -> Result<()> {
            panic!("the login item must not be removed when machine additions are kept");
        }
    }

    /// The trust store, which must not be touched either.
    struct PanicTrust;

    impl agentify_ingress::TrustInstaller for PanicTrust {
        fn commands(&self, _root_cert: &Path) -> agentify_ingress::TrustPrepare {
            agentify_ingress::TrustPrepare::default()
        }

        fn removal_commands(&self) -> agentify_ingress::TrustPrepare {
            agentify_ingress::TrustPrepare::default()
        }

        fn install(
            &self,
            _root_cert: &Path,
        ) -> std::result::Result<agentify_ingress::TrustReport, agentify_ingress::TrustError>
        {
            panic!("an uninstall must not install trust");
        }

        fn remove(
            &self,
        ) -> std::result::Result<agentify_ingress::TrustReport, agentify_ingress::TrustError>
        {
            panic!("trust must not be removed when machine additions are kept");
        }
    }

    fn data_root() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("Agentify");
        let data = DataRoot::new(&root);
        for directory in data.directories() {
            std::fs::create_dir_all(&directory).expect("create directory");
        }
        std::fs::write(data.state_db(), "sqlite").expect("state.db");
        std::fs::write(data.discovery_file(), "{}").expect("discovery.json");
        std::fs::write(data.tools().join("caddy.exe"), "caddy").expect("caddy.exe");
        std::fs::write(data.projects().join("marker"), "m").expect("a Project workspace");
        (temp, root)
    }

    #[test]
    fn a_non_destructive_uninstall_keeps_projects_and_state() {
        let (_temp, root) = data_root();
        let report = run(
            &root,
            false,
            true,
            &FakeLoginItem,
            &FakeTrust,
            &FakeRuntimeRemover {
                removed: vec!["agentify-dev".to_string()],
            },
        )
        .expect("uninstall");

        assert!(report.removed_trust);
        assert!(report.removed_login_item);
        assert!(report.removed_caddy);
        assert!(report.removed_runtimes.is_empty(), "non-destructive does not touch runtimes");
        assert!(!report.removed_data_root);

        let data = DataRoot::new(&root);
        assert!(data.state_db().exists(), "state.db must survive");
        assert!(data.projects().exists(), "Projects must survive");
        assert!(data.checkpoints().exists(), "Checkpoints must survive");
        assert!(!data.ingress().exists(), "Caddy data must go");
        assert!(!data.tools().join("caddy.exe").exists(), "the staged Caddy binary must go");
        assert!(
            !data.discovery_file().exists(),
            "the discovery file is runtime state, not user data: it must not outlive an uninstall"
        );
    }

    #[test]
    fn a_purge_removes_the_whole_data_root_and_verifies_it() {
        let (_temp, root) = data_root();
        let removed = vec!["agentify-dev".to_string(), "agentify-hello".to_string()];
        let report = run(
            &root,
            true,
            true,
            &FakeLoginItem,
            &FakeTrust,
            &FakeRuntimeRemover {
                removed: removed.clone(),
            },
        )
        .expect("uninstall");

        assert_eq!(report.removed_runtimes, removed);
        assert!(report.removed_data_root, "notes: {:?}", report.notes);
        assert!(!root.exists(), "the data root must be gone after a purge");
    }

    #[test]
    fn keeping_the_machine_additions_still_removes_the_data_root() {
        let (_temp, root) = data_root();
        let report = run(
            &root,
            true,
            false,
            &PanicLoginItem,
            &PanicTrust,
            &FakeRuntimeRemover {
                removed: Vec::new(),
            },
        )
        .expect("uninstall");

        assert!(!report.removed_login_item);
        assert!(!report.removed_trust);
        assert!(!report.removed_caddy);
        assert!(report.removed_data_root);
        assert!(!root.exists());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("machine-level additions were kept")),
            "the report says what was kept: {:?}",
            report.notes
        );
    }

    #[test]
    fn keeping_everything_removes_nothing() {
        let (_temp, root) = data_root();
        let report = run(
            &root,
            false,
            false,
            &PanicLoginItem,
            &PanicTrust,
            &FakeRuntimeRemover {
                removed: Vec::new(),
            },
        )
        .expect("uninstall");

        assert!(!report.removed_login_item);
        assert!(!report.removed_trust);
        assert!(!report.removed_caddy);
        assert!(!report.removed_data_root);

        let data = DataRoot::new(&root);
        assert!(data.state_db().exists());
        assert!(data.ingress().exists());
        assert!(data.tools().join("caddy.exe").exists());
        assert!(
            data.discovery_file().exists(),
            "nothing is removed when both answers are no"
        );
    }

    #[test]
    fn a_missing_directory_is_already_removed() {
        assert!(remove_dir(PathBuf::from("/nonexistent/agentify/uninstall")));
        assert!(remove_file(PathBuf::from("/nonexistent/agentify/caddy.exe")));
    }
}
