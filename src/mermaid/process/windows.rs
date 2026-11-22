use std::{
    fs::Metadata,
    io,
    os::windows::process::CommandExt,
    process::{Child, Command, ExitStatus},
};

use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::windows_job::Job;

pub(super) fn executable(_: &Metadata) -> bool {
    true
}

pub(super) struct Running {
    child: Child,
    job: Option<Job>,
}

impl Running {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let job = Job::new(None)?;
        let child = command.creation_flags(CREATE_NO_WINDOW).spawn()?;
        let attachment = job.attach(&child);
        let running = Self {
            child,
            job: Some(job),
        };
        if let Err(error) = attachment {
            // A renderer that exited before attachment needs no direct cleanup.
            // If still running, fail rather than continuing without supervision.
            let mut running = running;
            if running.try_wait()?.is_none() {
                return Err(error);
            }
            return Ok(running);
        }
        Ok(running)
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Closing this sole handle terminates processes attached to the job.
        drop(self.job.take());
        drop(self.child.kill());
        drop(self.child.wait());
    }
}
