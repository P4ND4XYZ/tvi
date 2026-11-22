use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
    app::{App, Document, Mode, Motion, ViewRows},
    logging::{self, Level},
    pdf::{Failure, Page, Source},
    terminal::{self, ImageMode, Screen},
    ui::{
        float::Floats,
        layout::{Layout, PdfGeometry as Geometry},
    },
};

use super::log_document_failure;

pub(super) type PlacedPage = Rc<Layout<'static>>;

pub(super) fn geometry(columns: u16, mode: ImageMode) -> Result<Geometry, Failure> {
    if !cfg!(windows) {
        return Err(Failure::platform());
    }
    match mode {
        ImageMode::Kitty {
            cell_width,
            cell_height,
        } if cell_width > 0 && cell_height > 0 => Ok(Geometry {
            columns: columns.max(1),
            cell_width,
            cell_height,
        }),
        _ => Err(Failure::terminal()),
    }
}

fn fit(page: &Page, geometry: Geometry) -> Result<Layout<'static>, Failure> {
    let image = page.decode()?;
    Layout::pdf_page(&image, page.aspect_height(), geometry).ok_or_else(|| {
        Failure::new(
            "pdf_layout",
            "display_size",
            "PDF page is too large to display at this width",
        )
    })
}

type ResultPage = Result<(Page, Result<Layout<'static>, Failure>), Failure>;

fn try_result(receiver: &Receiver<ResultPage>) -> Option<ResultPage> {
    match receiver.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Disconnected) => Some(Err(Failure::worker())),
        Err(TryRecvError::Empty) => None,
    }
}

enum Work {
    Open(PathBuf),
    Turn(Source, u32),
    Fit(Page),
}

impl Work {
    fn place(self, geometry: Geometry) -> ResultPage {
        let page = match self {
            Self::Open(path) => Page::open(&path)?,
            Self::Turn(source, index) => Page::render(source, index)?,
            Self::Fit(page) => page,
        };
        let layout = fit(&page, geometry);
        Ok((page, layout))
    }

    fn spawn(self, geometry: Geometry) -> Result<Receiver<ResultPage>, Failure> {
        let (send, receive) = mpsc::channel();
        thread::Builder::new()
            .name("tvi-pdf".into())
            .spawn(move || drop(send.send(self.place(geometry))))
            .map_err(|_| Failure::worker())?;
        Ok(receive)
    }
}

#[derive(PartialEq, Eq)]
enum Intent {
    Open {
        from: u64,
        mode: Mode,
        path: PathBuf,
        float: Option<(u64, u64)>,
    },
    Turn {
        from: (u64, u32),
        target: u32,
        previous: bool,
        motion: Motion,
    },
    Fit {
        from: (u64, u32),
    },
}

impl Intent {
    fn document_id(&self) -> u64 {
        match self {
            Self::Open { from, .. } => *from,
            Self::Turn { from, .. } | Self::Fit { from } => from.0,
        }
    }

    fn still_current(&self, app: &App, floats: &Floats) -> bool {
        match self {
            Self::Open {
                from, mode, float, ..
            } => {
                *from == app.document_id()
                    && *float == floats.active()
                    && app.mode() == *mode
                    && !app.help_open()
            }
            Self::Turn { from, .. } => {
                Some(*from) == page_key(app)
                    && floats.active().is_none()
                    && app.mode() == Mode::Normal
                    && !app.help_open()
            }
            Self::Fit { from } => Some(*from) == page_key(app),
        }
    }
}

struct Request {
    intent: Intent,
    work: Work,
}

struct Running {
    // Dropping the intent cancels publication, but the native call may still be completing.
    intent: Option<Intent>,
    geometry: Geometry,
    result: Receiver<ResultPage>,
}

#[derive(Default)]
pub(super) struct Jobs {
    queued: Option<Request>,
    running: Option<Running>,
    placed: Option<Placement>,
    failed_fit: Option<((u64, u32), Geometry)>,
    deferred_motion: Option<((u64, u32), Motion)>,
}

struct Placement {
    key: (u64, u32),
    geometry: Geometry,
    layout: PlacedPage,
}

fn page_key(app: &App) -> Option<(u64, u32)> {
    app.pdf().map(|page| (app.document_id(), page.index))
}

