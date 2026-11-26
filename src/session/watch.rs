use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use notify_debouncer_mini::{
    Config, DebounceEventResult, DebouncedEventBatch, Debouncer, new_debouncer_opt,
    notify::{self, EventKindMask, RecommendedWatcher, RecursiveMode},
};

use crate::{
    app::Mode,
    logging::{self, Level},
};

use super::{
    ReadDocumentError, Session, document_from_markdown, log_document_failure, require_regular_file,
};

const DEBOUNCE: Duration = Duration::from_millis(150);
const RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_ATTEMPTS: u8 = 4;
const READ_NOTICE: &str = "文書を再読み込みできません";
const WATCH_NOTICE: &str = "文書の変更を監視できません";

#[derive(Default)]
struct Signals {
    changed: AtomicBool,
    parent_changed: AtomicBool,
    failed: AtomicBool,
}

struct Watch {
    _debouncer: Debouncer<RecommendedWatcher>,
    signals: Arc<Signals>,
    parent: PathBuf,
}

impl Watch {
    fn start(path: &Path) -> notify::Result<Self> {
        let absolute = std::path::absolute(path)?;
        let file_name = absolute
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?
            .to_owned();
        // Normalize the directory, not the file: the latter may be deleted or replaced.
        let parent = fs::canonicalize(absolute.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing parent directory")
        })?)?;
        let event_parent = parent.clone();
        let signals = Arc::new(Signals::default());
        let output = Arc::clone(&signals);
        // Ignore opens and read-only closes so our own reads do not trigger another reload.
        let config = Config::default().with_timeout(DEBOUNCE).with_notify_config(
            notify::Config::default()
                .with_event_kinds(EventKindMask::CORE | EventKindMask::ACCESS_CLOSE),
        );
        let mut debouncer = new_debouncer_opt(config, move |result: DebounceEventResult| {
            let changed = match result {
                Ok(DebouncedEventBatch::Events(events)) => {
                    let mut changed = false;
                    for event in events {
                        if event.path == event_parent {
                            output.parent_changed.store(true, Ordering::Relaxed);
                        } else if event.path.parent() == Some(event_parent.as_path())
                            && event
                                .path
                                .file_name()
                                .is_some_and(|name| same_name(name, &file_name))
                        {
                            changed = true;
                        }
                    }
                    changed
                }
                Ok(DebouncedEventBatch::Rescan) => true,
                Ok(_) => false,
                Err(_) => {
                    output.failed.store(true, Ordering::Relaxed);
                    false
                }
            };
            if changed {
                output.changed.store(true, Ordering::Relaxed);
            }
        })?;
        debouncer
            .watcher()
            .watch(&parent, RecursiveMode::NonRecursive)?;
        Ok(Self {
            _debouncer: debouncer,
            signals,
            parent,
        })
    }

    fn changed(&self) -> bool {
        self.signals.changed.swap(false, Ordering::Relaxed)
    }

    fn parent_lost(&self) -> bool {
        // Debounced paths have no event kind. A metadata change alone is not a failure.
        self.signals.parent_changed.swap(false, Ordering::Relaxed) && !self.parent.is_dir()
    }

    fn failed(&self) -> bool {
        self.signals.failed.swap(false, Ordering::Relaxed)
    }
}

fn same_name(left: &OsStr, right: &OsStr) -> bool {
    left == right
        || (cfg!(windows)
            && (left.eq_ignore_ascii_case(right)
                || left
                    .to_str()
                    .zip(right.to_str())
                    .is_some_and(|(left, right)| left.to_uppercase() == right.to_uppercase())))
}

#[derive(Default)]
pub(super) struct Reload {
    current: Option<u64>,
    watch: Option<Watch>,
    pending: Option<(Instant, u8)>,
    read_failed: bool,
    watch_failed: bool,
    watch_notice_pending: bool,
}

