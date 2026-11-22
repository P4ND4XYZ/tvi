use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use super::{Deadline, Failure, INPUT, OUTPUT};

pub(super) const RENDER_ARGS: [&str; 6] = ["-i", INPUT, "-o", OUTPUT, "--scale", "2"];

#[cfg(unix)]
#[path = "process/unix.rs"]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod windows;
#[cfg(windows)]
use windows as platform;

#[cfg(windows)]
const NAMES: &[&str] = &["mmdc.exe", "mmdc.cmd"];
#[cfg(unix)]
const NAMES: &[&str] = &["mmdc"];

pub(super) fn find(path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?)
        // Empty/relative PATH entries would search the current directory.
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| NAMES.iter().map(move |name| dir.join(name)))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|metadata| metadata.is_file() && platform::executable(&metadata))
        })
}

pub(super) fn run(
    executable: &Path,
    path: &OsStr,
    work: &Path,
    deadline: &Deadline,
) -> Result<(), Failure> {
    let Deadline {
        started,
        budget,
        scope,
    } = *deadline;
    // Reserve part of the existing Unix deadline for cooperative shutdown.
    // This never adds grace beyond the caller's budget.
    #[cfg(unix)]
    let run_budget = budget - (budget / 5).min(Duration::from_millis(250));
    #[cfg(windows)]
    let run_budget = budget;
    if started.elapsed() >= run_budget {
        return Err(Failure::Timeout(scope));
    }

    // Absolute executable and fixed, relative workspace filenames. Rust
    // dispatches Windows batch files; no application-built shell command.
    let mut command = Command::new(executable);
    command
        .args(RENDER_ARGS)
        .current_dir(work)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut running = platform::Running::spawn(&mut command).map_err(|error| Failure::Launch {
        kind: error.kind(),
        os_code: error.raw_os_error(),
    })?;

    loop {
        let remaining = run_budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            #[cfg(unix)]
            running.graceful_shutdown(started, budget);
            return Err(Failure::Timeout(scope));
        }
        if let Some(status) = running
            .try_wait()
            .map_err(|error| Failure::Wait(error.kind()))?
        {
            return if status.success() {
                Ok(())
            } else {
                Err(Failure::Exit {
                    code: status.code(),
                })
            };
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}