impl Jobs {
    pub(super) fn placed(&self, app: &App) -> Option<PlacedPage> {
        self.placed
            .as_ref()
            .filter(|placed| Some(placed.key) == page_key(app))
            .map(|placed| Rc::clone(&placed.layout))
    }

    pub(super) fn depart(&mut self, document: u64) {
        self.cancel_if(|intent| intent.document_id() == document);
        if self
            .placed
            .as_ref()
            .is_some_and(|placed| placed.key.0 == document)
        {
            self.placed = None;
        }
        self.failed_fit = None;
        self.deferred_motion = None;
    }

    pub(super) fn open(&mut self, app: &App, path: &Path, float: Option<(u64, u64)>) {
        let intent = Intent::Open {
            from: app.document_id(),
            mode: app.mode(),
            path: path.to_owned(),
            float,
        };
        self.deferred_motion = None;
        if self.pending() != Some(&intent) {
            self.replace(Request {
                intent,
                work: Work::Open(path.to_owned()),
            });
        }
    }

    fn matching_layout(
        &self,
        app: &App,
        geometry: Result<Geometry, Failure>,
    ) -> Option<PlacedPage> {
        let geometry = geometry.ok()?;
        let placed = self.placed.as_ref()?;
        (Some(placed.key) == page_key(app) && placed.geometry == geometry)
            .then(|| Rc::clone(&placed.layout))
    }

    pub(super) fn move_pdf(
        &mut self,
        app: &mut App,
        motion: Motion,
        geometry: Result<Geometry, Failure>,
    ) -> bool {
        let Some(from) = page_key(app) else {
            return false;
        };
        if app.mode() != Mode::Normal || app.help_open() {
            return false;
        }
        if let Some(layout) = self.matching_layout(app, geometry) {
            self.deferred_motion = None;
            self.apply_motion(app, motion, layout.as_ref());
        } else {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            if geometry.is_ok_and(|geometry| self.failed_fit != Some((from, geometry))) {
                // Keep the latest motion across a refit, not an edge decision from the old view.
                self.deferred_motion = Some((from, motion));
            }
        }
        true
    }

    fn apply_motion(&mut self, app: &mut App, motion: Motion, rows: &impl ViewRows) {
        let Some(page) = app.pdf() else { return };
        let Some((target, previous)) = app.pdf_turn(motion, rows) else {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            app.apply(crate::app::Action::Move(motion), rows, "");
            return;
        };
        let intent = Intent::Turn {
            from: (app.document_id(), page.index),
            target,
            previous,
            motion,
        };
        if self.pending() != Some(&intent) {
            self.replace(Request {
                intent,
                work: Work::Turn(page.source.clone(), target),
            });
        }
    }

    fn revalidate_turn(&mut self, app: &App, floats: &Floats, geometry: Result<Geometry, Failure>) {
        let can_move = |key| {
            Some(key) == page_key(app)
                && floats.active().is_none()
                && app.mode() == Mode::Normal
                && !app.help_open()
        };
        self.deferred_motion = self.deferred_motion.filter(|&(key, _)| can_move(key));
        let layout = self.matching_layout(app, geometry);
        if let Some(&Intent::Turn {
            from,
            motion,
            target,
            previous,
        }) = self.pending()
            && layout.as_ref().is_none_or(|layout| {
                app.pdf_turn(motion, layout.as_ref()) != Some((target, previous))
            })
        {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            self.deferred_motion = Some((from, motion));
        }
    }

    fn replay_resized_motion(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
    ) -> bool {
        if let Some(layout) = self.matching_layout(app, geometry)
            && let Some((_, motion)) = self.deferred_motion.take()
        {
            app.reflow_current_height(layout.as_ref());
            self.apply_motion(app, motion, layout.as_ref());
            return true;
        }
        false
    }

    fn pending(&self) -> Option<&Intent> {
        self.queued
            .as_ref()
            .map(|r| &r.intent)
            .or_else(|| self.running.as_ref()?.intent.as_ref())
    }

    fn cancel_if(&mut self, should_cancel: impl Fn(&Intent) -> bool) {
        if self
            .queued
            .as_ref()
            .is_some_and(|r| should_cancel(&r.intent))
        {
            self.queued = None;
        }
        if let Some(running) = &mut self.running
            && running.intent.as_ref().is_some_and(should_cancel)
        {
            running.intent = None;
        }
    }

