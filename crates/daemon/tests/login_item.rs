//! The login item, exercised against the real Task Scheduler.
//!
//! This is an integration test because the thing under test is an OS side
//! effect: a Scheduled Task either exists or it does not, and only the scheduler
//! can answer. It installs a task under a unique name, checks the definition the
//! scheduler stored, removes it, and checks it is gone — and it removes the task
//! on the way out even when an assertion fails, so the machine is left exactly
//! as it was found.
//!
//! The task it registers runs `cmd /c exit 0`, never the daemon: a test must not
//! start a second daemon or bind the machine's real login item.

use std::path::PathBuf;

use agentify_daemon::login_item::{self, LoginItem};

/// A unique task name, so two runs and two tests cannot collide.
fn unique_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("Agentify Test {}-{}", std::process::id(), nanos)
}

/// Removes the task when it goes out of scope, whatever happened.
struct Installed {
    name: String,
}

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = login_item::remove(&self.name);
    }
}

fn harmless_item(name: &str) -> LoginItem {
    let program = if cfg!(windows) {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()))
            .join("System32")
            .join("cmd.exe")
    } else {
        PathBuf::from("/bin/true")
    };
    LoginItem::new(name, program).with_args(["/c", "exit 0"])
}

#[cfg(windows)]
#[test]
fn a_login_item_is_registered_and_removed_again() {
    let name = unique_name();
    // Nothing left over from a previous run.
    let _ = login_item::remove(&name);
    assert!(
        !login_item::status(&name).expect("status").registered,
        "a fresh task name must not be registered"
    );

    let item = harmless_item(&name);
    item.install().expect("install the login item");
    // From here on the task is removed even if an assertion below fails.
    let _guard = Installed { name: name.clone() };

    let status = item.status().expect("status after install");
    assert!(status.registered, "the task exists after install");
    assert!(
        status
            .detail
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("cmd.exe"),
        "the task runs the program it was given, got {:?}",
        status.detail
    );

    // The definition Task Scheduler actually stored: logon trigger, current
    // user, no elevation, and restart on failure.
    let definition = login_item::definition(&name)
        .expect("read the task definition")
        .expect("the task has a definition");
    assert!(
        definition.contains("<LogonTrigger>"),
        "the task starts at logon:\n{definition}"
    );
    assert!(
        definition.contains("<UserId>"),
        "the task names a principal:\n{definition}"
    );
    // The scheduler stores the *effective* definition, and elides a `RunLevel`
    // that is the default (`LeastPrivilege`). So "not elevated" is checked as
    // the absence of `HighestAvailable`, not as the presence of a word the
    // scheduler does not write back.
    assert!(
        !definition.contains("HighestAvailable"),
        "a per-user task must not run elevated:\n{definition}"
    );
    assert!(
        definition.contains("<LogonType>InteractiveToken</LogonType>"),
        "the task runs as the logged-on user, not as a service:\n{definition}"
    );
    assert!(
        definition.contains("<RestartOnFailure>"),
        "a crashed daemon is restarted:\n{definition}"
    );
    let user = login_item::current_user().expect("current user");
    assert!(
        definition.contains(&user),
        "the task runs as {user}:\n{definition}"
    );

    item.remove().expect("remove the login item");
    assert!(
        !login_item::status(&name).expect("status after remove").registered,
        "the task is gone after remove"
    );
    assert!(
        login_item::definition(&name)
            .expect("query after remove")
            .is_none(),
        "the scheduler holds no definition after remove"
    );
}

#[cfg(windows)]
#[test]
fn removing_a_login_item_that_is_not_there_is_not_an_error() {
    let name = unique_name();
    login_item::remove(&name).expect("removing an absent task is a no-op");
    assert!(!login_item::status(&name).expect("status").registered);
}

#[cfg(windows)]
#[test]
fn the_shipped_login_item_starts_the_daemon_beside_the_running_binary() {
    // What the installer registers, without registering it: the command line
    // has to point at `agentifyd.exe` and never at a service wrapper.
    let item = LoginItem::for_daemon(login_item::DEFAULT_TASK_NAME).expect("the daemon binary");
    assert!(item.program.ends_with("agentifyd.exe"), "{:?}", item.program);
    assert!(!item.program.to_string_lossy().contains("svchost"));
    let xml = item.xml(&login_item::current_user().expect("current user"));
    assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"), "{xml}");
    assert!(xml.contains("<RestartOnFailure>"), "{xml}");
}

#[cfg(not(windows))]
#[test]
fn the_login_item_is_windows_only_in_v0() {
    let item = harmless_item(&unique_name());
    let err = item.install().expect_err("Unix refuses the Windows mechanism");
    assert!(err.to_string().contains("Scheduled Task"), "{err}");
}
