use std::{
    cell::RefCell,
    collections::VecDeque,
    fs, io,
    path::{self, Path, PathBuf},
    rc::Rc,
    string::FromUtf8Error,
};

use crossterm::event::KeyEvent;

use crate::{
    app::{App, Document, ViewRows},
    files,
    logging::{self, Level},
    lua::{Host, LuaLink, Runtime},
    markdown::{MarkdownRenderer, tui_markdown::TuiMarkdown},
    terminal::{self, ImageMode, Screen},
    ui::{
        self,
        float::{FloatOptions, Floats},
        input::{Input, InputAction, is_escape, plugin_key},
        layout::Layout,
    },
};

#[cfg(test)]
use crate::images;

mod mermaid;

const BODY_VIEW: u64 = 1;

struct Session {
    app: App,
    floats: Floats,
    image_mode: ImageMode,
    renderer: Option<crate::mermaid::Mermaid>,
    prepared: Option<(u64, Rc<mermaid::PreparedDisplay>)>,
}

#[derive(Default)]
struct InputQueue {
    input: Input,
    replay: VecDeque<KeyEvent>,
}

impl Session {
    #[cfg(test)]
    fn new(document: Document) -> Self {
        Self::with_images(document, ImageMode::Text, None)
    }

    fn with_images(
        document: Document,
        image_mode: ImageMode,
        renderer: Option<crate::mermaid::Mermaid>,
    ) -> Self {
        Self {
            app: App::from_document(document),
            floats: Floats::default(),
            image_mode,
            renderer,
            prepared: None,
        }
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
        self.prepared = Some((id, Rc::clone(&prepared)));
        prepared
    }

    fn restore_after_init_error(&mut self, error: &str) {
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
    Ok(Document::new(name, markdown, path.to_owned(), links).with_resources(resources))
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

    fn open_file(&self, view: u64, path: &Path) -> Result<(), String> {
        validate_view(view)?;
        if !path.is_absolute() {
            log_document_failure(path, "open_file", "path", "relative_path", None);
            return Err("file path must be an absolute path".into());
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
        session.app.open_document(document);
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

pub fn run(
    screen: &mut Screen,
    terminal_input: &mut terminal::Input,
    document: Document,
) -> io::Result<()> {
    let mode = terminal_input.image_mode();
    let session = Rc::new(RefCell::new(Session::with_images(
        document,
        mode,
        (mode != ImageMode::Text).then(crate::mermaid::Mermaid::from_env),
    )));
    drop(session.borrow_mut().display());
    let (runtime, error) = Runtime::load(session.clone());
    if let Some(error) = error {
        session.borrow_mut().restore_after_init_error(&error);
    }

    let mut keys = InputQueue::default();
    while !session.borrow().app.should_quit() {
        run_document(screen, terminal_input, &session, &runtime, &mut keys)?;
    }
    Ok(())
}

fn run_document(
    screen: &mut Screen,
    terminal_input: &mut terminal::Input,
    session: &RefCell<Session>,
    runtime: &Runtime,
    keys: &mut InputQueue,
) -> io::Result<()> {
    let mut image_mode = terminal_input.image_mode();
    let mut width = terminal::check(screen.size(), "size", Some(image_mode), "exit")?.width;
    let (document_id, prepared) = {
        let mut session = session.borrow_mut();
        (session.app.document_id(), session.display())
    };
    let document = &prepared.document;
    let images = &prepared.images;
    let mut layout = Layout::with_images(document, width, images, image_mode);
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
        let mut retired = Vec::new();
        let draw_result = screen.draw(|frame| {
            let area = frame.area();
            drawn_size = (area.width, area.height);
            if width != area.width || image_mode != mode {
                width = area.width;
                image_mode = mode;
                retired.extend(layout.transmitted_ids());
                layout = Layout::with_images(document, width, images, image_mode);
            }
            let mut session = session.borrow_mut();
            if session.app.mode() == crate::app::Mode::Normal && session.app.notice().is_none() {
                mermaid::show_notice(&mut session.app, &prepared);
                if session.app.notice().is_none() && logging::take_failure_notice() {
                    session.app.set_notice("ログを保存できません".into());
                }
            }
            let body = ui::body_area(area, session.app.mode());
            session.app.reflow(&layout, usize::from(body.height));
            ui::draw(
                frame,
                &session.app,
                &layout,
                session.floats.active().is_some(),
            );
            session.floats.draw(frame, body);
        });
        let draw_result = terminal::check(draw_result, "draw", Some(mode), "exit");
        let cleanup = terminal::check(
            terminal::delete_images(retired),
            "delete_images",
            Some(mode),
            "exit",
        );
        draw_result?;
        cleanup?;
        loop {
            let key = keys
                .pop_replayed()
                .map_or_else(|| terminal_input.poll_key(), |key| Ok(Some(key)))?;
            if keys.dispatch(key, session, runtime, &layout, document.search_text()) {
                let app = &session.borrow().app;
                if app.should_quit() || app.document_id() != document_id {
                    return Ok(());
                }
                break;
            }
            let size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
            if (size.width, size.height) != drawn_size || terminal_input.image_mode() != image_mode
            {
                break;
            }
        }
    })();
    result.and(terminal::check(
        terminal::delete_images(layout.transmitted_ids()),
        "delete_images",
        Some(image_mode),
        "exit",
    ))
}

impl InputQueue {
    fn pop_replayed(&mut self) -> Option<KeyEvent> {
        self.replay.pop_front()
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
            return false;
        };
        for key in dispatch.replay.into_iter().rev() {
            self.replay.push_front(key);
        }
        match dispatch.action {
            InputAction::Builtin(action) => {
                session.borrow_mut().app.apply(action, rows, search_text);
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
    use crate::app::{Action, Mode};
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