    fn replace(&mut self, request: Request) {
        self.cancel_if(|_| true);
        self.queued = Some(request);
    }

    /// Poll and publish on the UI thread. At most one native/encoding job runs at a time;
    /// waiting UI requests coalesce to the latest intent.
    pub(super) fn poll(
        &mut self,
        app: &mut App,
        floats: &Floats,
        geometry: Result<Geometry, Failure>,
    ) -> Update {
        let key = page_key(app);
        if self
            .placed
            .as_ref()
            .is_some_and(|placed| Some(placed.key) != key)
        {
            self.placed = None;
        }
        if self
            .failed_fit
            .is_some_and(|(failed, _)| Some(failed) != key)
            || geometry.is_ok_and(|geometry| {
                self.failed_fit
                    .is_some_and(|(_, failed)| failed != geometry)
            })
        {
            self.failed_fit = None;
        }
        self.cancel_if(|intent| !intent.still_current(app, floats));

        self.revalidate_turn(app, floats, geometry);
        let mut update = Update::default();
        if let Some(running) = &mut self.running
            && let Some(result) = try_result(&running.result)
        {
            let intent = running.intent.take();
            let placed_at = running.geometry;
            self.running = None;
            if let Some(intent) = intent {
                update = match (geometry, result) {
                    (Ok(latest), Ok((page, _))) if placed_at != latest => {
                        self.queued = Some(Request {
                            intent,
                            work: Work::Fit(page),
                        });
                        Update::default()
                    }
                    (Ok(_), Ok((page, Ok(layout)))) => {
                        self.commit(app, intent, page, placed_at, layout)
                    }
                    (_, Err(error) | Ok((_, Err(error)))) => {
                        self.fail(app, intent, error, Some(placed_at))
                    }
                    (Err(error), Ok(_)) => self.fail(app, intent, error, None),
                };
            }
        }

        update.redraw |= self.replay_resized_motion(app, geometry);
        if self.pending().is_none()
            && let (Some(key), Some(page), Ok(geometry)) = (page_key(app), app.pdf(), geometry)
            && !self
                .placed
                .as_ref()
                .is_some_and(|placed| placed.key == key && placed.geometry == geometry)
            && self.failed_fit != Some((key, geometry))
        {
            self.queued = Some(Request {
                intent: Intent::Fit { from: key },
                work: Work::Fit(page.clone()),
            });
        }
        if self.running.is_none()
            && let Some(Request { intent, work }) = self.queued.take()
        {
            match geometry
                .and_then(|geometry| work.spawn(geometry).map(|result| (geometry, result)))
            {
                Ok((geometry, result)) => {
                    self.running = Some(Running {
                        intent: Some(intent),
                        geometry,
                        result,
                    });
                }
                Err(error) => return self.fail(app, intent, error, geometry.ok()),
            }
        }
        update
    }

    fn commit(
        &mut self,
        app: &mut App,
        intent: Intent,
        page: Page,
        geometry: Geometry,
        layout: Layout<'static>,
    ) -> Update {
        let close_float = match intent {
            Intent::Open { path, float, .. } => {
                app.open_document(Document::pdf(path, page));
                float
            }
            Intent::Turn { previous, .. } => {
                app.turn_pdf_page(page, previous);
                app.clear_notice_if(TURN_NOTICE);
                None
            }
            Intent::Fit { .. } => {
                app.clear_notice_if(FIT_NOTICE);
                None
            }
        };
        if let Some(key) = page_key(app) {
            self.placed = Some(Placement {
                key,
                geometry,
                layout: Rc::new(layout),
            });
        }
        self.failed_fit = None;
        Update {
            redraw: true,
            close_float,
        }
    }

    fn fail(
        &mut self,
        app: &mut App,
        intent: Intent,
        failure: Failure,
        geometry: Option<Geometry>,
    ) -> Update {
        match intent {
            Intent::Open { path, .. } => {
                log_open_failure(&path, "open_file", failure);
                app.set_notice(failure.message.into());
            }
            Intent::Turn { target, .. } => {
                log_page_failure(app.document_path(), target, "turn", failure);
                app.set_notice(TURN_NOTICE.into());
            }
            Intent::Fit { from } => {
                self.failed_fit = geometry.map(|geometry| (from, geometry));
                self.deferred_motion = None;
                log_page_failure(app.document_path(), from.1, "resize", failure);
                app.set_notice(FIT_NOTICE.into());
            }
        }
        Update {
            redraw: true,
            close_float: None,
        }
    }

