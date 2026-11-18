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
    let mut child = Command::new(env!("CARGO_BIN_EXE_tvi"))
        .current_dir(&dir.0)
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
