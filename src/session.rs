use std::{
    cell::RefCell,
    collections::VecDeque,
    fs, io,
    path::{self, Path, PathBuf},
    rc::Rc,
    string::FromUtf8Error,
    time::{Duration, Instant},
};

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::{
    app::{Action, App, Document, Mode, ViewRows},
    files,
    logging::{self, Level},
    lua::{FileOpen, Host, LuaLink, Runtime},
    markdown::{MarkdownRenderer, tui_markdown::TuiMarkdown},
    pdf::{self as pdf_format, Failure as PdfFailure},
    terminal::{self, ImageMode, Screen},
    ui::{
        self,
        float::{FloatOptions, Floats},
        input::{Input, InputAction, is_escape, plugin_key},
        layout::{Layout, PdfGeometry},
    },
};

#[cfg(test)]
use crate::images;

mod mermaid;
mod pdf;
mod watch;

const BODY_VIEW: u64 = 1;

struct Session {
    app: App,
    floats: Floats,
    image_mode: ImageMode,
    renderer: Option<crate::mermaid::Mermaid>,
    prepared: Option<(u64, Rc<mermaid::PreparedDisplay>)>,
    reload_positions: Option<u64>,
    source_scroll: Option<(u64, usize)>,
    pdf: pdf::Jobs,
    pdf_geometry: Result<PdfGeometry, PdfFailure>,
}

#[derive(Default)]
struct InputQueue {
    input: Input,
    replay: VecDeque<KeyEvent>,
    undecided_key: Option<Instant>,
}

impl Session {
    #[cfg(test)]
    fn new(document: Document) -> Self {
        Self::with_images(document, ImageMode::Text, None)
    }

    #[cfg(test)]
    fn with_images(
        document: Document,
        image_mode: ImageMode,
        renderer: Option<crate::mermaid::Mermaid>,
    ) -> Self {
        Self::with_app(App::from_document(document), image_mode, renderer)
    }

    fn with_app(
        app: App,
        image_mode: ImageMode,
        renderer: Option<crate::mermaid::Mermaid>,
    ) -> Self {
        Self {
            app,
            floats: Floats::default(),
            image_mode,
            renderer,
            prepared: None,
            reload_positions: None,
            source_scroll: None,
            pdf: pdf::Jobs::default(),
            pdf_geometry: pdf::geometry(80, image_mode),
        }
    }

    fn apply(&mut self, action: Action, rows: &impl ViewRows, text: &str) {
        if let Action::Move(motion) = action
            && self.pdf.move_pdf(&mut self.app, motion, self.pdf_geometry)
        {
            return;
        }
        self.app.apply(action, rows, text);
    }

    fn display(&mut self) -> Rc<mermaid::PreparedDisplay> {
        let id = self.app.document_id();
        if let Some((current, prepared)) = &self.prepared
            && *current == id
        {
            return Rc::clone(prepared);
        }
        self.prepared = None;
        let prepared = Rc::new(mermaid::prepare(
            &mut self.app,
            self.image_mode,
            self.renderer.as_ref(),
        ));
        if self.reload_positions.take() == Some(id) {
            self.app
                .refresh_reloaded_positions(prepared.document.search_text());
        }
        self.prepared = Some((id, Rc::clone(&prepared)));
        prepared
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>, layout: &Layout<'_>, runtime: &Runtime) {
        let body = ui::body_area(frame.area(), self.app.mode());
        self.app.reflow(layout, usize::from(body.height));
        if let Some((id, line)) = self.source_scroll.take()
            && id == self.app.document_id()
            && let Some(row) = layout.row_for_source_line(line)
        {
            self.app.center_row(layout, row);
        }
        let scheme = runtime.colorscheme();
        ui::draw(
            frame,
            &self.app,
            layout,
            self.floats.active().is_some(),
            &scheme,
        );
        self.floats.draw(frame, body, &scheme);
    }

    fn sync_pdf_layout(
        &mut self,
        width: u16,
        mode: ImageMode,
        active: &mut Option<pdf::PlacedPage>,
        retired: &mut Vec<u32>,
    ) {
        self.pdf_geometry = pdf::geometry(width, mode);
        let latest = self.pdf.placed(&self.app);
        if active
            .as_ref()
            .zip(latest.as_ref())
            .is_some_and(|(old, new)| Rc::ptr_eq(old, new))
        {
            return;
        }
        if let Some(old) = std::mem::replace(active, latest) {
            retired.extend(old.transmitted_ids());
        }
    }

    fn show_display_notice(&mut self, markdown: Option<&mermaid::PreparedDisplay>) {
        if self.app.mode() == Mode::Normal && self.app.notice().is_none() {
            if let Some(markdown) = markdown {
                mermaid::show_notice(&mut self.app, markdown);
            }
            if self.app.notice().is_none() && logging::take_failure_notice() {
                self.app.set_notice("ログを保存できません".into());
            }
        }
    }

    fn body_uninterrupted(&self) -> bool {
        self.app.mode() == Mode::Normal && !self.app.help_open() && self.floats.active().is_none()
    }

    fn restore_after_init_error(&mut self, error: &str) {
        self.source_scroll = None;
        self.pdf.reset();
        self.app.reset_to_initial_document();
        self.floats = Floats::default();
        self.show_lua_error(error);
    }

    fn show_lua_error(&mut self, error: &str) {
        let message = error.lines().next().unwrap_or_default();
        self.app.set_notice(message.replace(char::is_control, "�"));
    }
}

enum ReadDocumentError {
    Read(io::Error),
    Decode(FromUtf8Error),
}

pub fn read_document(path: &Path) -> io::Result<Document> {
    load_document(path).map_err(|error| {
        error.log(path, "startup");
        let label = path.to_string_lossy();
        let label = label.escape_debug();
        match error {
            ReadDocumentError::Read(error) => {
                io::Error::new(error.kind(), format!("cannot read {label}: {error}"))
            }
            ReadDocumentError::Decode(error) => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{label} is not valid UTF-8: {error}"),
            ),
        }
    })
}

impl ReadDocumentError {
    fn log(&self, path: &Path, trigger: &'static str) {
        match self {
            Self::Read(error) => log_document_failure(
                path,
                trigger,
                "read",
                logging::io_reason(error),
                error.raw_os_error(),
            ),
            Self::Decode(_) => log_document_failure(path, trigger, "decode", "invalid_utf8", None),
        }
    }
}

