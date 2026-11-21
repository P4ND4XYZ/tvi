#![allow(
    unsafe_code,
    reason = "Unix process-group signaling has no std equivalent"
)]

use std::{
    ffi::c_int,
    fs::Metadata,
    io,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

unsafe extern "C" {
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

pub(super) fn executable(metadata: &Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

pub(super) struct Running {
    child: Child,
    reaped: bool,
}

impl Running {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command.process_group(0).spawn()?;
        Ok(Self {
            child,
            reaped: false,
        })
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped = status.is_some();
        Ok(status)
    }

    pub(super) fn graceful_shutdown(&self, started: Instant, budget: Duration) {
        if started.elapsed() < budget {
            self.signal_group(15); // SIGTERM
        }
        // Do not reap the group leader during grace: retaining its PID lets
        // Drop safely signal the original group even if the leader exits.
        loop {
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining);
        }
        // Drop sends SIGKILL and reaps the direct child. The OS may delay
        // the scheduling or completion of that wait (e.g. uninterruptible I/O).
    }

    fn signal_group(&self, signal: c_int) {
        // SAFETY: process_group(0) puts the child in a dedicated group with
        // its PID. The leader has not been reaped, so the ID cannot be reused.
        if let Ok(pid) = c_int::try_from(self.child.id()) {
            unsafe { kill(-pid, signal) };
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // After reaping the PID could be reused for an unrelated group.
        if self.reaped {
            return;
        }
        self.signal_group(9); // SIGKILL
        // Reap the direct child even if sending the group signal failed.
        drop(self.child.kill());
        drop(self.child.wait());
    }
}