impl Reload {
    /// Called on the UI thread. Return true if a notice or the displayed document changed.
    pub(super) fn poll(&mut self, session: &mut Session, now: Instant, input_busy: bool) -> bool {
        if session.app.document().markdown().is_none() {
            *self = Self::default();
            return false;
        }
        let id = session.app.document_id();
        if self.current != Some(id) {
            // Each activation owns its own callback signals, even in the same directory.
            *self = Self {
                current: Some(id),
                ..Self::default()
            };
            // Check after registering, to cover saves while away or during the switch.
            self.pending = Some((now, 0));
            match Watch::start(session.app.document_path()) {
                Ok(watch) => self.watch = Some(watch),
                Err(_) => self.watch_failure(session.app.document_path(), "register"),
            }
        }
        if let Some(watch) = &self.watch {
            if watch.changed() {
                self.pending.get_or_insert((now, 0));
            }
            if watch.parent_lost() {
                self.watch = None;
                self.pending = None;
                self.watch_failure(session.app.document_path(), "receive");
            } else if watch.failed() {
                self.watch_failure(session.app.document_path(), "receive");
            }
        }
        if input_busy
            || session.app.mode() != Mode::Normal
            || session.app.help_open()
            || session.floats.active().is_some()
        {
            return false;
        }
        if self.watch_notice_pending {
            self.watch_notice_pending = false;
            session.app.set_notice(WATCH_NOTICE.into());
            return true;
        }
        let Some((due, attempts)) = self.pending else {
            return false;
        };
        if now < due {
            return false;
        }
        match self.read(session) {
            Ok(changed) => {
                self.pending = None;
                self.read_failed = false;
                let cleared = session.app.clear_notice_if(READ_NOTICE);
                cleared || changed
            }
            Err(_) if attempts + 1 < MAX_ATTEMPTS => {
                self.pending = Some((now + RETRY_DELAY, attempts + 1));
                false
            }
            Err(error) => {
                self.pending = None;
                if self.read_failed {
                    return false;
                }
                let path = session.app.document_path();
                match error {
                    ReloadError::Metadata(error) => log_document_failure(
                        path,
                        "reload",
                        "metadata",
                        logging::io_reason(&error),
                        error.raw_os_error(),
                    ),
                    ReloadError::Contents(error) => error.log(path, "reload"),
                }
                self.read_failed = true;
                session.app.set_notice(READ_NOTICE.into());
                true
            }
        }
    }

    fn watch_failure(&mut self, path: &Path, stage: &'static str) {
        if !self.watch_failed {
            self.watch_failed = true;
            self.watch_notice_pending = true;
            logging::emit(
                Level::Warn,
                "document.watch_failed",
                serde_json::json!({
                    "document": logging::basename(path), "stage": stage,
                }),
            );
        }
    }

    fn read(&mut self, session: &mut Session) -> Result<bool, ReloadError> {
        let path = session.app.document_path();
        require_regular_file(path).map_err(ReloadError::Metadata)?;
        let bytes = fs::read(path)
            .map_err(|error| ReloadError::Contents(ReadDocumentError::Read(error)))?;
        let markdown = String::from_utf8(bytes)
            .map_err(|error| ReloadError::Contents(ReadDocumentError::Decode(error)))?;
        if markdown == session.app.document().markdown().unwrap().text() {
            return Ok(false);
        }
        let replacement = document_from_markdown(path, markdown);
        session.clear_markdown();
        session.app.replace_document(replacement);
        let id = session.app.document_id();
        session.reload_positions = Some(id);
        self.current = Some(id);
        Ok(true)
    }
}

enum ReloadError {
    Metadata(io::Error),
    Contents(ReadDocumentError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{Action, Direction, ViewAnchor},
        lua::Runtime,
    };
    use std::{cell::RefCell, rc::Rc};
    use std::{error::Error, thread};

    fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + timeout;
        loop {
            if check() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_watcher_reports_consecutive_overwrites_and_replacements() -> Result<(), Box<dyn Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("表示.md");
        let tmp = dir.path().join("editor.tmp");
        fs::write(&path, "original")?;
        let mut session = Session::new(super::super::read_document(&path)?);
        let mut reload = Reload::default();
        assert!(!reload.poll(&mut session, Instant::now(), false));
        for (text, replace) in [
            ("one", false),
            ("two", true),
            ("three", true),
            ("four", false),
        ] {
            if replace {
                fs::write(&tmp, text)?;
                fs::rename(&tmp, &path)?;
            } else {
                fs::write(&path, text)?;
            }
            assert!(
                wait_until(Duration::from_secs(3), || {
                    reload.poll(&mut session, Instant::now(), false);
                    session.app.document().markdown().unwrap().text() == text
                }),
                "no reload for {text}"
            );
            assert!(session.app.notice().is_none());
            // Consume trailing events so each subsequent save needs a new notification.
            let id = session.app.document_id();
            wait_until(DEBOUNCE * 2, || {
                reload.poll(&mut session, Instant::now(), false)
            });
            assert_eq!(session.app.document_id(), id);
        }
        let watch = reload.watch.as_ref().ok_or("watch was not registered")?;
        fs::write(dir.path().join("other.md"), "unrelated")?;
        assert!(!wait_until(DEBOUNCE * 3, || watch.changed()));
        assert!(!watch.failed());
        Ok(())
    }

    #[test]
    fn reload_keeps_lua_document_active_but_does_not_retarget_link_ids()
    -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("source.md");
        fs::write(&path, "[Old](old.md)")?;
        let session = Rc::new(RefCell::new(Session::new(super::super::read_document(
            &path,
        )?)));
        let init = dir.path().join("init.lua");
        fs::write(
            &init,
            r#"
            local doc = tvi.ui.current_view():document()
            local path = assert(doc:path())
            local old = doc:links()[1].id
            assert(doc:resolve_link(old):find("old.md", 1, true))
            tvi.keymap.set("normal", "<F2>", function()
                assert(doc:type() == "md" and doc:path() == path)
                local target, err = doc:resolve_link(old)
                assert(target == nil and err)
                local links = doc:links()
                assert(#links == 1 and links[1].label == "New")
                assert(doc:resolve_link(links[1].id):find("new.md", 1, true))
                doc:scroll_to_line(1)
            end)
            "#,
        )?;
        let (runtime, diagnostic) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(diagnostic, None);
        fs::write(path, "[New](new.md)")?;
        Reload::default()
            .read(&mut session.borrow_mut())
            .map_err(|_| io::Error::other("reload failed"))?;
        runtime.run_mapping(1)?;
        Ok(())
    }