pub(crate) fn log_document_failure(
    path: &Path,
    trigger: &'static str,
    stage: &'static str,
    reason: &'static str,
    os_code: Option<i32>,
) {
    let mut fields = serde_json::json!({
        "document":logging::basename(path), "trigger":trigger, "stage":stage, "reason":reason,
    });
    if let Some(code) = os_code {
        fields["os_code"] = code.into();
    }
    logging::emit(
        if trigger == "startup" {
            Level::Error
        } else {
            Level::Warn
        },
        "document.open_failed",
        fields,
    );
}

fn load_document(path: &Path) -> Result<Document, ReadDocumentError> {
    let bytes = fs::read(path).map_err(ReadDocumentError::Read)?;
    let markdown = String::from_utf8(bytes).map_err(ReadDocumentError::Decode)?;
    Ok(document_from_markdown(path, markdown))
}

fn document_from_markdown(path: &Path, markdown: String) -> Document {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .replace(char::is_control, "�");
    let mut resources = TuiMarkdown.resources(&markdown);
    let links = std::mem::take(&mut resources.links)
        .into_iter()
        .map(|link| (link.label, link.target))
        .collect();
    Document::new(name, markdown, path.to_owned(), links).with_resources(resources)
}

fn require_regular_file(path: &Path) -> io::Result<()> {
    if fs::metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target is not a regular file",
        ))
    }
}

impl Host for RefCell<Session> {
    fn current_view(&self) -> Result<u64, String> {
        Ok(BODY_VIEW)
    }

    fn notify(&self, message: &str) {
        self.borrow_mut()
            .app
            .set_notice(message.replace(char::is_control, "�"));
    }

    fn document_path(&self, view: u64) -> Result<PathBuf, String> {
        validate_view(view)?;
        let session = self.borrow();
        let path = session.app.document_path();
        if path.as_os_str().is_empty() {
            return Err("document path is unavailable".into());
        }
        if path.is_absolute() {
            return Ok(path.to_owned());
        }
        let absolute = std::env::current_dir()
            .map_err(|error| format!("cannot determine working directory: {error}"))?
            .join(path);
        if absolute.is_absolute() {
            Ok(absolute)
        } else {
            // Windows drive-relative paths may refer to a different working directory.
            path::absolute(path)
                .map_err(|error| format!("cannot make document path absolute: {error}"))
        }
    }

    fn project_root(&self, view: u64, markers: &[String]) -> Result<PathBuf, String> {
        validate_view(view)?;
        files::project_root(self.borrow().app.document_path(), markers)
    }

    fn links(&self, view: u64) -> Result<Vec<LuaLink>, String> {
        validate_view(view)?;
        Ok(self
            .borrow()
            .app
            .links()
            .iter()
            .map(|link| LuaLink {
                id: link.id,
                label: link.label.clone(),
                target: link.target.clone(),
            })
            .collect())
    }

    fn resolve_link(&self, view: u64, id: u64) -> Result<PathBuf, String> {
        validate_view(view)?;
        let session = self.borrow();
        let path = session.app.resolve_link(id).inspect_err(|_| {
            log_document_failure(
                session.app.document_path(),
                "link",
                "resolve",
                "invalid_link",
                None,
            );
        })?;
        if path.is_absolute() {
            return Ok(path);
        }
        path::absolute(path).map_err(|error| {
            log_document_failure(
                session.app.document_path(),
                "link",
                "resolve",
                logging::io_reason(&error),
                error.raw_os_error(),
            );
            format!("cannot resolve link path: {error}")
        })
    }

    fn supports_pdf(&self) -> bool {
        self.borrow().pdf_geometry.is_ok()
    }

    fn open_file(&self, view: u64, path: &Path) -> Result<FileOpen, String> {
        validate_view(view)?;
        if !path.is_absolute() {
            log_document_failure(path, "open_file", "path", "relative_path", None);
            return Err("file path must be an absolute path".into());
        }
        if pdf_format::is_pdf(path) {
            let mut session = self.borrow_mut();
            if let Err(failure) = session.pdf_geometry {
                pdf::log_open_failure(path, "open_file", failure);
                return Err(failure.message.into());
            }
            let Session {
                pdf, app, floats, ..
            } = &mut *session;
            pdf.open(app, path, floats.active());
            return Ok(FileOpen::Pending);
        }
        require_regular_file(path).map_err(|error| {
            log_document_failure(
                path,
                "open_file",
                "metadata",
                logging::io_reason(&error),
                error.raw_os_error(),
            );
            format!("cannot read: {error}")
        })?;
        let document = load_document(path).map_err(|error| {
            error.log(path, "open_file");
            match error {
                ReadDocumentError::Read(error) => format!("cannot read: {error}"),
                ReadDocumentError::Decode(error) => format!("not valid UTF-8: {error}"),
            }
        })?;
        let mut session = self.borrow_mut();
        session.prepared = None;
        session.source_scroll = None;
        session.app.open_document(document);
        Ok(FileOpen::Opened)
    }

    fn scroll_to_line(&self, view: u64, line: i64) -> Result<(), String> {
        validate_view(view)?;
        let mut session = self.borrow_mut();
        if !session.app.markdown().is_empty() {
            let line = usize::try_from(line.max(1)).unwrap_or(usize::MAX);
            session.source_scroll = Some((session.app.document_id(), line - 1));
        }
        Ok(())
    }

    fn open_float(&self, view: u64, options: FloatOptions) -> Result<u64, String> {
        validate_view(view)?;
        self.borrow_mut().floats.open(view, options)
    }

    fn update_float(&self, view: u64, float: u64, options: FloatOptions) -> Result<(), String> {
        self.borrow_mut().floats.update(view, float, options)
    }

    fn close_float(&self, view: u64, float: u64) -> Result<(), String> {
        self.borrow_mut().floats.close(view, float)
    }
}

fn validate_view(view: u64) -> Result<(), String> {
    if view == BODY_VIEW {
        Ok(())
    } else {
        Err("view is closed".into())
    }
}

pub fn start_pdf(path: PathBuf) -> io::Result<()> {
    if !cfg!(windows) {
        let failure = PdfFailure::platform();
        pdf::log_open_failure(&path, "startup", failure);
        return Err(failure.into());
    }
    pdf::Jobs::with_screen(|screen, input, jobs| {
        if let Some((app, jobs)) = pdf::startup(screen, input, path, jobs)? {
            run_app(screen, input, app, jobs)
        } else {
            Ok(())
        }
    })
}

