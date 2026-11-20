#[path = "../src/mermaid.rs"]
mod mermaid;

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::Cursor,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use image::{ImageFormat, ImageReader};
use mermaid::{CacheIssue, Failure, Limits, Mermaid, Stage, TimeoutScope};
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    bin: PathBuf,
    work: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new(renderer: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let work = root.path().join("work");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&work).unwrap();
        let fixture = Self {
            bin,
            work,
            cache: root.path().join("cache"),
            _root: root,
        };
        if renderer {
            fixture.install();
        }
        fixture
    }

    fn executable(&self) -> PathBuf {
        self.bin
            .join(if cfg!(windows) { "mmdc.exe" } else { "mmdc" })
    }

    fn install(&self) {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_mmdc.rs");
        let mut command = Command::new(rustc);
        command
            .args(["--edition=2024", "--crate-name=fake_mmdc"])
            .arg(source)
            .arg("-o")
            .arg(self.executable())
            .current_dir(&self.bin);
        let output = bounded_output(&mut command);
        assert!(
            output.status.success(),
            "fake compiler: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn options(&self, capacity: u64) -> Mermaid {
        Mermaid::isolated(
            env::join_paths([&self.bin]).unwrap(),
            self.work.clone(),
            self.cache.clone(),
            capacity,
        )
    }

    fn entries(&self) -> Vec<PathBuf> {
        self.cache
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "png"))
            .collect()
    }

    fn assert_clean(&self) {
        assert!(self.work.read_dir().unwrap().next().is_none());
    }
}

fn bounded_output(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            drop(child.wait());
            panic!("fake compiler exceeded its test deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn pixel(png: &[u8]) -> Option<[u8; 3]> {
    let reader = ImageReader::with_format(Cursor::new(png), ImageFormat::Png);
    Some(reader.decode().ok()?.to_rgb8().get_pixel(0, 0).0)
}

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];

#[test]
fn missing_renderer_does_not_consult_an_existing_cache() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096);
    assert!(options.start_document().convert("red", pixel).cache.saved);
    fs::remove_file(fixture.executable()).unwrap();
    let result = options.start_document().convert("red", |_| -> Option<()> {
        panic!("cache was consulted without a renderer")
    });
    assert_eq!(result.result, Err(Failure::Unavailable));
    assert_eq!(result.cache, Default::default());
    fixture.assert_clean();
}

#[test]
fn output_and_exit_failures_do_not_prevent_later_diagrams() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096);
    let mut document = options.start_document();

    assert_eq!(document.convert("red", pixel).result, Ok(RED));
    for (source, failure) in [
        ("exit", Failure::Exit { code: Some(17) }),
        ("missing", Failure::MissingOutput),
        ("invalid", Failure::InvalidPng),
    ] {
        let conversion = document.convert(source, pixel);
        assert_eq!(conversion.result, Err(failure));
        assert!(!conversion.cache.saved);
        fixture.assert_clean();
    }
    assert_eq!(document.convert("blue", pixel).result, Ok(BLUE));
    assert_eq!(fixture.entries().len(), 2);
    assert_clean_launch_failure(&fixture, document);
}

fn assert_clean_launch_failure(fixture: &Fixture, mut document: mermaid::Document<'_>) {
    fs::remove_file(fixture.executable()).unwrap();
    assert_eq!(
        document.convert("fresh", pixel).result.unwrap_err().stage(),
        Stage::Launch
    );
    assert_eq!(
        document.convert("next", pixel).result,
        Err(Failure::Unavailable)
    );
    fixture.assert_clean();
}

#[test]
fn deadline_cleans_process_and_workspace_and_limits_the_document() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096).with_limits(Limits {
        per_diagram: Duration::from_millis(1100),
        per_document: Duration::from_millis(2000),
    });
    let mut document = options.start_document();

    for scope in [TimeoutScope::Diagram, TimeoutScope::Document] {
        let conversion = document.convert("timeout", pixel);
        assert_eq!(conversion.result, Err(Failure::Timeout(scope)));
        assert!(conversion.elapsed < Duration::from_secs(5));
        assert_process_stopped(&fixture.work, "running", "ready");
        assert_process_stopped(&fixture.work, "descendant-running", "descendant-ready");
        fixture.assert_clean();
    }
    assert_eq!(
        document.convert("red", pixel).result,
        Err(Failure::Timeout(TimeoutScope::Document))
    );
    assert!(!fixture.cache.exists());
}

#[cfg(unix)]
#[test]
fn unix_term_allows_cooperative_cleanup_and_kills_unresponsive_group() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096).with_limits(Limits {
        per_diagram: Duration::from_secs(1),
        per_document: Duration::from_secs(4),
    });
    let mut document = options.start_document();
    for (marker, cooperative) in [("cooperative", true), ("unresponsive", false)] {
        let result = document.convert(marker, pixel);
        assert_eq!(result.result, Err(Failure::Timeout(TimeoutScope::Diagram)));
        assert!(result.elapsed < Duration::from_secs(2));
        assert_process_stopped(&fixture.work, "running", "ready");
        assert_process_stopped(&fixture.work, "descendant-running", "descendant-ready");
        let notification = fixture.work.join("term-handled");
        assert_eq!(notification.is_file(), cooperative);
        if cooperative {
            fs::remove_file(notification).unwrap();
        }
        fixture.assert_clean();
    }
}