    fn initial(app: &App, geometry: Geometry, layout: Layout<'static>) -> Self {
        Self {
            placed: page_key(app).map(|key| Placement {
                key,
                geometry,
                layout: Rc::new(layout),
            }),
            ..Self::default()
        }
    }
}

const TURN_NOTICE: &str = "Could not display the requested PDF page";
const FIT_NOTICE: &str = "Could not fit the PDF page to this window";

#[derive(Default)]
pub(super) struct Update {
    pub redraw: bool,
    pub close_float: Option<(u64, u64)>,
}

pub(super) fn log_open_failure(path: &Path, trigger: &'static str, failure: Failure) {
    log_document_failure(path, trigger, failure.stage, failure.reason, failure.code);
}

fn log_page_failure(path: &Path, index: u32, trigger: &'static str, failure: Failure) {
    let mut fields = serde_json::json!({
        "document":logging::basename(path), "page":index + 1, "trigger":trigger,
        "stage":failure.stage, "reason":failure.reason,
    });
    if let Some(code) = failure.code {
        fields["os_code"] = code.into();
    }
    logging::emit(Level::Warn, "pdf.page_failed", fields);
}

// At startup there is no previous document or Lua view to publish into yet.
// Use the same open-and-place worker while allowing the user to cancel loading.
pub(super) fn startup(
    screen: &mut Screen,
    input: &mut terminal::Input,
    path: PathBuf,
) -> io::Result<Option<(App, Jobs)>> {
    let mode = input.image_mode();
    let mut size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
    let report = |failure: &Failure| log_open_failure(&path, "startup", *failure);
    let mut placed_at = geometry(size.width, mode).inspect_err(report)?;
    let mut receiver = Work::Open(path.clone())
        .spawn(placed_at)
        .inspect_err(report)?;

    loop {
        let mode = input.image_mode();
        let latest_size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
        if latest_size != size {
            size = latest_size;
            input.refresh_cell_size();
        }
        if let Some(result) = try_result(&receiver) {
            let (page, layout) = result.inspect_err(report)?;
            let latest = geometry(size.width, mode).inspect_err(report)?;
            if placed_at == latest {
                let layout = layout.inspect_err(report)?;
                let app = App::from_document(Document::pdf(path, page));
                let jobs = Jobs::initial(&app, placed_at, layout);
                return Ok(Some((app, jobs)));
            }
            // Keep the native raster; only the placement needs to match the new width.
            placed_at = latest;
            receiver = Work::Fit(page).spawn(placed_at).inspect_err(report)?;
        }
        terminal::check(
            screen.draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("Opening PDF… (Esc to cancel)"),
                    frame.area(),
                );
            }),
            "draw",
            Some(mode),
            "exit",
        )?;
        if input.poll_key()?.is_some_and(|key| {
            key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        }) {
            return Ok(None);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use crate::{
        app::{Action, Direction},
        lua::{FileOpen, Host, Runtime},
        session::{BODY_VIEW, InputQueue, Session, poll_pdf},
        ui::float::FloatOptions,
    };
    use crossterm::event::KeyEvent;
    use std::cell::RefCell;

    use super::*;

    const KITTY: ImageMode = ImageMode::Kitty {
        cell_width: 2,
        cell_height: 4,
    };
    const FIRST: Geometry = Geometry {
        columns: 80,
        cell_width: 2,
        cell_height: 4,
    };

    fn test_session() -> Rc<RefCell<Session>> {
        Rc::new(RefCell::new(Session::with_images(
            Document::new(
                "original.md".into(),
                "Original".into(),
                "original.md".into(),
                vec![],
            ),
            KITTY,
            None,
        )))
    }

    fn wait(session: &RefCell<Session>, runtime: &Runtime) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            poll_pdf(session, runtime);
            let jobs = &session.borrow().pdf;
            if jobs.running.is_none() && jobs.queued.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PDF worker did not finish",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn place(session: &mut Session, height: usize) -> io::Result<PlacedPage> {
        let placed = session
            .pdf
            .placed(&session.app)
            .ok_or_else(|| io::Error::other("PDF was not placed"))?;
        session.app.reflow(placed.as_ref(), height);
        Ok(placed)
    }

    struct Demo {
        session: Rc<RefCell<Session>>,
        runtime: Runtime,
        original: u64,
        directory: tempfile::TempDir,
    }

    impl Demo {
        fn open() -> Result<Self, Box<dyn std::error::Error>> {
            Self::open_at(FIRST)
        }

        fn open_at(geometry: Geometry) -> Result<Self, Box<dyn std::error::Error>> {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("manual.pdf");
            fs::write(&path, crate::test_pdf::two_pages())?;
            let session = test_session();
            session.borrow_mut().pdf_geometry = Ok(geometry);
            let original = session.borrow().app.document_id();
            drop(session.borrow_mut().display()); // same Markdown cache as in a live session
            let (runtime, error) = Runtime::load_from(session.clone(), None);
            assert!(error.is_none());
            assert!(matches!(
                session.open_file(BODY_VIEW, &path)?,
                FileOpen::Pending
            ));
            assert_eq!(session.borrow().app.document_id(), original);
            wait(&session, &runtime)?;
            assert!(
                session.borrow().prepared.is_none(),
                "history must not reuse the old Markdown display"
            );
            Ok(Self {
                session,
                runtime,
                original,
                directory,
            })
        }

        fn page(&self) -> io::Result<PlacedPage> {
            wait(&self.session, &self.runtime)?;
            place(&mut self.session.borrow_mut(), 24)
        }

        fn move_by(&self, motion: Motion, page: &Layout<'static>) {
            self.session
                .borrow_mut()
                .apply(Action::Move(motion), page, "");
        }
    }

    #[test]
    fn scroll_during_width_refit_cannot_turn_from_the_old_layout()
    -> Result<(), Box<dyn std::error::Error>> {
        let narrow = Geometry {
            columns: 24,
            ..FIRST
        };
        let demo = Demo::open_at(narrow)?;
        let old = place(&mut demo.session.borrow_mut(), 79)?;
        assert_eq!(old.len(), 48);
        demo.session.borrow_mut().pdf_geometry = Ok(FIRST);
        demo.move_by(Motion::LineDown, &old);
        wait(&demo.session, &demo.runtime)?;
        let current = place(&mut demo.session.borrow_mut(), 79)?;
        assert_eq!(current.len(), 160);
        assert_eq!(
            page_key(&demo.session.borrow().app).map(|key| key.1),
            Some(0)
        );
        assert_eq!(demo.session.borrow().app.top_row(), 1);
        Ok(())
    }

    #[test]
    fn resize_rechecks_even_an_already_completed_turn_before_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        for geometry_changes in [false, true] {
            let narrow = Geometry {
                columns: 24,
                ..FIRST
            };
            let demo = Demo::open_at(narrow)?;
            let old = place(&mut demo.session.borrow_mut(), 79)?;
            demo.move_by(Motion::LineDown, &old);
            {
                let mut state = demo.session.borrow_mut();
                let request = state.pdf.queued.take().ok_or("expected turn")?;
                assert!(matches!(request.intent, Intent::Turn { .. }));
                // Deliver an already prepared result to make this race deterministic.
                let (send, result) = mpsc::channel();
                assert!(send.send(request.work.place(narrow)).is_ok());
                state.pdf.running = Some(Running {
                    intent: Some(request.intent),
                    geometry: narrow,
                    result,
                });
                if geometry_changes {
                    state.pdf_geometry = Ok(FIRST);
                } else {
                    state.app.reflow(old.as_ref(), 24);
                }
            }
            wait(&demo.session, &demo.runtime)?;
            assert_eq!(
                page_key(&demo.session.borrow().app).map(|key| key.1),
                Some(0)
            );
            assert_eq!(demo.session.borrow().app.top_row(), 1);
        }
        Ok(())
    }

    #[test]
    fn tall_page_turns_only_after_reaching_the_edge_and_does_not_add_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        let id = demo.session.borrow().app.document_id();
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 0)));
        assert_eq!(first.len(), 160);
        assert!(demo.session.borrow().app.links().is_empty());
        demo.move_by(Motion::LineUp, &first);
        assert!(demo.session.borrow().pdf.pending().is_none());
        for top in [24, 48, 72, 96, 120, 136] {
            demo.move_by(Motion::PageDown, &first);
            let state = demo.session.borrow();
            assert_eq!(state.app.top_row(), top);
            assert!(
                state.pdf.pending().is_none(),
                "reaching the bottom must not turn the page"
            );
            assert!(state.app.notice().is_none());
        }
        demo.move_by(Motion::LineDown, &first);
        demo.move_by(Motion::LineDown, &first);
        let second = demo.page()?;
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 1)));
        assert_eq!(demo.session.borrow().app.top_row(), 0);

        demo.move_by(Motion::End, &second);
        demo.move_by(Motion::PageDown, &second);
        assert!(demo.session.borrow().pdf.pending().is_none());
        assert!(demo.session.borrow().app.notice().is_none());

        demo.move_by(Motion::Start, &second);
        demo.move_by(Motion::LineUp, &second);
        let first = demo.page()?;
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 0)));
        assert_eq!(demo.session.borrow().app.top_row(), 136);
        demo.session
            .borrow_mut()
            .apply(Action::JumpBack, first.as_ref(), "");
        assert_eq!(
            demo.session.borrow().app.document_id(),
            demo.original,
            "turns must not create history entries"
        );
        Ok(())
    }

    #[test]
    fn resize_and_document_history_restore_the_position_in_the_current_pdf_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        let id = demo.session.borrow().app.document_id();
        demo.move_by(Motion::End, &first);
        demo.move_by(Motion::LineDown, &first);
        let second = demo.page()?;
        assert_eq!(second.len(), 80);
        demo.move_by(Motion::PageDown, &second);
        demo.move_by(Motion::LineDown, &second);
        assert_eq!(demo.session.borrow().app.top_row(), 25);

        demo.session.borrow_mut().pdf_geometry = Ok(Geometry {
            columns: 40,
            ..FIRST
        });
        let narrow = demo.page()?;
        assert_eq!(narrow.len(), 40);
        assert_eq!(demo.session.borrow().app.top_row(), 12);

        let other = demo.directory.path().join("other.md");
        fs::write(&other, "Other Markdown")?;
        assert!(matches!(
            demo.session.open_file(BODY_VIEW, &other)?,
            FileOpen::Opened
        ));
        let mut state = demo.session.borrow_mut();
        state.pdf.depart(id);
        state.pdf_geometry = Ok(FIRST);
        state.apply(Action::JumpBack, &[], "");
        assert_eq!(page_key(&state.app), Some((id, 1)));
        drop(state);

        let restored = demo.page()?;
        let mut state = demo.session.borrow_mut();
        assert_eq!(state.app.top_row(), 25);
        state.apply(
            Action::StartSearch(Direction::Forward),
            restored.as_ref(),
            "",
        );
        assert_eq!(state.app.mode(), Mode::Normal);
        assert_eq!(state.app.notice(), Some("Search is not supported for PDFs"));
        Ok(())
    }

    #[test]
    fn failed_and_cancelled_requests_do_not_publish_and_turn_can_retry()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let private = dir.path().join("private-pdf-directory");
        fs::create_dir(&private)?;
        let logs = dir.path().join("logs");
        let good = private.join("good.pdf");
        let bad = private.join("bad.pdf");
        fs::write(&good, crate::test_pdf::two_pages())?;
        fs::write(&bad, "PDF content must not be logged")?;
        let state = test_session();
        let original = state.borrow().app.document_id();
        let (runtime, error) = Runtime::load_from(state.clone(), None);
        assert!(error.is_none());
        let float = state.open_float(BODY_VIEW, FloatOptions::default())?;

        logging::capture_for_test(&logs, || -> Result<(), Box<dyn std::error::Error>> {
            assert!(matches!(
                state.open_file(BODY_VIEW, &bad)?,
                FileOpen::Pending
            ));
            wait(&state, &runtime)?;
            Ok(())
        })?;
        assert_eq!(state.borrow().floats.active(), Some((BODY_VIEW, float)));
        assert_eq!(state.borrow().app.document_id(), original);
        assert!(state.borrow().app.notice().is_some());

        assert!(matches!(
            state.open_file(BODY_VIEW, &good)?,
            FileOpen::Pending
        ));
        poll_pdf(&state, &runtime); // hand off to worker before cancelling the originating float
        state.close_float(BODY_VIEW, float)?;
        wait(&state, &runtime)?;
        assert_eq!(state.borrow().app.document_id(), original);

        assert!(matches!(
            state.open_file(BODY_VIEW, &good)?,
            FileOpen::Pending
        ));
        wait(&state, &runtime)?;
        let first = place(&mut state.borrow_mut(), 24)?;
        let id = state.borrow().app.document_id();
        {
            let mut session = state.borrow_mut();
            session.apply(Action::Move(Motion::End), first.as_ref(), "");
            session.apply(Action::Move(Motion::LineDown), first.as_ref(), "");
            // Substitute only the worker's result, so failure is deterministic even on
            // Windows installations that cache both pages in the native PDF object.
            let intent = session
                .pdf
                .queued
                .take()
                .ok_or_else(|| io::Error::other("no turn requested"))?
                .intent;
            let (send, result) = mpsc::channel();
            send.send(Err(Failure::worker()))
                .map_err(|_| io::Error::other("worker disconnected"))?;
            session.pdf.running = Some(Running {
                intent: Some(intent),
                geometry: FIRST,
                result,
            });
        }
        logging::capture_for_test(&logs, || {
            assert!(poll_pdf(&state, &runtime));
            assert!(!poll_pdf(&state, &runtime));
        });
        assert_eq!(page_key(&state.borrow().app), Some((id, 0)));
        assert_eq!(state.borrow().app.top_row(), 136);
        assert_eq!(state.borrow().app.notice(), Some(TURN_NOTICE));
        state
            .borrow_mut()
            .apply(Action::Move(Motion::LineDown), first.as_ref(), "");
        wait(&state, &runtime)?;
        assert_eq!(page_key(&state.borrow().app), Some((id, 1)));
        assert!(state.borrow().app.notice().is_none());

        let content = fs::read_to_string(logs.join("tvi.log"))?;
        assert!(!content.contains("private-pdf-directory"));
        assert!(!content.contains("PDF content must not be logged"));
        let records = content
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "document.open_failed");
        assert_eq!(records[0]["trigger"], "open_file");
        assert_eq!(records[0]["document"], "bad.pdf");
        assert_eq!(records[0]["level"], "WARN");
        assert_eq!(records[1]["event"], "pdf.page_failed");
        assert_eq!(records[1]["trigger"], "turn");
        assert_eq!(records[1]["document"], "good.pdf");
        assert_eq!(records[1]["page"], 2);
        Ok(())
    }

    #[test]
    #[ignore = "requires fd or fdfind on PATH"]
    fn bundled_ff_selects_pdf_and_only_closes_after_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("manual.PDF");
        let markdown = dir.path().join("start.md");
        fs::write(&path, crate::test_pdf::two_pages())?;
        fs::write(&markdown, "# Original")?;
        fs::create_dir(dir.path().join(".git"))?;
        let state = Rc::new(RefCell::new(Session::with_images(
            super::super::read_document(&markdown)?,
            KITTY,
            None,
        )));
        let (runtime, error) = Runtime::load_from(state.clone(), None);
        assert!(error.is_none());
        let mut keys = InputQueue::default();
        let press = |keys: &mut InputQueue, code| {
            keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &state,
                &runtime,
                &[],
                "",
            );
        };
        press(&mut keys, KeyCode::Char('f'));
        press(&mut keys, KeyCode::Char('f'));
        let float = state
            .borrow()
            .floats
            .active()
            .ok_or_else(|| io::Error::other("picker not open"))?;
        press(&mut keys, KeyCode::Enter);
        assert_eq!(state.borrow().floats.active(), Some(float));
        assert_eq!(state.borrow().app.document_path(), markdown);
        assert!(
            state.borrow().pdf.pending().is_some(),
            "PDF was not requested"
        );
        wait(&state, &runtime)?;
        assert!(
            state.borrow().floats.active().is_none(),
            "{:?}",
            state.borrow().app.notice()
        );
        assert_eq!(
            state.borrow().app.document_path().canonicalize()?,
            path.canonicalize()?
        );
        assert!(state.borrow().app.pdf().is_some());
        // Run another Lua mapping to make sure cleanup also released the Lua handle.
        press(&mut keys, KeyCode::Char('f'));
        press(&mut keys, KeyCode::Char('f'));
        assert!(state.borrow().floats.active().is_some());
        press(&mut keys, KeyCode::Esc);
        Ok(())
    }
}
