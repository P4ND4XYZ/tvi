use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let binary = Path::new(env!("CARGO_BIN_EXE_tvi"));
        let base = binary
            .parent()
            .ok_or_else(|| io::Error::other("binary has no parent"))?;
        for suffix in 0..100 {
            let path = base.join(format!("cli-{}-{suffix}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("could not allocate test directory"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "could not clean test directory {}: {error}",
                self.0.display()
            );
        }
    }
}

fn run(dir: &Scratch, args: &[&str]) -> io::Result<Output> {
    run_with_log_level(dir, args, "off-by-default")
}

fn logs(dir: &Scratch) -> PathBuf {
    dir.0.join("state/tvi/logs")
}

fn run_with_log_level(dir: &Scratch, args: &[&str], level: &str) -> io::Result<Output> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tvi"))
        .current_dir(&dir.0)
        .env("HOME", dir.0.join("home"))
        .env("XDG_STATE_HOME", dir.0.join("state"))
        .env("XDG_CONFIG_HOME", dir.0.join("config"))
        .env("LOCALAPPDATA", dir.0.join("state"))
        .env("TVI_LOG", level)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err(io::Error::new(io::ErrorKind::TimedOut, "tvi did not exit")),
            Err(error) => break Err(error),
        }
    };
    let kill = child.kill();
    child.wait()?;
    kill?;
    result
}

#[test]
fn invalid_invocations_fail_before_touching_the_terminal() -> io::Result<()> {
    let dir = Scratch::new()?;
    fs::write(dir.0.join("bad.md"), [0xff])?;
    for (args, diagnostic) in [
        (&[][..], "exactly one path"),
        (&["first.md", "second.md"][..], "exactly one path"),
        (&["missing.md"][..], "cannot read"),
        (&["bad.md"][..], "not valid UTF-8"),
    ] {
        let output = run(&dir, args)?;
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(diagnostic), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("interactive terminal required"),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn valid_utf8_path_reaches_terminal_preflight() -> io::Result<()> {
    let dir = Scratch::new()?;
    fs::write(dir.0.join("文書.md"), "# 日本語")?;
    let output = run(&dir, &["文書.md"])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal required"));
    Ok(())
}

#[test]
fn pdf_cli_does_not_use_markdown_utf8_decoding() -> io::Result<()> {
    let dir = Scratch::new()?;
    fs::write(dir.0.join("資料.PDF"), b"%PDF-1.4\n\xff")?;
    let output = run(&dir, &["資料.PDF"])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = if cfg!(windows) {
        "interactive terminal required"
    } else {
        "PDF viewing is only supported on Windows"
    };
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!stderr.contains("not valid UTF-8"), "{stderr}");
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn startup_logs_safe_document_and_terminal_fields() -> io::Result<()> {
    use serde_json::Value;

    let dir = Scratch::new()?;
    let log = logs(&dir).join("tvi.log");
    let usage = run(&dir, &[])?;
    assert!(!usage.status.success());
    assert!(!log.exists());
    fs::create_dir(dir.0.join("private-parent"))?;
    let path = dir.0.join("private-parent").join("文書.md");
    let path_str = path
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF-8 test path"))?;
    let missing = run(&dir, &[path_str])?;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot read"));
    fs::write(&path, "# body-must-not-be-logged")?;
    let preflight = run_with_log_level(&dir, &[path_str], "debug")?;
    assert!(!preflight.status.success());
    let content = fs::read_to_string(log)?;
    assert!(!content.contains("private-parent"));
    assert!(!content.contains("body-must-not-be-logged"));
    let entries: Vec<Value> = content
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(entries.len(), 3);
    let missing = &entries[0];
    assert_eq!(missing["event"], "document.open_failed");
    assert_eq!(missing["document"], "文書.md");
    assert_eq!(missing["level"], "ERROR");
    assert_eq!(missing["trigger"], "startup");
    assert_eq!(missing["stage"], "read");
    assert_eq!(missing["reason"], "not_found");
    assert!(missing["os_code"].is_i64());
    assert_eq!(entries[1]["event"], "tvi.start");
    assert_eq!(entries[1]["level"], "DEBUG");
    assert_eq!(entries[1]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(entries[2]["event"], "terminal.failed");
    assert_eq!(entries[2]["operation"], "preflight");
    assert_eq!(entries[2]["reason"], "interactive_terminal_required");
    assert_eq!(entries[2]["action"], "exit");
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn competing_process_does_not_wait_or_write_while_locked() -> io::Result<()> {
    let dir = Scratch::new()?;
    let log_dir = logs(&dir);
    fs::create_dir_all(&log_dir)?;
    let lock = fs::File::create(log_dir.join("tvi.log.lock"))?;
    lock.try_lock().map_err(io::Error::from)?;
    let blocked = run(&dir, &["missing.md"])?;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("cannot read"));
    assert!(!log_dir.join("tvi.log").exists());
    drop(lock);
    assert!(!run(&dir, &["missing.md"])?.status.success());
    assert!(log_dir.join("tvi.log").is_file());
    Ok(())
}