#[test]
fn slow_decode_cannot_return_success_or_start_work_after_deadline() {
    let fixture = Fixture::new(true);
    let normal = fixture.options(4096);
    assert!(normal.start_document().convert("red", pixel).cache.saved);
    let short = fixture.options(4096).with_limits(Limits {
        per_diagram: Duration::from_secs(2),
        per_document: Duration::from_millis(70),
    });
    let mut document = short.start_document();
    let mut decoded = false;
    let cached = document.convert("red", |png| {
        decoded = true;
        thread::sleep(Duration::from_millis(95));
        pixel(png)
    });
    assert_eq!(cached.result, Err(Failure::Timeout(TimeoutScope::Document)));
    assert!(decoded);
    assert!(!cached.cache.hit);
    assert_eq!(
        document
            .convert("blue", |_| -> Option<()> {
                panic!("expired document decoded another diagram")
            })
            .result,
        Err(Failure::Timeout(TimeoutScope::Document))
    );

    let short = fixture.options(4096).with_limits(Limits {
        per_diagram: Duration::from_millis(700),
        per_document: Duration::from_secs(4),
    });
    let mut document = short.start_document();
    let mut decoded = false;
    let fresh = document.convert("blue", |png| {
        decoded = true;
        assert_eq!(pixel(png), Some(BLUE));
        thread::sleep(Duration::from_millis(750));
        Some(BLUE)
    });
    assert_eq!(fresh.result, Err(Failure::Timeout(TimeoutScope::Diagram)));
    assert!(decoded);
    assert!(!fresh.cache.saved);
    assert_eq!(fixture.entries().len(), 1);
    assert!(document.convert("different diagram", pixel).cache.saved);
    fixture.assert_clean();
}

#[test]
fn cache_saves_the_bytes_that_were_decoded_even_if_the_file_changes() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096);
    let result = options.start_document().convert("red", |png| {
        let output = fixture
            .work
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("output.png");
        fs::write(output, b"replaced after the read").unwrap();
        pixel(png)
    });
    assert_eq!(result.result, Ok(RED));
    assert!(result.cache.saved);
    let reused = options.start_document().convert("red", pixel);
    assert_eq!(reused.result, Ok(RED));
    assert!(reused.cache.hit);
    fixture.assert_clean();
}

fn assert_process_stopped(work: &Path, lock_name: &str, ready_name: &str) {
    assert!(work.join(ready_name).is_file(), "{lock_name} never started");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(work.join(lock_name))
        .unwrap();
    let started = Instant::now();
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if started.elapsed() < Duration::from_secs(6) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{lock_name} did not release its lock: {error}"),
        }
    }
    drop(lock);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{lock_name} outlived the timeout"
    );
    fs::remove_file(work.join(ready_name)).unwrap();
    fs::remove_file(work.join(lock_name)).unwrap();
}

#[test]
fn cache_hits_across_sessions_and_regenerates_invalid_entries() {
    let fixture = Fixture::new(true);
    let first = fixture.options(4096).start_document().convert("red", pixel);
    assert_eq!(first.result, Ok(RED));
    assert!(first.cache.saved && !first.cache.hit);

    let options = fixture.options(4096);
    let mut next = options.start_document();
    let hit = next.convert("red", pixel);
    assert_eq!(hit.result, Ok(RED));
    assert!(hit.cache.hit && !hit.cache.saved);

    let entry = fixture.entries().pop().unwrap();
    fs::write(&entry, b"not-a-png").unwrap();
    let repaired = next.convert("red", pixel);
    assert_eq!(repaired.result, Ok(RED));
    assert_eq!(repaired.cache.lookup_issue, Some(CacheIssue::InvalidEntry));
    assert!(repaired.cache.saved);

    let changed = next.convert("blue", pixel);
    assert_eq!(changed.result, Ok(BLUE));
    assert!(!changed.cache.hit && changed.cache.saved);
    assert_eq!(fixture.entries().len(), 2);
    fixture.assert_clean();
}