pub fn run(document: Document) -> io::Result<()> {
    pdf::Jobs::with_screen(|screen, input, jobs| {
        run_app(screen, input, App::from_document(document), jobs)
    })
}

fn run_app(
    screen: &mut Screen,
    terminal_input: &mut terminal::Input,
    app: App,
    jobs: pdf::Jobs,
) -> io::Result<()> {
    let mode = terminal_input.image_mode();
    let size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
    let mut initial = Session::with_app(
        app,
        mode,
        (mode != ImageMode::Text).then(crate::mermaid::Mermaid::from_env),
    );
    initial.pdf = jobs;
    initial.pdf_geometry = pdf::geometry(size.width, mode);
    if initial.app.pdf().is_none() {
        drop(initial.display());
    }
    let session = Rc::new(RefCell::new(initial));
    let (runtime, error) = Runtime::load(session.clone());
    if let Some(error) = error {
        session.borrow_mut().restore_after_init_error(&error);
    }

    let mut keys = InputQueue::default();
    let mut reload = watch::Reload::default();
    while !session.borrow().app.should_quit() {
        let now = Instant::now();
        reload.poll(&mut session.borrow_mut(), now, keys.busy(now));
        run_document(
            screen,
            terminal_input,
            &session,
            &runtime,
            &mut keys,
            &mut reload,
        )?;
    }
    Ok(())
}

fn run_document(
    screen: &mut Screen,
    terminal_input: &mut terminal::Input,
    session: &RefCell<Session>,
    runtime: &Runtime,
    keys: &mut InputQueue,
    reload: &mut watch::Reload,
) -> io::Result<()> {
    let mut image_mode = terminal_input.image_mode();
    let mut width = terminal::check(screen.size(), "size", Some(image_mode), "exit")?.width;
    let (document_id, prepared) = {
        let mut session = session.borrow_mut();
        (
            session.app.document_id(),
            session.app.pdf().is_none().then(|| session.display()),
        )
    };
    let build_markdown = |width, mode| {
        prepared
            .as_ref()
            .map(|md| Layout::with_images(&md.document, width, &md.images, mode))
    };
    let mut markdown_layout = build_markdown(width, image_mode);
    let mut pdf_layout: Option<pdf::PlacedPage> = None;
    let blank = Layout::empty_pdf();
    let search_text = prepared.as_ref().map_or("", |md| md.document.search_text());
    let mut previous_size = None;
    let result = (|| loop {
        let size = terminal::check(screen.size(), "size", Some(image_mode), "exit")?;
        if previous_size
            .replace(size)
            .is_some_and(|previous| previous != size)
        {
            terminal_input.refresh_cell_size();
        }
        let mode = terminal_input.image_mode();
        let mut drawn_size = (0, 0);
        let mut more_tiles = false;
        draw_frame(screen, mode, |frame, retired| {
            let area = frame.area();
            drawn_size = (area.width, area.height);
            if width != area.width || image_mode != mode {
                width = area.width;
                image_mode = mode;
                if let Some(layout) = &markdown_layout {
                    retired.extend(layout.transmitted_ids());
                }
                markdown_layout = build_markdown(width, image_mode);
            }
            let mut session = session.borrow_mut();
            session.sync_pdf_layout(area.width, mode, &mut pdf_layout, retired);
            let layout = markdown_layout
                .as_ref()
                .or_else(|| {
                    session.pdf_geometry.ok()?;
                    pdf_layout.as_deref()
                })
                .unwrap_or(&blank);
            session.show_display_notice(prepared.as_deref());
            session.draw(frame, layout, runtime);
            more_tiles = !session.app.help_open()
                && session.floats.active().is_none()
                && layout.pdf_transfer_pending(
                    ui::body_area(area, session.app.mode()),
                    session.app.top_row(),
                );
        })?;
        let layout = markdown_layout
            .as_ref()
            .or(pdf_layout
                .as_deref()
                .filter(|_| session.borrow().pdf_geometry.is_ok()))
            .unwrap_or(&blank);
        loop {
            let key = keys.pop_replayed().map_or_else(
                || {
                    if more_tiles {
                        terminal_input.poll_key_for(Duration::from_millis(8))
                    } else {
                        terminal_input.poll_key()
                    }
                },
                |key| Ok(Some(key)),
            )?;
            if prepared.is_none() && frame_stale(screen, terminal_input, drawn_size, image_mode)? {
                if let Some(key) = key {
                    keys.replay.push_front(key);
                }
                break;
            }
            let before = session.borrow().body_uninterrupted();
            let redraw = keys.dispatch(key, session, runtime, layout, search_text);
            let now = Instant::now();
            let net_redraw = poll_net(session, runtime, now, before);
            if session.borrow().app.should_quit()
                || session.borrow().app.document_id() != document_id
            {
                return Ok(());
            }
            // A resize can occur during dispatch (including a Lua callback). Check
            // again before publishing any asynchronous completion.
            let resized = frame_stale(screen, terminal_input, drawn_size, image_mode)?;
            let pdf_redraw = !resized && poll_pdf(session, runtime);
            let reload_redraw = reload.poll(&mut session.borrow_mut(), now, keys.busy(now));
            if session.borrow().app.document_id() != document_id {
                return Ok(());
            }
            if redraw || reload_redraw || net_redraw || pdf_redraw || resized || more_tiles {
                break;
            }
        }
    })();
    session.borrow_mut().pdf.depart(document_id);
    result.and(terminal::check(
        terminal::delete_images(
            markdown_layout
                .as_ref()
                .or(pdf_layout.as_deref())
                .into_iter()
                .flat_map(Layout::transmitted_ids),
        ),
        "delete_images",
        Some(image_mode),
        "exit",
    ))
}

fn frame_stale(
    screen: &Screen,
    input: &terminal::Input,
    drawn_size: (u16, u16),
    drawn_mode: ImageMode,
) -> io::Result<bool> {
    let size = terminal::check(screen.size(), "size", Some(input.image_mode()), "exit")?;
    Ok((size.width, size.height) != drawn_size || input.image_mode() != drawn_mode)
}

