//! Killing git together with everything it started (ssh, credential
//! helpers, hooks), so a cancel leaves nothing running.
//!
//! - Windows: each git child goes into its own job object; cancel
//!   terminates the job.
//! - Unix: each git child leads its own process group; cancel sends
//!   SIGTERM to the group, so git can remove its lock files, and the runner
//!   escalates to SIGKILL if git is still alive after a grace period.

use std::process::{Child, Command};

/// Prepares a command so its whole tree can be killed later.
pub fn prepare(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

pub struct ProcessTree {
    #[cfg(windows)]
    job: Option<windows::Job>,
    #[cfg(unix)]
    group: u32,
}

impl ProcessTree {
    pub fn attach(child: &Child) -> Self {
        #[cfg(windows)]
        {
            Self {
                job: windows::Job::attach(child),
            }
        }
        #[cfg(unix)]
        {
            Self { group: child.id() }
        }
    }

    /// Asks the whole tree to stop. On Unix this is SIGTERM; on Windows the
    /// job is terminated at once (Windows has no gentle signal for a
    /// windowless process).
    pub fn terminate(&self, child: &mut Child) {
        #[cfg(windows)]
        match &self.job {
            Some(job) => job.terminate(),
            None => {
                let _ = child.kill();
            }
        }
        #[cfg(unix)]
        {
            let _ = child;
            signal_group(self.group, "TERM");
        }
    }

    /// Kills the tree without waiting for it to clean up.
    pub fn kill(&self, child: &mut Child) {
        #[cfg(windows)]
        self.terminate(child);
        #[cfg(unix)]
        {
            signal_group(self.group, "KILL");
            let _ = child.kill();
        }
    }
}

/// Signals a process group with the system `kill`, which every Unix has.
/// Only called while the group leader is still unreaped, so its id cannot
/// have been reused by an unrelated group.
#[cfg(unix)]
fn signal_group(group: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([format!("-{signal}"), "--".into(), format!("-{group}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(windows)]
mod windows {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };

    /// A job holding one git child and all its descendants. It is not
    /// kill-on-close, so processes still running after a normal exit are
    /// left alone; only a cancel terminates the job.
    pub struct Job(HANDLE);

    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// `None` if the job cannot be created or assigned; the caller then
        /// falls back to killing git alone. git runs briefly before it is
        /// assigned, and a process it starts in that moment stays outside
        /// the job; git does its own setup first, so this is unlikely.
        pub fn attach(child: &Child) -> Option<Self> {
            // SAFETY: plain Win32 calls on handles we own; every pointer
            // passed is valid for the duration of the call.
            unsafe {
                let handle = CreateJobObjectW(null(), null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                // Default limits: no breakaway. With breakaway allowed, the
                // MSYS runtime behind Git for Windows' `sh` starts its
                // children outside the job, so a cancel missed a hook's
                // `sleep`. The price: a child that insists on leaving the
                // job (a daemon) fails to start, and git runs without it.
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return None;
                }
                Some(job)
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is open until drop.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}
