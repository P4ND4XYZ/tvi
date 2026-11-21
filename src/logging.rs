//! Best-effort local diagnostics. Event fields must be selected by the caller;
//! never pass raw error messages, user content, or paths other than a basename.

mod rotation;

use std::{
    env, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Iso8601};

pub(crate) const MAX_RECORD_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) enum Level {
    Debug,
    Warn,
    Error,
}

static LOGGER: OnceLock<Background> = OnceLock::new();
const QUEUE_CAPACITY: usize = 64;
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

type Event = (Level, &'static str, Value);

struct Background {
    sender: Mutex<Option<SyncSender<Event>>>,
    done: Mutex<Receiver<()>>,
    notice: Arc<AtomicU8>,
    debug: bool,
}

impl Background {
    fn start(debug: bool, make_writer: impl FnOnce() -> Logger + Send + 'static) -> Self {
        let (sender, queue) = mpsc::sync_channel::<Event>(QUEUE_CAPACITY);
        let (finished, done) = mpsc::channel();
        let notice = Arc::new(AtomicU8::new(0));
        let worker_notice = Arc::clone(&notice);
        let worker = std::thread::Builder::new()
            .name("tvi-log".into())
            .spawn(move || {
                // Even resolving the home directory belongs off the UI thread.
                let writer = make_writer();
                for (level, event, fields) in queue {
                    if writer
                        .write(level, event, fields, OffsetDateTime::now_utc)
                        .is_err()
                    {
                        mark_failure(&worker_notice);
                    }
                }
                drop(finished.send(()));
            });
        if worker.is_err() {
            mark_failure(&notice);
        }
        Self {
            sender: Mutex::new(Some(sender)),
            done: Mutex::new(done),
            notice,
            debug,
        }
    }

    fn enqueue(&self, event: Event) {
        if matches!(event.0, Level::Debug) && !self.debug {
            return;
        }
        if !self.sender.try_lock().is_ok_and(|sender| {
            sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(event).is_ok())
        }) {
            mark_failure(&self.notice);
        }
    }

    fn shutdown(&self, timeout: std::time::Duration) {
        if let Ok(mut sender) = self.sender.try_lock() {
            drop(sender.take());
            drop(sender);
            if let Ok(done) = self.done.try_lock() {
                drop(done.recv_timeout(timeout));
            }
        }
    }
}

fn mark_failure(notice: &AtomicU8) {
    let _previous = notice.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed);
}

fn take_notice(notice: &AtomicU8) -> bool {
    notice
        .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

struct Logger {
    directory: Option<PathBuf>,
    debug: bool,
    #[cfg(test)]
    notice: AtomicU8,
}

pub(crate) fn init() {
    LOGGER.get_or_init(|| {
        let debug = env::var("TVI_LOG").as_deref() == Ok("debug");
        Background::start(debug, move || Logger::new(directory(), debug))
    });
}

pub(crate) fn shutdown() {
    if let Some(logger) = LOGGER.get() {
        logger.shutdown(FLUSH_TIMEOUT);
    }
}

fn directory() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    #[cfg(target_os = "linux")]
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::home_dir()
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        });
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let base: Option<PathBuf> = None;
    base.map(|base| base.join("tvi").join("logs"))
}

#[cfg(test)]
std::thread_local! {
    static TEST_LOGGER: std::cell::RefCell<Option<Logger>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn capture_for_test<T>(directory: &Path, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_LOGGER.set(None);
        }
    }
    assert!(TEST_LOGGER.with_borrow(Option::is_none));
    TEST_LOGGER.set(Some(Logger::new(Some(directory.to_owned()), false)));
    let _reset = Reset;
    run()
}

/// A structured event with a static name and an object of privacy-reviewed fields.
pub(crate) fn emit(level: Level, event: &'static str, fields: Value) {
    #[cfg(test)]
    let Some(fields) = TEST_LOGGER.with_borrow(|logger| match logger {
        Some(logger) => {
            logger.emit(level, event, fields, OffsetDateTime::now_utc);
            None
        }
        None => Some(fields),
    }) else {
        return;
    };

    if let Some(logger) = LOGGER.get() {
        logger.enqueue((level, event, fields));
    }
}

pub(crate) fn take_failure_notice() -> bool {
    LOGGER
        .get()
        .is_some_and(|logger| take_notice(&logger.notice))
}

