//! Windows Job Object memory limit for the helper process (plan D8 rule 3).
//!
//! On Unix the helper limits itself with `RLIMIT_AS`; Windows has no such
//! per-process call, so the *parent* puts the child in a Job Object whose
//! limits the kernel enforces: `JOB_OBJECT_LIMIT_PROCESS_MEMORY` caps the
//! committed memory of the helper, and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! kills it if the parent dies, so an orphaned helper cannot outlive the app.
//! The helper is assigned before the request is sent, and it does nothing but
//! wait for the request until then, so it never runs unconfined.
//!
//! # Unsafe
//!
//! This is the only module (with `rarfmt`) with `unsafe`: three Win32 calls
//! and a handle that is closed exactly once in `Drop`. Each block says why it
//! is sound. `tests/supervision.rs` has the test that a breach is reported as
//! a crash and the parent survives (it runs on Windows in CI).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};

/// A Job Object holding one helper process.
pub struct Job(HANDLE);

impl Job {
    /// A job that caps each process's committed memory at `bytes` and kills
    /// its processes when the last handle to it closes.
    pub fn with_memory_limit(bytes: u64) -> io::Result<Job> {
        let limit = usize::try_from(bytes).unwrap_or(usize::MAX);
        // SAFETY: null attributes and a null name are valid arguments; the
        // result is checked before use.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);
        // SAFETY: the struct is plain integers and pointers; all-zero is a
        // valid "no limits" value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = limit;
        // SAFETY: `handle` is a live job; `info` is a correctly sized
        // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` for the class passed.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Puts `child` in the job. An error means the limit is NOT in force.
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are live: the job is owned by `self`, and the
        // child's handle stays open while `child` is borrowed.
        let ok = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}
