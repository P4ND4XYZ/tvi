#![cfg(windows)]

use std::{
    fs,
    io::{self, Write as _},
    os::windows::{fs::MetadataExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

#[allow(
    dead_code,
    reason = "share the production Windows job helper; this test only assigns jobs"
)]
#[path = "../src/process/windows.rs"]
mod job;
#[allow(
    dead_code,
    reason = "the shared fixtures also serve renderer and extraction tests"
)]
#[path = "support/pdf.rs"]
mod pdf;

fn worker(dir: &Path, pdf: &Path, bounded: bool) -> io::Result<Output> {
    let binary = std::env::var_os("TVI_PDF_WORKER_TEST_EXE")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_tvi")), PathBuf::from);
    let metadata = fs::metadata(pdf)?;
    let packet = serde_json::to_vec(&serde_json::json!([
        [
            pdf,
            metadata.file_size(),
            metadata.creation_time(),
            metadata.last_write_time()
        ],
        1,
        "ハイ"
    ]))?;
    let job = bounded
        .then(|| job::Job::new(Some(256 * 1024 * 1024)))
        .transpose()?;
    let mut child = Command::new(binary)
        .arg("--internal-pdf-text")
        .creation_flags(CREATE_NO_WINDOW)
        .env("LOCALAPPDATA", dir.join("state"))
        .env("TVI_LOG", "debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let exchange = (|| -> io::Result<()> {
        if let Some(job) = &job {
            job.attach(&child)?;
        }
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing worker stdin"))?
            .write_all(&packet)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err(io::Error::other("PDF worker did not exit"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    })();
    if exchange.is_err() {
        drop(job);
        drop(child.kill());
        drop(child.wait());
        exchange?;
    }
    child.wait_with_output()
}

#[test]
fn pdf_extractor_entry_contains_parser_failure_and_keeps_stdio_and_logs_clean() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("synthetic.pdf");
    let widths = "65 66 500";
    // A standard predefined horizontal CMap not implemented by pdf-extract
    // 0.12.1. This exercises its unsupported-font panic without exotic input.
    fs::write(&path, pdf::cid_text_page(widths, "90ms-RKSJ-H", false))?;
    let failed = worker(dir.path(), &path, true)?;
    assert!(!failed.status.success());
    assert_ne!(
        failed.status.code(),
        Some(1),
        "fixture should reach the parser's panic, not fail argument validation"
    );
    assert!(failed.stdout.is_empty());
    assert!(failed.stderr.is_empty());

    fs::write(&path, pdf::cid_text_page(widths, "Identity-H", false))?;
    let unbounded = worker(dir.path(), &path, false)?;
    assert!(!unbounded.status.success());
    assert!(unbounded.stdout.is_empty() && unbounded.stderr.is_empty());

    let recovered = worker(dir.path(), &path, true)?;
    assert!(recovered.status.success(), "{}", recovered.status);
    assert_eq!(recovered.stdout.len(), 1 + 2 * (4 + 6 * 8));
    assert_eq!(recovered.stdout.first(), Some(&0));
    assert!(recovered.stderr.is_empty());
    assert!(!dir.path().join("state/tvi/logs").exists());
    Ok(())
}