fn draw_frame(
    screen: &mut Screen,
    mode: ImageMode,
    draw: impl FnOnce(&mut ratatui::Frame<'_>, &mut Vec<u32>),
) -> io::Result<()> {
    let mut retired = Vec::new();
    let result = terminal::check(
        screen.draw(|frame| draw(frame, &mut retired)),
        "draw",
        Some(mode),
        "exit",
    );
    result.and(terminal::check(
        terminal::delete_images(retired),
        "delete_images",
        Some(mode),
        "exit",
    ))
}

fn poll_pdf(session: &RefCell<Session>, runtime: &Runtime) -> bool {
    let update = {
        let mut state = session.borrow_mut();
        let Session {
            pdf,
            app,
            floats,
            pdf_geometry,
            prepared,
            source_scroll,
            ..
        } = &mut *state;
        let id = app.document_id();
        let update = pdf.poll(app, floats, *pdf_geometry);
        if app.document_id() != id {
            // Markdown history must re-read images even if a PDF was the only intervening view.
            *prepared = None;
            *source_scroll = None;
        }
        update
    };
    if let Some((view, float)) = update.close_float {
        let closed = session.close_float(view, float);
        debug_assert!(
            closed.is_ok(),
            "only the active float can complete an open request"
        );
        runtime.forget_float(float);
    }
    update.redraw
}

fn poll_net(session: &RefCell<Session>, runtime: &Runtime, now: Instant, before: bool) -> bool {
    runtime.poll_net(now, || {
        let session = session.borrow();
        before && !session.app.should_quit() && session.body_uninterrupted()
    })
}

impl InputQueue {
    fn pop_replayed(&mut self) -> Option<KeyEvent> {
        self.replay.pop_front()
    }

    fn busy(&self, now: Instant) -> bool {
        // Let an ambiguous key sequence resolve before synchronous display preparation.
        // Pure prefixes have no input deadline and can survive a reload.
        !self.replay.is_empty()
            || self
                .undecided_key
                .is_some_and(|key| now.duration_since(key) < Duration::from_millis(350))
    }

    fn dispatch(
        &mut self,
        key: Option<KeyEvent>,
        session: &RefCell<Session>,
        runtime: &Runtime,
        rows: &impl ViewRows,
        search_text: &str,
    ) -> bool {
        let active_float = session.borrow().floats.active();
        if let Some((view, float)) = active_float {
            self.input.reset();
            self.undecided_key = None;
            let Some(key) = key else { return false };
            if is_escape(key) {
                close_active_float(session, runtime);
            } else if let Some(key) = plugin_key(key) {
                if let Err(error) = runtime.run_float_key(view, float, key) {
                    recover_lua_error(session, runtime, &error);
                }
            } else {
                return false;
            }
            return true;
        }

        let dispatch = if let Some(key) = key {
            let app = &session.borrow().app;
            self.input
                .dispatch(key, app.mode(), app.help_open(), &runtime.keymap())
        } else {
            self.input.expire()
        };
        let Some(dispatch) = dispatch else {
            if key.is_some_and(|key| key.kind == KeyEventKind::Press) {
                self.undecided_key = Some(Instant::now());
            }
            return false;
        };
        self.undecided_key = None;
        for key in dispatch.replay.into_iter().rev() {
            self.replay.push_front(key);
        }
        match dispatch.action {
            InputAction::Builtin(action) => {
                session.borrow_mut().apply(action, rows, search_text);
            }
            InputAction::Lua(mapping) => {
                if let Err(error) = runtime.run_mapping(mapping) {
                    recover_lua_error(session, runtime, &error);
                }
            }
        }
        true
    }
}

fn close_active_float(session: &RefCell<Session>, runtime: &Runtime) {
    let active_float = session.borrow().floats.active();
    if let Some((view, float)) = active_float {
        let closed = session.close_float(view, float);
        debug_assert!(closed.is_ok(), "the active float must be closable");
        runtime.forget_float(float);
    }
}

fn recover_lua_error(session: &RefCell<Session>, runtime: &Runtime, error: &str) {
    close_active_float(session, runtime);
    session.borrow_mut().show_lua_error(error);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Action;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::{
        env,
        error::Error,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> io::Result<Self> {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let next = NEXT.fetch_add(1, Ordering::Relaxed);
                let path =
                    env::temp_dir().join(format!("tvi-session-{}-{next}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self(path)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
        }

        fn init(&self, source: &str) -> io::Result<PathBuf> {
            let path = self.0.join("init.lua");
            fs::write(&path, source)?;
            Ok(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    fn lua_view_path_preserves_relative_startup_spelling_and_tracks_document_switches()
    -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let next = scratch.0.join("next.md");
        fs::write(&next, "Next")?;
        let relative = Path::new("./docs/index.md");
        let absolute = env::current_dir()?.join(relative);
        let init = scratch.init(
            r#"
            local view = tvi.ui.current_view()
            tvi.keymap.set('normal', '<F2>', function() tvi.ui.notify(assert(view:path())) end)
        "#,
        )?;
        let session = Rc::new(RefCell::new(Session::new(read_document(relative)?)));
        let (runtime, error) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(error, None);
        let mut keys = InputQueue::default();
        let mut show_path = || {
            keys.dispatch(
                Some(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
                &session,
                &runtime,
                &[],
                "",
            )
        };
        assert!(show_path());
        assert_eq!(session.borrow().app.notice(), absolute.to_str());
        session.open_file(BODY_VIEW, &next)?;
        assert!(show_path());
        assert_eq!(session.borrow().app.notice(), next.to_str());
        session.borrow_mut().app.apply(Action::JumpBack, &[], "");
        assert!(show_path());
        assert_eq!(session.borrow().app.notice(), absolute.to_str());
        Ok(())
    }

    #[test]
    fn follow_scrolls_on_an_idle_poll_but_discards_modal_frames() -> Result<(), Box<dyn Error>> {
        use crate::app::Direction;
        use ratatui::{Terminal, backend::TestBackend};
        use std::{
            io::Write,
            net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
        };

        let scratch = Scratch::new()?;
        let source = scratch.0.join("source.md");
        let markup: String = (1..=30).map(|i| format!("item {i}\n\n")).collect();
        fs::write(&source, &markup)?;
        let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let address = reserved.local_addr()?;
        let init = scratch.init(&format!(
            r#"
            os.getenv = function(name)
                if name == 'HERDR_WORKSPACE_ID' then return 'workspace' end
                if name == 'TVI_FOLLOW_PORT' then return '{}' end
            end
            local follow = (function()
                {}
            end)()
            assert(follow.start())
        "#,
            address.port(),
            include_str!("../plugins/follow.lua")
        ))?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        drop(reserved);
        let (runtime, error) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(error, None);
        let send = |line: usize| -> io::Result<()> {
            let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
            writeln!(stream, "workspace\t{line}\t{}", source.display())?;
            stream.shutdown(Shutdown::Write)
        };
        let id = session.borrow().app.document_id();
        let prepared = session.borrow_mut().display();
        let layout = Layout::at_width(&prepared.document, 30);
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| session.borrow_mut().draw(frame, &layout, &runtime))
                .map(|_| ())
        };
        let mut terminal = Terminal::new(TestBackend::new(30, 10))?;
        draw(&mut terminal)?;
        for action in [
            Action::StartCommand,
            Action::StartSearch(Direction::Forward),
            Action::Help,
        ] {
            session.borrow_mut().app.apply(action, &layout, "");
            assert!(!session.borrow().body_uninterrupted());
            send(29)?;
            assert!(!poll_net(&session, &runtime, Instant::now(), false));
            session.borrow_mut().app.apply(Action::Cancel, &layout, "");
        }
        let float = session.open_float(BODY_VIEW, FloatOptions::default())?;
        assert!(!session.borrow().body_uninterrupted());
        send(29)?;
        // Escape dismisses the float before the tick, but must not deliver its backlog.
        session.close_float(BODY_VIEW, float)?;
        assert!(!poll_net(&session, &runtime, Instant::now(), false));
        assert!(!poll_net(&session, &runtime, Instant::now(), true));
        draw(&mut terminal)?;
        assert_eq!(session.borrow().app.top_row(), 0);

        send(20)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        while !poll_net(&session, &runtime, Instant::now(), true) {
            assert!(Instant::now() < deadline, "follow did not request a redraw");
            std::thread::yield_now();
        }
        draw(&mut terminal)?;
        assert!(session.borrow().app.top_row() > 0);
        assert_eq!(session.borrow().app.document_id(), id);
        assert!(session.borrow().app.search_query().is_empty());
        session
            .borrow_mut()
            .app
            .apply(Action::JumpBack, &layout, "");
        assert_eq!(session.borrow().app.document_id(), id);
        Ok(())
    }

    #[test]
    fn lua_scroll_centers_the_new_document_and_keeps_the_float_and_search()
    -> Result<(), Box<dyn Error>> {
        use KeyCode::{Char, Enter, Esc, F};
        use ratatui::{Terminal, backend::TestBackend};

        let scratch = Scratch::new()?;
        let source = scratch.0.join("source.md");
        let markup: String = (0..20).map(|i| format!("item {i}\r\n\r\n")).collect();
        fs::write(&source, &markup)?;
        fs::write(scratch.0.join("next.md"), &markup)?;
        fs::write(scratch.0.join("later.md"), &markup)?;
        fs::write(scratch.0.join("empty.md"), "")?;
        let init = scratch.init(
            r#"
            local view = tvi.ui.current_view()
            local root = assert(view:project_root({ root_markers = {} }))
            local sep = root:find("\\", 1, true) and "\\" or "/"
            tvi.keymap.set("normal", "<F2>", function()
                assert(view:open_file(root .. sep .. "next.md"))
                view:scroll_to_line(math.mininteger)
                view:scroll_to_line(21)
                assert(not pcall(view.scroll_to_line, view, "5"))
            end)
            tvi.keymap.set("normal", "<F3>", function()
                assert(not pcall(view.scroll_to_line, view, 2.0))
                view:open_float({
                    lines = {"body scroll test"},
                    on_key = function(_, key)
                        view:scroll_to_line(key.text == "b" and math.maxinteger or math.mininteger)
                    end,
                })
            end)
            tvi.keymap.set("normal", "<F4>", function()
                view:scroll_to_line(21)
                assert(view:open_file(root .. sep .. "empty.md"))
                view:scroll_to_line(500)
                assert(view:open_file(root .. sep .. "later.md"))
            end)
        "#,
        )?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let (runtime, diagnostic) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(diagnostic, None);
        let mut terminal = Terminal::new(TestBackend::new(30, 10))?;
        let mut keys = InputQueue::default();
        let mut send = |code| {
            let prepared = session.borrow_mut().display();
            let layout = Layout::at_width(&prepared.document, 30);
            assert!(keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &session,
                &runtime,
                &layout,
                prepared.document.search_text(),
            ));
        };
        let draw =
            |terminal: &mut Terminal<TestBackend>| -> Result<usize, std::convert::Infallible> {
                let prepared = session.borrow_mut().display();
                let layout = Layout::at_width(&prepared.document, 30);
                terminal.draw(|frame| session.borrow_mut().draw(frame, &layout, &runtime))?;
                Ok(session.borrow().app.top_row())
            };

        send(F(2));
        assert!(
            session.borrow().app.document_path().ends_with("next.md"),
            "{:?}",
            session.borrow().app.notice()
        );
        assert_eq!(draw(&mut terminal)?, 16);
        terminal.backend_mut().resize(30, 8);
        assert_eq!(draw(&mut terminal)?, 16);
        terminal.backend_mut().resize(30, 10);

        for code in "/item 10".chars().map(Char).chain([Enter]) {
            send(code);
        }
        let before = draw(&mut terminal)?;
        let selected = session.borrow().app.search_selection();
        assert!(selected.is_some());
        send(F(3));
        assert_eq!(draw(&mut terminal)?, before);
        let float = session.borrow().floats.active();
        assert!(float.is_some());
        send(Char('b'));
        assert_eq!(draw(&mut terminal)?, 30);
        send(Char('a'));
        assert_eq!(draw(&mut terminal)?, 0);
        assert_eq!(session.borrow().floats.active(), float);
        assert_eq!(session.borrow().app.search_selection(), selected);
        send(Esc);
        send(F(4));
        assert!(session.borrow().app.document_path().ends_with("later.md"));
        assert_eq!(draw(&mut terminal)?, 0);
        Ok(())
    }

    #[test]
    fn colorscheme_changes_on_redraw_without_resetting_search_prompt_or_float()
    -> Result<(), Box<dyn Error>> {
        use KeyCode::{Char, Enter, Esc, F};
        use ratatui::{
            Terminal,
            backend::TestBackend,
            style::{Color, Modifier},
        };

        let scratch = Scratch::new()?;
        let source = scratch.0.join("source.md");
        fs::write(
            &source,
            (0..20).map(|i| format!("row {i}\n\n")).collect::<String>(),
        )?;
        let init = scratch.init(
            r#"
            tvi.colorscheme.set({ ["document.normal"] = { bg = "red" } })
            local view = tvi.ui.current_view()
            tvi.keymap.set("search", "<F2>", function()
                view:open_float({
                    title = "title", header = "head", lines = {"focus"}, focus_line = 1,
                    on_key = function()
                        tvi.colorscheme.set({
                            ["ui.status"] = { fg = "cyan", reverse = false },
                            ["ui.prompt"] = { bg = "magenta" },
                            ["float.normal"] = { bg = "blue", reverse = false },
                            ["float.border"] = { fg = "yellow" },
                        })
                        tvi.ui.notify("ready")
                    end,
                })
            end)
        "#,
        )?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let (runtime, diagnostic) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(diagnostic, None);
        let prepared = session.borrow_mut().display();
        let layout = Layout::at_width(&prepared.document, 30);
        let mut terminal = Terminal::new(TestBackend::new(30, 10))?;
        let mut keys = InputQueue::default();
        let mut send = |code| {
            assert!(keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &session,
                &runtime,
                &layout,
                prepared.document.search_text(),
            ));
        };
        let mut draw = || {
            terminal.draw(|frame| session.borrow_mut().draw(frame, &layout, &runtime))?;
            Ok::<_, std::convert::Infallible>(terminal.backend().buffer().clone())
        };

        assert_eq!(draw()?[(29, 0)].bg, Color::Red);
        for code in "/row 15"
            .chars()
            .map(Char)
            .chain([Enter, Char('/'), Char('x'), F(2)])
        {
            send(code);
        }
        let before = draw()?;
        let (handle, top, selected) = {
            let session = session.borrow();
            (
                session.floats.active(),
                session.app.top_row(),
                session.app.search_selection(),
            )
        };
        assert!(top > 0 && selected.is_some());
        assert_eq!(before[(12, 4)].symbol(), "f");
        assert!(before[(12, 4)].modifier.contains(Modifier::REVERSED));

        send(Char('c'));
        let after = draw()?;
        assert_eq!(after[(12, 2)].symbol(), "t");
        assert_eq!(after[(12, 2)].fg, Color::Yellow);
        assert_eq!(after[(12, 3)].bg, Color::Blue);
        assert!(after[(12, 3)].modifier.contains(Modifier::BOLD));
        assert_eq!(after[(12, 4)].symbol(), "f");
        assert_eq!(after[(12, 4)].bg, Color::Blue);
        assert!(!after[(12, 4)].modifier.contains(Modifier::REVERSED));
        assert_eq!(after[(29, 0)].bg, Color::Reset);
        assert_eq!(after[(0, 8)].fg, Color::Cyan);
        assert!(!after[(0, 8)].modifier.contains(Modifier::REVERSED));
        assert_eq!(after[(1, 9)].symbol(), "x");
        assert_eq!(after[(29, 9)].bg, Color::Magenta);
        assert_eq!(session.borrow().floats.active(), handle);
        assert_eq!(session.borrow().app.top_row(), top);
        assert_eq!(session.borrow().app.search_selection(), selected);
        assert_eq!(session.borrow().app.notice(), Some("ready"));
        send(Esc);
        assert_eq!(session.borrow().app.prompt(), "x");
        Ok(())
    }

    #[test]
    fn lua_notify_changes_only_notice_and_user_ff_overrides_default() -> Result<(), Box<dyn Error>>
    {
        let scratch = Scratch::new()?;
        let source = scratch.0.join("source.md");
        fs::write(&source, "# Source")?;
        let init = scratch.init(
            r#"
            local view = tvi.ui.current_view()
            local root, err = view:project_root({ root_markers = {} })
            assert(root and err == nil)
            local invalid, reason = view:project_root({ root_markers = { "../bad" } })
            assert(invalid == nil and reason)
            tvi.keymap.set("normal", "ff", function()
                tvi.ui.notify("sample notice")
            end)
        "#,
        )?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let (runtime, diagnostic) = Runtime::load_from(session.clone(), Some(init));
        assert_eq!(diagnostic, None);
        let id = session.borrow().app.document_id();
        let mut keys = InputQueue::default();
        let f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);
        assert!(!keys.dispatch(Some(f), &session, &runtime, &[], ""));
        assert!(keys.dispatch(Some(f), &session, &runtime, &[], ""));
        let session = session.borrow();
        assert_eq!(session.app.notice(), Some("sample notice"));
        assert_eq!(session.app.mode(), Mode::Normal);
        assert_eq!(session.app.document_id(), id);
        assert_eq!(session.floats.active(), None);
        Ok(())
    }

    #[test]
    #[ignore = "requires fd or fdfind on PATH"]
    fn file_picker_opens_real_fd_results_and_ctrl_o_returns() -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let root = scratch.0.join("資料 project");
        let docs = root.join("notes with space");
        fs::create_dir_all(&docs)?;
        fs::create_dir(root.join(".git"))?;
        fs::write(root.join(".fdignore"), "ignored.md\n")?;
        fs::write(root.join("a.md"), "# A")?;
        fs::write(root.join("ignored.md"), "# Ignore")?;
        fs::write(root.join(".hidden.md"), "# Hidden")?;
        fs::write(root.join("other.txt"), "# Wrong extension")?;
        fs::write(docs.join("資料 b.md"), "# Selected")?;
        let source = docs.join("last.md");
        fs::write(&source, "# Source")?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let source_id = session.borrow().app.document_id();
        let (runtime, diagnostic) = Runtime::load_from(session.clone(), None);
        assert_eq!(diagnostic, None);
        let mut keys = InputQueue::default();
        let mut send = |code, modifiers| {
            let key = KeyEvent::new(code, modifiers);
            keys.dispatch(Some(key), &session, &runtime, &[], "")
        };
        let plain = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let f = KeyCode::Char('f');
        assert!(!send(f, plain));
        assert!(send(f, plain));
        assert!(
            session.borrow().floats.active().is_some(),
            "{:?}",
            session.borrow().app.notice()
        );
        assert!(send(KeyCode::Esc, plain));
        assert!(session.borrow().floats.active().is_none());
        assert_eq!(session.borrow().app.document_id(), source_id);
        assert!(!send(f, plain));
        assert!(send(f, plain));
        assert!(send(KeyCode::Char('n'), ctrl));
        assert!(send(KeyCode::Char('n'), ctrl));
        assert!(send(KeyCode::Char('n'), ctrl));
        assert!(send(KeyCode::Char('p'), ctrl));
        assert!(send(KeyCode::Char('n'), ctrl));
        assert!(send(KeyCode::Enter, plain));
        assert_eq!(session.borrow().app.name(), "資料 b.md");
        assert!(session.borrow().floats.active().is_none());
        assert!(send(KeyCode::Char('o'), ctrl));
        assert_eq!(session.borrow().app.document_id(), source_id);
        Ok(())
    }

    #[test]
    fn local_images_follow_document_jumps_without_becoming_links() -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let docs = scratch.0.join("docs");
        fs::create_dir(&docs)?;
        let pixels = image::RgbImage::from_pixel(30, 60, image::Rgb([250, 0, 0]));
        pixels.save_with_format(scratch.0.join("first.data"), image::ImageFormat::Png)?;
        let pixels = image::RgbImage::from_pixel(30, 60, image::Rgb([0, 0, 250]));
        pixels.save_with_format(docs.join("second.data"), image::ImageFormat::Jpeg)?;
        fs::write(docs.join("broken.png"), b"not a PNG")?;
        fs::create_dir(docs.join("directory.png"))?;
        let root = scratch.0.join("root.md");
        fs::write(&root, "[Next](docs/next.md)\n\n![same](first.data)")?;
        fs::write(
            docs.join("next.md"),
            "![same](../first.data)\n\n![same](second.data)\n\n![same](broken.png)\n\n![same](directory.png)\n\n![same](missing.png)",
        )?;
        let session = RefCell::new(Session::new(read_document(&root)?));
        let read = || {
            let session = session.borrow();
            let app = &session.app;
            images::load(
                app.document_path(),
                &app.markdown_resources().images,
                &mut images::Budget::default(),
            )
        };
        assert_eq!(
            read().iter().map(Option::is_some).collect::<Vec<_>>(),
            [true]
        );
        let links = session.links(BODY_VIEW)?;
        assert_eq!(links.len(), 1);
        let path = session.resolve_link(BODY_VIEW, links[0].id)?;
        session.open_file(BODY_VIEW, &path)?;
        assert!(session.links(BODY_VIEW)?.is_empty());
        let loaded = read();
        assert_eq!(
            loaded.iter().map(Option::is_some).collect::<Vec<_>>(),
            [true, true, false, false, false]
        );
        let png = loaded[0].as_ref().ok_or("PNG missing")?.to_rgb8();
        let jpeg = loaded[1].as_ref().ok_or("JPEG missing")?.to_rgb8();
        assert_eq!(png.get_pixel(0, 0).0, [250, 0, 0]);
        assert!(jpeg.get_pixel(0, 0).0[2] > 240);
        session.borrow_mut().app.apply(Action::JumpBack, &[], "");
        assert_eq!(session.links(BODY_VIEW)?[0].id, links[0].id);
        assert_eq!(read().len(), 1);
        Ok(())
    }

    #[test]
    fn resolve_link_returns_an_absolute_path_without_requiring_the_target()
    -> Result<(), Box<dyn Error>> {
        let session = RefCell::new(Session::new(Document::new(
            "source.md".into(),
            "[Missing](missing.md)".into(),
            PathBuf::from("docs/source.md"),
            vec![("Missing".into(), "missing.md".into())],
        )));
        let id = session.links(BODY_VIEW)?[0].id;
        assert_eq!(
            session.resolve_link(BODY_VIEW, id)?,
            env::current_dir()?.join("docs").join("missing.md")
        );
        assert_eq!(session.borrow().app.name(), "source.md");
        Ok(())
    }

    #[test]
    fn open_file_failures_leave_the_original_document_and_history() -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let invalid = scratch.0.join("invalid.md");
        let directory = scratch.0.join("directory.md");
        fs::write(&invalid, [0xff])?;
        fs::create_dir(&directory)?;
        let source = scratch.0.join("source.md");
        let next = scratch.0.join("next.md");
        fs::write(&source, "# Source")?;
        fs::write(&next, "# Next")?;
        let session = RefCell::new(Session::new(read_document(&source)?));
        let initial_id = session.borrow().app.document_id();
        session.open_file(BODY_VIEW, &next)?;
        assert_eq!(session.borrow().app.document_path(), next);
        let next_id = session.borrow().app.document_id();
        for (path, expected) in [
            (invalid.as_path(), "not valid UTF-8"),
            (directory.as_path(), "not a regular file"),
            (Path::new("relative.md"), "must be an absolute path"),
        ] {
            assert!(matches!(
                session.open_file(BODY_VIEW, path),
                Err(reason) if reason.contains(expected)
            ));
            assert_eq!(session.borrow().app.document_id(), next_id);
        }
        session.borrow_mut().app.apply(Action::JumpBack, &[], "");
        assert_eq!(session.borrow().app.document_id(), initial_id);
        Ok(())
    }

    #[test]
    fn failed_init_restores_the_original_document_after_opening_a_link()
    -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let docs = scratch.0.join("docs");
        fs::create_dir(&docs)?;
        fs::write(scratch.0.join("next.md"), "Next document")?;
        let source = docs.join("source.md");
        fs::write(&source, "[Next](../next.md)")?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let initial_id = session.borrow().app.document_id();
        let link = session.links(BODY_VIEW)?[0].id;
        let init = scratch.init(
            r#"
local view = tvi.ui.current_view()
local id = view:links()[1].id
local target = assert(view:resolve_link(id))
assert(view:open_file(target))
local path, err = view:resolve_link(id)
assert(path == nil and err:find("current document", 1, true))
error("startup failed")
"#,
        )?;
        let (_runtime, error) = Runtime::load_from(session.clone(), Some(init));
        let error = error.ok_or("the config should have failed")?;
        assert!(error.contains("startup failed"), "{error}");
        let mut session = session.borrow_mut();
        assert_eq!(session.app.name(), "next.md");
        session.restore_after_init_error(&error);
        assert_eq!(session.app.document_id(), initial_id);
        assert_eq!(session.app.links()[0].id, link);
        assert!(session.app.notice().is_some_and(|notice| {
            notice.contains("init.lua") && notice.contains("startup failed")
        }));
        session.app.apply(Action::JumpBack, &[], "");
        assert_eq!(session.app.document_id(), initial_id);
        Ok(())
    }

    #[test]
    fn document_image_and_config_events_use_only_safe_fields() -> Result<(), Box<dyn Error>> {
        use serde_json::Value;

        let scratch = Scratch::new()?;
        let docs = scratch.0.join("private-directory");
        fs::create_dir(&docs)?;
        let source = docs.join("source.md");
        fs::write(
            &source,
            "[Missing](missing.md)\n\n![private-caption](private-target.png)",
        )?;
        let logs = scratch.0.join("logs");
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));

        logging::capture_for_test(&logs, || -> io::Result<()> {
            let init = scratch.0.join("init.lua");
            let (_, error) = Runtime::load_from(session.clone(), Some(init.clone()));
            assert!(error.is_none());
            assert!(!logs.exists());
            let id = session.borrow().app.links()[0].id;
            let path = session
                .resolve_link(BODY_VIEW, id)
                .map_err(io::Error::other)?;
            assert!(session.open_file(BODY_VIEW, &path).is_err());
            assert_eq!(session.borrow().app.name(), "source.md");

            let session_ref = session.borrow();
            let app = &session_ref.app;
            let images = images::load(
                app.document_path(),
                &app.markdown_resources().images,
                &mut images::Budget::default(),
            );
            assert_eq!(images.len(), 1);
            assert!(images[0].is_none());
            drop(session_ref);

            fs::write(&init, "error('private-lua-message')")?;
            let (_, error) = Runtime::load_from(session.clone(), Some(init));
            assert!(error.is_some_and(|error| error.contains("private-lua-message")));
            Ok(())
        })?;

        let output = fs::read_to_string(logs.join("tvi.log"))?;
        assert!(!output.contains("private-"));
        let records: Vec<Value> = output
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["event"], "document.open_failed");
        assert_eq!(records[0]["document"], "missing.md");
        assert_eq!(records[0]["trigger"], "open_file");
        assert_eq!(records[0]["level"], "WARN");
        assert_eq!(records[0]["reason"], "not_found");
        assert_eq!(records[1]["event"], "image.load_failed");
        assert_eq!(records[1]["document"], "source.md");
        assert_eq!(records[1]["image_index"], 1);
        assert_eq!(records[1]["reason"], "not_found");
        assert_eq!(records[2]["event"], "lua.failed");
        assert_eq!(records[2]["source"], "init.lua");
        assert_eq!(records[2]["stage"], "config");
        assert_eq!(records[2]["reason"], "runtime_error");
        assert_eq!(records[2]["action"], "use_defaults");
        Ok(())
    }

    #[test]
    fn search_reveals_the_later_column_and_keeps_the_reading_position_on_resize() {
        let source = "| L | R |\n| --- | --- |\n| abcdefgh | FIND |\n| abcdefgh | FIND |\n\nend";
        let session = Rc::new(RefCell::new(Session::new(Document::new(
            "source.md".into(),
            source.into(),
            PathBuf::from("source.md"),
            vec![],
        ))));
        let (runtime, _) = Runtime::load_from(session.clone(), None);
        let document = TuiMarkdown.render(source, 13);
        let wide = Layout::at_width(&document, 19);
        let narrow = Layout::new(&document);
        let mut keys = InputQueue::default();
        session.borrow_mut().app.reflow(&wide, 1);
        for code in "/FIND".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
            assert!(keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &session,
                &runtime,
                &wide,
                document.search_text()
            ));
        }
        assert_eq!(session.borrow().app.top_row(), 3);
        let locations = narrow;
        session.borrow_mut().app.reflow(&locations, 1);
        assert_eq!(session.borrow().app.top_row(), 3);
        assert_eq!(locations.text().lines[3].to_string(), "│ abc │ FIN │");
        for (key, row, selection) in [('N', 6, (2, 2)), ('n', 3, (1, 2))] {
            assert!(keys.dispatch(
                Some(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)),
                &session,
                &runtime,
                &locations,
                document.search_text()
            ));
            assert_eq!(session.borrow().app.top_row(), row);
            assert_eq!(session.borrow().app.search_selection(), Some(selection));
        }
        assert!(keys.dispatch(
            Some(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            &session,
            &runtime,
            &locations,
            document.search_text()
        ));
        assert_eq!(session.borrow().app.top_row(), 4);
        session.borrow_mut().app.reflow(&wide, 1);
        assert_eq!(session.borrow().app.top_row(), 3);
        session.borrow_mut().app.reflow(&locations, 1);
        assert_eq!(session.borrow().app.top_row(), 4);
    }

    #[test]
    fn replayed_keys_stay_with_the_float_and_errors_return_to_the_prompt()
    -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let root = Document::new("source.md".into(), String::new(), PathBuf::new(), vec![]);
        let session = Rc::new(RefCell::new(Session::new(root)));
        let init = scratch.init(
            r#"
local function open()
  local view = tvi.ui.current_view()
  assert(view:open_float({
    lines = {"modal"},
    on_key = function(float, key)
      if key.name == "Esc" then error("Escape reached Lua") end
      if key.name == "x" then
        float:close()
        assert(view:open_float({lines = {"replacement"}, on_key = function() end}))
        error("callback failed")
      end
    end,
  }))
end
tvi.keymap.set("search", "<F2>", open, { desc = "open" })
tvi.keymap.set("search", "<F2><F3>", function() end, { desc = "longer" })
"#,
        )?;
        let (runtime, error) = Runtime::load_from(session.clone(), Some(init));
        assert!(error.is_none(), "{error:?}");
        let mut keys = InputQueue::default();
        let send = |keys: &mut InputQueue, code| {
            keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &session,
                &runtime,
                &[],
                "",
            )
        };
        assert!(send(&mut keys, KeyCode::Char('/')));
        assert!(send(&mut keys, KeyCode::Char('z')));

        for key in ['q', 'x'] {
            assert!(!send(&mut keys, KeyCode::F(2)));
            assert!(send(&mut keys, KeyCode::Char(key)));
            assert!(session.borrow().floats.active().is_some());
            let replayed = keys.pop_replayed();
            assert_eq!(
                replayed,
                Some(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
            );
            assert!(keys.dispatch(replayed, &session, &runtime, &[], ""));
            assert_eq!(session.borrow().app.prompt(), "z");
            if key == 'q' {
                assert!(send(&mut keys, KeyCode::Esc));
                assert!(session.borrow().app.notice().is_none());
            } else {
                assert!(
                    session
                        .borrow()
                        .app
                        .notice()
                        .is_some_and(|notice| notice.contains("callback failed"))
                );
            }
            assert!(session.borrow().floats.active().is_none());
            assert!(matches!(session.borrow().app.mode(), Mode::Search(_)));
        }
        assert!(send(&mut keys, KeyCode::Char('q')));
        assert_eq!(session.borrow().app.prompt(), "zq");
        assert!(send(&mut keys, KeyCode::Esc));
        assert_eq!(session.borrow().app.mode(), Mode::Normal);
        Ok(())
    }
}
