//! The shell's ownership of its daemon, enforced by the OS.
//!
//! The shell spawns the daemon as a child and puts it in a Job Object with
//! `KILL_ON_JOB_CLOSE`. Closing the job's handle — a normal quit, a crash, the
//! process being killed — takes the daemon and everything it spawned (Caddy)
//! with it. The shell still asks the daemon to stop on a normal exit, so the
//! shutdown is graceful and the discovery file is removed; the job is the
//! backstop that a crashed shell cannot forget.
//!
//! Every OS effect is behind this small type, and the non-Windows build is a
//! no-op, so the rest of the shell does not need platform code.

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// One job per shell run. Dropping it kills every process still inside.
    pub struct DaemonJob(HANDLE);

    // The handle is owned by the shell and only ever passed to Win32 by value;
    // a kernel handle is safe to share between threads.
    unsafe impl Send for DaemonJob {}
    unsafe impl Sync for DaemonJob {}

    impl DaemonJob {
        pub fn new() -> Result<Self, String> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(last_error("CreateJobObject"));
                }

                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if configured == 0 {
                    CloseHandle(job);
                    return Err(last_error("SetInformationJobObject"));
                }
                Ok(Self(job))
            }
        }

        /// Put an already-running process in this job. Its children — Caddy —
        /// inherit membership, so they die with it.
        pub fn attach(&self, pid: u32) -> Result<(), String> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return Err(last_error("OpenProcess"));
                }
                let assigned = AssignProcessToJobObject(self.0, process);
                let failure = (assigned == 0).then(|| last_error("AssignProcessToJobObject"));
                CloseHandle(process);
                failure.map_or(Ok(()), Err)
            }
        }
    }

    impl Drop for DaemonJob {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn last_error(operation: &str) -> String {
        format!(
            "{operation} failed: {}",
            std::io::Error::last_os_error()
        )
    }
}

#[cfg(not(windows))]
mod platform {
    /// v0 ships a Windows daemon; on another OS the shell owns nothing yet.
    pub struct DaemonJob;

    impl DaemonJob {
        pub fn new() -> Result<Self, String> {
            Ok(Self)
        }

        pub fn attach(&self, _pid: u32) -> Result<(), String> {
            Ok(())
        }
    }
}

pub use platform::DaemonJob;

#[cfg(all(test, windows))]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::DaemonJob;

    #[test]
    fn dropping_the_job_ends_a_process_inside_it() {
        let job = DaemonJob::new().expect("create a job object");
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a long-running child");

        if let Err(err) = job.attach(child.id()) {
            // Some CI hosts already hold the test process in a job that forbids
            // nesting; the OS contract is still what `attach` uses.
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("skipping the job test: {err}");
            return;
        }

        drop(job);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = child.kill();
                    panic!("the job did not end its process when its handle closed");
                }
            }
        }
    }
}