pub(crate) fn basename(path: &Path) -> String {
    path.file_name().map_or_else(
        || "(unknown)".into(),
        |name| {
            name.to_string_lossy()
                .chars()
                .take(255)
                .map(|c| if c.is_control() { '�' } else { c })
                .collect()
        },
    )
}

pub(crate) fn io_reason(error: &io::Error) -> &'static str {
    use io::ErrorKind as Kind;
    match error.kind() {
        Kind::NotFound => "not_found",
        Kind::PermissionDenied => "permission_denied",
        Kind::InvalidInput => "invalid_input",
        Kind::InvalidData => "invalid_data",
        Kind::AlreadyExists => "already_exists",
        Kind::TimedOut => "timed_out",
        Kind::Interrupted => "interrupted",
        Kind::WouldBlock => "would_block",
        Kind::BrokenPipe => "broken_pipe",
        Kind::Unsupported => "unsupported",
        _ => "io_error",
    }
}

impl Logger {
    fn new(directory: Option<PathBuf>, debug: bool) -> Self {
        Self {
            directory,
            debug,
            #[cfg(test)]
            notice: AtomicU8::new(0),
        }
    }

    #[cfg(test)]
    fn emit(
        &self,
        level: Level,
        event: &str,
        fields: Value,
        clock: impl FnOnce() -> OffsetDateTime,
    ) {
        if self.write(level, event, fields, clock).is_err() {
            let _previous =
                self.notice
                    .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    fn take_failure_notice(&self) -> bool {
        self.notice
            .compare_exchange(1, 2, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn write(
        &self,
        level: Level,
        event: &str,
        fields: Value,
        clock: impl FnOnce() -> OffsetDateTime,
    ) -> io::Result<()> {
        let level = match level {
            Level::Debug if !self.debug => return Ok(()),
            Level::Debug => "DEBUG",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        };
        let directory = self
            .directory
            .as_deref()
            .ok_or_else(|| io::Error::other("no log directory"))?;
        rotation::append(directory, || encode(clock(), level, event, fields))
    }
}

const TS_CONFIG: u128 = time::format_description::well_known::iso8601::Config::DEFAULT
    .set_time_precision(
        time::format_description::well_known::iso8601::TimePrecision::Second {
            decimal_digits: std::num::NonZeroU8::new(3),
        },
    )
    .encode();

fn encode(
    now: OffsetDateTime,
    level: &str,
    event: &str,
    fields: Value,
) -> io::Result<(String, Vec<u8>)> {
    let ts = now
        .to_utc()
        .format(&Iso8601::<TS_CONFIG>)
        .map_err(io::Error::other)?;
    let date = ts[..10].to_owned();
    let mut record = serde_json::Map::from_iter([
        ("ts".into(), ts.into()),
        ("level".into(), level.into()),
        ("event".into(), event.into()),
        ("pid".into(), std::process::id().into()),
    ]);
    let mut line = serde_json::to_vec(&record)?;
    if line.len() >= MAX_RECORD_BYTES {
        return Err(io::Error::other("oversized event name"));
    }
    if let Value::Object(fields) = fields {
        let mut oversized = Vec::new();
        for (key, value) in fields {
            if record.contains_key(&key) {
                continue;
            }
            record.insert(key.clone(), value);
            let candidate = serde_json::to_vec(&record)?;
            if candidate.len() < MAX_RECORD_BYTES {
                line = candidate;
            } else if let Some(Value::String(text)) = record.remove(&key) {
                oversized.push((key, text));
            }
        }
        // Fit long strings after smaller fields, so a long label cannot crowd out metadata.
        for (key, text) in oversized {
            record.insert(key.clone(), Value::String(String::new()));
            let overhead = serde_json::to_vec(&record)?.len();
            if let Some(short) = (MAX_RECORD_BYTES - 1)
                .checked_sub(overhead)
                .and_then(|available| shorten(&text, available))
            {
                record.insert(key, short.into());
                line = serde_json::to_vec(&record)?;
            } else {
                record.remove(&key);
            }
        }
    }
    line.push(b'\n');
    Ok((date, line))
}

fn shorten(text: &str, available: usize) -> Option<String> {
    const ELLIPSIS: &str = "…";
    let mut unused = available.checked_sub(ELLIPSIS.len())?;
    let mut prefix = text;
    for (index, c) in text.char_indices() {
        let bytes = match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\x08' | '\x0c' => 2,
            '\0'..='\x1f' => 6,
            _ => c.len_utf8(),
        };
        let Some(remaining) = unused.checked_sub(bytes) else {
            prefix = &text[..index];
            break;
        };
        unused = remaining;
    }
    Some(format!("{prefix}{ELLIPSIS}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use time::{Duration, UtcOffset};

    fn time() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(1_234_567_890).unwrap()
    }

    fn logger(directory: &Path, debug: bool) -> Logger {
        Logger::new(Some(directory.to_owned()), debug)
    }

    #[test]
    fn filters_debug_without_creating_logs_and_bounds_utf8_json() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tvi.log");
        logger(dir.path(), false).write(Level::Debug, "quiet", json!({}), time)?;
        assert!(!path.exists());
        assert!(fs::read_dir(dir.path())?.next().is_none());
        let clock = || time().to_offset(UtcOffset::from_hms(9, 0, 0).unwrap());
        logger(dir.path(), true).write(
            Level::Debug,
            "visible",
            json!({
                "document":"文書.md", "large":"界\n".repeat(4096), "level":"override",
            }),
            clock,
        )?;
        let content = fs::read_to_string(path)?;
        assert!(content.len() <= 4096);
        assert!(content.ends_with('\n'));
        let mut record: serde_json::Map<String, Value> = serde_json::from_str(content.trim_end())?;
        let large = record.remove("large").unwrap();
        let large = large.as_str().unwrap().strip_suffix('…').unwrap();
        assert!(large.len() > 1000 && "界\n".repeat(4096).starts_with(large));
        assert_eq!(
            Value::Object(record),
            json!({
                "ts":"1970-01-01T00:00:01.234Z", "event":"visible", "pid":std::process::id(),
                "level":"DEBUG", "document":"文書.md",
            })
        );
        Ok(())
    }

    #[test]
    fn rotates_at_utc_day_change_and_keeps_five_archives() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let logger = logger(dir.path(), false);
        for day in 0..8 {
            logger.write(Level::Warn, "same_day", json!({"day":day}), || {
                time() + Duration::days(day)
            })?;
            logger.write(Level::Error, "still_same_day", json!({"day":day}), || {
                time() + Duration::days(day) + Duration::hours(23)
            })?;
        }
        for (index, day) in (2..8).rev().enumerate() {
            let name = if index == 0 {
                "tvi.log".to_owned()
            } else {
                format!("tvi.log.{index}")
            };
            let text = fs::read_to_string(dir.path().join(name))?;
            let records: Vec<Value> = text
                .lines()
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            assert_eq!(records.len(), 2);
            assert_eq!(records[0]["day"], day);
            assert_eq!(records[1]["day"], day);
        }
        assert!(!dir.path().join("tvi.log.6").exists());
        Ok(())
    }

    #[test]
    fn rotates_only_when_next_line_exceeds_size_limit() -> io::Result<()> {
        const LIMIT: usize = 1_048_576;
        let dir = tempfile::tempdir()?;
        let active = dir.path().join("tvi.log");
        let history = dir.path().join("tvi.log.1");
        let logger = logger(dir.path(), false);
        let (_, record) = encode(time(), "WARN", "test", json!({}))?;
        // Fill the file with complete valid records, leaving exactly one upcoming record's space.
        let remaining = LIMIT - record.len();
        let mut fixture = record.repeat(remaining / record.len() - 1);
        let filler = " ".repeat(remaining - fixture.len() - record.len());
        fixture.extend_from_slice(filler.as_bytes());
        fixture.extend_from_slice(&record);
        fs::write(&active, fixture)?;
        logger.write(Level::Warn, "test", json!({}), time)?;
        assert_eq!(fs::metadata(&active)?.len(), LIMIT as u64);
        assert!(!history.exists());
        logger.write(Level::Warn, "test", json!({}), time)?;
        assert_eq!(fs::metadata(history)?.len(), LIMIT as u64);
        assert_eq!(fs::read(active)?, record);
        Ok(())
    }

    #[test]
    fn an_unwritable_log_requests_only_one_notice_and_leaves_the_path_unchanged() -> io::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let not_a_directory = dir.path().join("file");
        fs::write(&not_a_directory, "unchanged")?;
        let logger = logger(&not_a_directory, true);
        assert!(!logger.take_failure_notice());
        logger.emit(Level::Warn, "failed", json!({}), time);
        assert!(logger.take_failure_notice());
        logger.emit(Level::Warn, "failed_again", json!({}), time);
        assert!(!logger.take_failure_notice());
        assert_eq!(fs::read_to_string(not_a_directory)?, "unchanged");
        Ok(())
    }

    #[test]
    fn non_log_content_and_partial_records_are_untouched() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tvi.log");
        let logger = logger(dir.path(), false);
        let (_, valid) = encode(time(), "WARN", "before", json!({}))?;
        let mut partial = valid;
        partial.extend_from_slice(b"{\"ts\":");
        for data in [b"unrelated file".as_slice(), b"{}\n", b"", &partial] {
            fs::write(&path, data)?;
            assert!(logger.write(Level::Warn, "after", json!({}), time).is_err());
            assert_eq!(fs::read(&path)?, data);
        }
        Ok(())
    }