    #[test]
    fn history_reads_changes_and_refreshes_search_without_adding_history() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = dir.path().join("first.md");
        let second = dir.path().join("second.md");
        fs::write(&first, "needle")?;
        fs::write(&second, "other")?;
        let mut session = Session::new(super::super::read_document(&first)?);
        let mut reload = Reload::default();
        let now = Instant::now();
        let old_id = session.app.document_id();
        assert!(!reload.poll(&mut session, now, false));
        assert_eq!(session.app.document_id(), old_id);
        let rows = [ViewAnchor::text(0)];
        session
            .app
            .apply(Action::StartSearch(Direction::Forward), &rows, "needle");
        for ch in "needle".chars() {
            session.app.apply(Action::Insert(ch), &rows, "needle");
        }
        session.app.apply(Action::Submit, &rows, "needle");
        session.app.apply(Action::StartCommand, &rows, "");
        for ch in "noh".chars() {
            session.app.apply(Action::Insert(ch), &rows, "");
        }
        session.app.apply(Action::Submit, &rows, "");
        assert!(session.app.highlights().0.is_empty());
        session
            .app
            .open_document(super::super::read_document(&second)?);
        assert!(!reload.poll(&mut session, now, false));
        fs::write(&first, "needle needle [link](other.md)")?;
        session.app.apply(Action::JumpBack, &rows, "");
        assert!(reload.poll(&mut session, now, false));
        assert_ne!(session.app.document_id(), old_id);
        drop(session.display());
        assert_eq!(session.app.search_query(), "needle");
        assert_eq!(session.app.search_selection(), Some((1, 2)));
        assert!(session.app.highlights().0.is_empty());
        assert_eq!(session.app.links()[0].target, "other.md");
        let id = session.app.document_id();
        session.app.apply(Action::JumpBack, &rows, "");
        assert_eq!(session.app.document_id(), id);
        Ok(())
    }

    #[test]
    fn removing_the_watched_parent_reports_monitoring_failure_once() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let parent = dir.path().join("watched");
        fs::create_dir(&parent)?;
        let path = parent.join("file.md");
        fs::write(&path, "old")?;
        let mut session = Session::new(super::super::read_document(&path)?);
        let mut reload = Reload::default();
        assert!(!reload.poll(&mut session, Instant::now(), false));
        let watch = reload
            .watch
            .as_ref()
            .ok_or_else(|| io::Error::other("watch missing"))?;
        watch.signals.parent_changed.store(true, Ordering::Relaxed);
        assert!(!reload.poll(&mut session, Instant::now(), false));
        fs::remove_dir_all(&parent)?;
        assert!(wait_until(Duration::from_secs(3), || {
            reload.poll(&mut session, Instant::now(), false);
            reload.watch.is_none() && session.app.notice() == Some(WATCH_NOTICE)
        }));
        assert_eq!(session.app.document().markdown().unwrap().text(), "old");
        session.app.clear_notice_if(WATCH_NOTICE);
        assert!(!wait_until(DEBOUNCE * 3, || reload.poll(
            &mut session,
            Instant::now(),
            false
        )));
        assert!(session.app.notice().is_none());
        Ok(())
    }

    #[test]
    fn invalid_utf8_keeps_old_contents_and_reports_only_after_retries() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file.md");
        fs::write(&path, "old")?;
        let mut session = Session::new(super::super::read_document(&path)?);
        let mut reload = Reload::default();
        let start = Instant::now();
        assert!(!reload.poll(&mut session, start, false));
        fs::write(&path, [0xff])?;
        reload.pending = Some((start, 0));
        for attempt in 0..MAX_ATTEMPTS {
            let now = start + RETRY_DELAY * u32::from(attempt);
            assert_eq!(
                reload.poll(&mut session, now, false),
                attempt + 1 == MAX_ATTEMPTS
            );
            assert_eq!(session.app.document().markdown().unwrap().text(), "old");
        }
        assert_eq!(session.app.notice(), Some(READ_NOTICE));
        Ok(())
    }

    #[test]
    fn failed_reload_keeps_old_document_and_recovers_when_notified() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("file.md");
        fs::write(&path, "old")?;
        let mut session = Session::new(super::super::read_document(&path)?);
        let mut reload = Reload::default();
        let now = Instant::now();
        assert!(!reload.poll(&mut session, now, false));
        fs::remove_file(&path)?;
        assert!(wait_until(Duration::from_secs(3), || reload.poll(
            &mut session,
            Instant::now(),
            false
        )));
        assert_eq!(session.app.document().markdown().unwrap().text(), "old");
        assert_eq!(session.app.notice(), Some(READ_NOTICE));
        let rows = [ViewAnchor::text(0)];
        session.app.apply(Action::StartCommand, &rows, "old");
        session.app.apply(Action::Insert('q'), &rows, "old");
        // A pending notification must not replace the document during command input.
        fs::write(&path, "new")?;
        assert!(wait_until(Duration::from_secs(3), || {
            reload.poll(&mut session, Instant::now(), false);
            reload.pending.is_some()
        }));
        assert_eq!(session.app.document().markdown().unwrap().text(), "old");
        assert_eq!(session.app.prompt(), "q");
        session.app.apply(Action::Cancel, &rows, "old");
        assert!(wait_until(Duration::from_secs(3), || reload.poll(
            &mut session,
            Instant::now(),
            false
        )));
        assert_eq!(session.app.document().markdown().unwrap().text(), "new");
        assert!(session.app.notice().is_none());
        Ok(())
    }
}
