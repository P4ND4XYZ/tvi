//! Private Windows job objects for supervising child processes.

use std::{
    io,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    ptr,
};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};

pub(crate) struct Job(OwnedHandle);

impl Job {
    #[allow(unsafe_code, reason = "Windows job limits have no std equivalent")]
    pub(crate) fn new(memory_limit: Option<usize>) -> io::Result<Self> {
        // SAFETY: null arguments create a private, unnamed, non-inheritable job.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful call returned a newly owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = memory_limit {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            limits.BasicLimitInformation.ActiveProcessLimit = 1;
            limits.ProcessMemoryLimit = bytes;
        }
        // SAFETY: the structure and its length match the class, and the handle is live.
        if unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .map_err(io::Error::other)?,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    #[allow(
        unsafe_code,
        reason = "Windows job limit queries have no std equivalent"
    )]
    pub(crate) fn current_is_limited(memory_limit: usize) -> bool {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let Ok(size) = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) else {
            return false;
        };
        // SAFETY: a null job handle queries the caller's job; the output buffer
        // and its size match this information class.
        let ok = unsafe {
            QueryInformationJobObject(
                ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                ptr::from_mut(&mut limits).cast(),
                size,
                ptr::null_mut(),
            )
        } != 0;
        let required = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        ok && limits.BasicLimitInformation.LimitFlags & required == required
            && limits.BasicLimitInformation.ActiveProcessLimit == 1
            && (1..=memory_limit).contains(&limits.ProcessMemoryLimit)
    }

    #[allow(unsafe_code, reason = "job attachment has no std equivalent")]
    pub(crate) fn attach(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles refer to live objects and remain owned by their callers.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
