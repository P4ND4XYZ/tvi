#![allow(
    unsafe_code,
    reason = "Windows Job Object supervision has no std equivalent"
)]

use std::{
    fs::Metadata,
    io,
    mem::size_of,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Child, Command, ExitStatus},
    ptr,
};

use windows_sys::Win32::System::{
    JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    },
    Threading::CREATE_NO_WINDOW,
};

pub(super) fn executable(_: &Metadata) -> bool {
    true
}

pub(super) struct Running {
    child: Child,
    job: Option<OwnedHandle>,
}

impl Running {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        let job = create_job()?;
        let child = command.creation_flags(CREATE_NO_WINDOW).spawn()?;
        let running = Self {
            child,
            job: Some(job),
        };
        // SAFETY: both handles refer to live objects and remain owned here.
        if unsafe {
            AssignProcessToJobObject(
                running
                    .job
                    .as_ref()
                    .map_or(ptr::null_mut(), AsRawHandle::as_raw_handle),
                running.child.as_raw_handle(),
            )
        } == 0
        {
            // A renderer that exited before attachment needs no direct cleanup.
            // If still running, fail rather than continuing without supervision.
            let error = io::Error::last_os_error();
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

fn create_job() -> io::Result<OwnedHandle> {
    // SAFETY: null arguments create a private, unnamed and non-inheritable job.
    let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call returned a newly owned handle.
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the structure and its length match the information class;
    // the job handle remains valid during the call.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}