    #[test]
    fn hard_link_to_even_a_valid_log_is_rejected() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let original = dir.path().join("original");
        let (_, valid) = encode(time(), "WARN", "before", json!({}))?;
        fs::write(&original, &valid)?;
        fs::hard_link(&original, dir.path().join("tvi.log"))?;
        assert!(
            logger(dir.path(), false)
                .write(Level::Warn, "after", json!({}), time)
                .is_err()
        );
        assert_eq!(fs::read(original)?, valid);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn failed_active_move_preserves_history_then_retry_rotates_once() -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir()?;
        let logger = logger(dir.path(), false);
        for day in 0..6 {
            logger.write(Level::Warn, "before", json!({"day":day}), || {
                time() + Duration::days(day)
            })?;
        }
        let active = dir.path().join("tvi.log");
        let blocked = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&active)?;
        for _ in 0..2 {
            assert!(
                logger
                    .write(Level::Warn, "after", json!({}), || time()
                        + Duration::days(6))
                    .is_err()
            );
            for i in 1..=5 {
                let value: Value =
                    serde_json::from_slice(&fs::read(dir.path().join(format!("tvi.log.{i}")))?)?;
                assert_eq!(value["day"], 5 - i);
            }
        }
        drop(blocked);
        logger.write(Level::Warn, "after", json!({}), || {
            time() + Duration::days(6)
        })?;
        let oldest: Value = serde_json::from_slice(&fs::read(dir.path().join("tvi.log.5"))?)?;
        assert_eq!(oldest["day"], 1);
        Ok(())
    }

    #[test]
    fn partially_shifted_history_resumes_without_a_second_eviction() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let at = |name| dir.path().join(name);
        let logger = logger(dir.path(), false);
        for day in 0..6 {
            logger.write(Level::Warn, "before", json!({"day":day}), || {
                time() + Duration::days(day)
            })?;
        }
        fs::rename(at("tvi.log"), at("tvi.log.pending"))?;
        fs::remove_file(at("tvi.log.5"))?;
        fs::rename(at("tvi.log.4"), at("tvi.log.5"))?;
        logger.write(Level::Warn, "after", json!({}), || {
            time() + Duration::days(6)
        })?;
        assert!(!at("tvi.log.pending").exists());
        for i in 1..=5 {
            let path = dir.path().join(format!("tvi.log.{i}"));
            let value: Value = serde_json::from_slice(&fs::read(path)?)?;
            assert_eq!(value["day"], 6 - i);
        }
        Ok(())
    }

    #[test]
    fn stalled_writer_does_not_block_enqueue_or_shutdown() -> io::Result<()> {
        use std::time::Duration as StdDuration;
        let dir = tempfile::tempdir()?;
        let directory = dir.path().to_owned();
        let (release, stall) = mpsc::channel();
        let background = Background::start(false, move || {
            drop(stall.recv_timeout(StdDuration::from_secs(2)));
            Logger::new(Some(directory), false)
        });
        for _ in 0..=QUEUE_CAPACITY {
            background.enqueue((Level::Warn, "stalled", json!({})));
        }
        background.shutdown(StdDuration::from_millis(10));
        assert!(!dir.path().join("tvi.log").exists());
        assert!(take_notice(&background.notice));
        release.send(()).map_err(io::Error::other)?;
        background
            .done
            .lock()
            .unwrap()
            .recv_timeout(StdDuration::from_secs(2))
            .map_err(io::Error::other)?;
        assert_eq!(
            fs::read_to_string(dir.path().join("tvi.log"))?
                .lines()
                .count(),
            QUEUE_CAPACITY
        );
        Ok(())
    }
}