#[test]
fn cache_pressure_evicts_oldest_by_size_but_not_on_read() {
    let fixture = Fixture::new(true);
    let options = fixture.options(4096);
    let mut seed = options.start_document();
    assert!(seed.convert("red", pixel).cache.saved);
    let oldest = fixture.entries().pop().unwrap();
    assert!(seed.convert("blue", pixel).cache.saved);
    let size = oldest.metadata().unwrap().len();
    let now = SystemTime::now();
    for entry in fixture.entries() {
        let age = if entry == oldest { 48 } else { 24 };
        File::options()
            .write(true)
            .open(entry)
            .unwrap()
            .set_modified(now - Duration::from_secs(age * 3600))
            .unwrap();
    }

    let options = fixture.options(size * 2);
    let mut document = options.start_document();
    assert!(document.convert("red", pixel).cache.hit);
    assert_eq!(fixture.entries().len(), 2);
    let newer = fixture
        .entries()
        .into_iter()
        .find(|path| path != &oldest)
        .unwrap();
    fs::write(&newer, vec![0; size as usize]).unwrap();
    let repair = document.convert("blue", pixel);
    assert_eq!(repair.cache.lookup_issue, Some(CacheIssue::InvalidEntry));
    assert!(repair.cache.saved);
    assert!(
        oldest.exists(),
        "replacing an entry needlessly evicted another"
    );
    assert!(document.convert("red again", pixel).cache.saved);
    assert!(!oldest.exists());
    assert!(document.convert("blue", pixel).cache.hit);
    assert_eq!(fixture.entries().len(), 2);
    let total: u64 = fixture
        .entries()
        .iter()
        .map(|path| path.metadata().unwrap().len())
        .sum();
    assert_eq!(total, size * 2);
}

#[test]
fn unusable_cache_and_write_contention_do_not_discard_rendered_images() {
    let fixture = Fixture::new(true);
    fs::write(&fixture.cache, b"not a directory").unwrap();
    let options = fixture.options(4096);
    let mut document = options.start_document();
    let blocked = document.convert("red", pixel);
    assert_eq!(blocked.result, Ok(RED));
    assert!(matches!(
        blocked.cache.save_issue,
        Some(CacheIssue::Write(_))
    ));

    fs::remove_file(&fixture.cache).unwrap();
    fs::create_dir(&fixture.cache).unwrap();
    let lock = File::create(fixture.cache.join(".lock")).unwrap();
    lock.lock().unwrap();
    let busy = document.convert("red", pixel);
    assert_eq!(busy.result, Ok(RED));
    assert_eq!(busy.cache.save_issue, Some(CacheIssue::Busy));
    assert!(fixture.entries().is_empty());
    drop(lock);

    let too_small = fixture.options(1).start_document().convert("red", pixel);
    assert_eq!(too_small.result, Ok(RED));
    assert_eq!(too_small.cache.save_issue, Some(CacheIssue::Capacity));
    assert!(fixture.entries().is_empty());
    fixture.assert_clean();
}

#[test]
fn production_paths_are_resolved_in_an_isolated_process() {
    const MODE: &str = "TVI_TEST_MERMAID_PRODUCTION_PATHS";
    if let Some(mode) = env::var_os(MODE) {
        let options = Mermaid::from_env();
        if mode == "relative" {
            assert_eq!(
                options.start_document().convert("blue", pixel).result,
                Err(Failure::Unavailable)
            );
        } else {
            let result = options.start_document().convert("blue", pixel);
            assert_eq!(result.result, Ok(BLUE));
            assert!(result.cache.saved);
        }
        return;
    }

    let fixture = Fixture::new(true);
    let root = fixture._root.path();
    let absolute_path = env::join_paths([&fixture.bin]).unwrap();
    for (mode, path, xdg, cache_base) in [
        (
            "absolute",
            absolute_path.clone(),
            root.join("xdg"),
            Some(root.join("xdg")),
        ),
        (
            "fallback",
            absolute_path,
            PathBuf::from("relative-xdg"),
            Some(root.join("home/.cache")),
        ),
        ("relative", ".".into(), PathBuf::from("relative-xdg"), None),
    ] {
        let local = root.join("local");
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "production_paths_are_resolved_in_an_isolated_process",
                "--nocapture",
            ])
            .current_dir(&fixture.bin)
            .env(MODE, mode)
            .env("PATH", path)
            .env("TMP", &fixture.work)
            .env("TEMP", &fixture.work)
            .env("TMPDIR", &fixture.work)
            .env("LOCALAPPDATA", &local)
            .env("XDG_CACHE_HOME", xdg)
            .env("HOME", root.join("home"));
        let output = bounded_output(&mut command);
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        #[cfg(windows)]
        let cache_base = cache_base.map(|_| local);
        if let Some(base) = cache_base {
            let target = base.join("tvi/cache/mermaid");
            assert!(target.is_dir(), "expected isolated cache in {target:?}");
            fs::remove_dir_all(target).unwrap();
        }
        assert!(!fixture.bin.join("relative-xdg").exists());
        fixture.assert_clean();
    }
}

#[cfg(windows)]
#[test]
fn windows_npm_style_cmd_shim_accepts_fixed_arguments() {
    let fixture = Fixture::new(true);
    fs::rename(fixture.executable(), fixture.bin.join("renderer.exe")).unwrap();
    fs::write(
        fixture.bin.join("mmdc.cmd"),
        b"@echo off\r\n\"%~dp0renderer.exe\" %*\r\n",
    )
    .unwrap();
    let conversion = fixture
        .options(4096)
        .start_document()
        .convert("blue", pixel);
    assert_eq!(conversion.result, Ok(BLUE));
    assert!(conversion.cache.saved);
    fixture.assert_clean();
}
