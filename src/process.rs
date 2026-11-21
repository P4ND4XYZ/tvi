use std::{
    env,
    ffi::OsStr,
    fs, io,
    path::{self, Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub struct RunError {
    pub kind: &'static str,
    pub source: io::Error,
}

pub fn run(program: &str, args: &[String], cwd: Option<&Path>) -> Result<Output, RunError> {
    run_with_path(program, args, cwd, env::var_os("PATH").as_deref())
}

fn run_with_path(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    search_path: Option<&OsStr>,
) -> Result<Output, RunError> {
    let cwd_error = |source| RunError {
        kind: "cwd",
        source,
    };
    let cwd = cwd.map(path::absolute).transpose().map_err(cwd_error)?;
    if let Some(cwd) = &cwd
        && !fs::metadata(cwd).map_err(cwd_error)?.is_dir()
    {
        return Err(cwd_error(io::Error::new(
            io::ErrorKind::NotADirectory,
            "working directory is not a directory",
        )));
    }
    if program.is_empty() || program.contains('\0') {
        return Err(RunError {
            kind: "spawn",
            source: io::Error::new(io::ErrorKind::InvalidInput, "invalid program name"),
        });
    }
    let executable = executable(program, search_path).map_err(spawn_error)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = &cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.output().map_err(spawn_error)
}

fn spawn_error(source: io::Error) -> RunError {
    let kind = if source.kind() == io::ErrorKind::NotFound {
        "not_found"
    } else {
        "spawn"
    };
    RunError { kind, source }
}

#[cfg(not(windows))]
fn executable(program: &str, search_path: Option<&OsStr>) -> io::Result<PathBuf> {
    if program.contains('/') {
        path::absolute(program)
    } else {
        search_executable(Path::new(program), search_path)
    }
}

#[cfg(windows)]
fn executable(program: &str, search_path: Option<&OsStr>) -> io::Result<PathBuf> {
    // Do not dispatch batch files through cmd.exe or search the working directory implicitly.
    let mut program = PathBuf::from(program);
    match program.extension().and_then(|ext| ext.to_str()) {
        None => {
            program.as_mut_os_string().push(".exe");
        }
        Some(ext) if ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("com") => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "program must be an .exe or .com executable",
            ));
        }
    }
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return path::absolute(program);
    }
    search_executable(&program, search_path)
}

fn search_executable(program: &Path, search_path: Option<&OsStr>) -> io::Result<PathBuf> {
    if let Some(paths) = search_path {
        // A bare name must not depend on the child's working directory.
        for directory in env::split_paths(paths).filter(|dir| dir.is_absolute()) {
            let candidate = directory.join(program);
            match fs::metadata(&candidate) {
                Ok(metadata) if metadata.is_file() => return Ok(candidate),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "executable not found on PATH",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_entry_does_not_search_the_child_working_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let bin = temp.path().join("bin");
        fs::create_dir(&bin)?;
        let name = if cfg!(windows) {
            "test-tool.exe"
        } else {
            "test-tool"
        };
        let tool = bin.join(name);
        fs::copy(env::current_exe()?, &tool)?;

        let args = ["--list".to_owned()];
        let error = run_with_path(name, &args, Some(temp.path()), Some(OsStr::new("bin")))
            .err()
            .ok_or("program was started from the child's working directory")?;
        assert_eq!(error.kind, "not_found");

        let search_path = env::join_paths([Path::new("bin"), &bin])?;
        let output = run_with_path(name, &args, Some(temp.path()), Some(&search_path))
            .map_err(|error| error.source)?;
        assert!(output.status.success());
        Ok(())
    }
}
