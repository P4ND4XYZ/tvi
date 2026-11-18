use std::{
    cell::RefCell, collections::VecDeque, fs, io, path::Path, rc::Rc, string::FromUtf8Error,
};

use crossterm::event::KeyEvent;

use crate::{
    app::{App, Document},
    lua::{Host, LuaLink, Runtime},
    markdown::{MarkdownRenderer, tui_markdown::TuiMarkdown},
    terminal::{self, Screen},
    ui::{
        self,
        float::{FloatOptions, Floats},
        input::{Input, InputAction, is_escape, plugin_key},
        layout::Layout,
    },
};

const BODY_VIEW: u64 = 1;

struct Session {
    app: App,
    floats: Floats,
}

#[derive(Default)]
struct InputQueue {
    input: Input,
    replay: VecDeque<KeyEvent>,
}

impl Session {
    fn new(document: Document) -> Self {
        Self {
            app: App::from_document(document),
            floats: Floats::default(),
        }
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

fn load_document(path: &Path) -> Result<Document, ReadDocumentError> {
    let bytes = fs::read(path).map_err(ReadDocumentError::Read)?;
    let markdown = String::from_utf8(bytes).map_err(ReadDocumentError::Decode)?;
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .replace(char::is_control, "�");
    let links = TuiMarkdown
        .links(&markdown)
        .into_iter()
        .map(|link| (link.label, link.target))
        .collect();
    Ok(Document::new(name, markdown, path.to_owned(), links))
}

fn require_regular_file(path: &Path) -> io::Result<()> {
    if fs::metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "link target is not a regular file",
        ))
    }
}

impl Host for RefCell<Session> {
    fn current_view(&self) -> Result<u64, String> {
        Ok(BODY_VIEW)
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

    fn open_link(&self, view: u64, id: u64) -> Result<(), String> {
        validate_view(view)?;
        let path = self.borrow().app.resolve_link(id)?;
        require_regular_file(&path).map_err(|error| format!("cannot read: {error}"))?;
        let document = load_document(&path).map_err(|error| match error {
            ReadDocumentError::Read(error) => format!("cannot read: {error}"),
            ReadDocumentError::Decode(error) => format!("not valid UTF-8: {error}"),
        })?;
        self.borrow_mut().app.open_document(document);
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

pub fn run(screen: &mut Screen, document: Document) -> io::Result<()> {
    let session = Rc::new(RefCell::new(Session::new(document)));
    let (runtime, error) = Runtime::load(session.clone());
    if let Some(error) = error {
        session.borrow_mut().restore_after_init_error(&error);
    }

    let mut keys = InputQueue::default();
    while !session.borrow().app.should_quit() {
        run_document(screen, &session, &runtime, &mut keys)?;
    }
    Ok(())
}

fn run_document(
    screen: &mut Screen,
    session: &RefCell<Session>,
    runtime: &Runtime,
    keys: &mut InputQueue,
) -> io::Result<()> {
    let (document_id, document) = {
        let session = session.borrow();
        let app = &session.app;
        (
            app.document_id(),
            ui::prepare(&TuiMarkdown, app.markdown(), screen.size()?.width),
        )
    };
    let mut width = document.width();
    let mut layout = Layout::new(&document);
    let mut anchors = ui::anchors(&layout);
    loop {
        let mut drawn_size = (0, 0);
        screen.draw(|frame| {
            let area = frame.area();
            drawn_size = (area.width, area.height);
            if width != area.width {
                width = area.width;
                layout = Layout::at_width(&document, width);
                anchors = ui::anchors(&layout);
            }
            let mut session = session.borrow_mut();
            let body = ui::body_area(area, session.app.mode());
            session.app.reflow(&anchors, usize::from(body.height));
            ui::draw(
                frame,
                &session.app,
                &layout,
                session.floats.active().is_some(),
            );
            session.floats.draw(frame, body);
        })?;
        loop {
            let key = keys.next_key()?;
            if keys.dispatch(key, session, runtime, &anchors, document.search_text()) {
                let app = &session.borrow().app;
                if app.should_quit() || app.document_id() != document_id {
                    return Ok(());
                }
                break;
            }
            let size = screen.size()?;
            if (size.width, size.height) != drawn_size {
                break;
            }
        }
    }
}

impl InputQueue {
    fn next_key(&mut self) -> io::Result<Option<KeyEvent>> {
        if let Some(key) = self.replay.pop_front() {
            Ok(Some(key))
        } else {
            terminal::poll_key()
        }
    }

    fn dispatch(
        &mut self,
        key: Option<KeyEvent>,
        session: &RefCell<Session>,
        runtime: &Runtime,
        anchors: &[usize],
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
                session.borrow_mut().app.apply(action, anchors, search_text);
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
        path::PathBuf,
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
    fn invalid_link_and_failed_init_leave_the_original_document() -> Result<(), Box<dyn Error>> {
        let scratch = Scratch::new()?;
        let docs = scratch.0.join("docs");
        fs::create_dir(&docs)?;
        fs::write(scratch.0.join("invalid.md"), [0xff])?;
        fs::write(scratch.0.join("next.md"), "Next document")?;
        fs::create_dir(scratch.0.join("directory.md"))?;
        let source = docs.join("source.md");
        fs::write(
            &source,
            "[Invalid](../invalid.md) [Next](../next.md) [Directory](../directory.md)",
        )?;
        let session = Rc::new(RefCell::new(Session::new(read_document(&source)?)));
        let initial_id = session.borrow().app.document_id();
        let invalid_link = session.borrow().app.links()[0].id;
        assert!(matches!(
            session.open_link(BODY_VIEW, invalid_link),
            Err(reason) if reason.contains("not valid UTF-8")
        ));
        let directory_link = session.borrow().app.links()[2].id;
        assert!(matches!(
            session.open_link(BODY_VIEW, directory_link),
            Err(reason) if reason.contains("not a regular file")
        ));
        assert_eq!(session.borrow().app.document_id(), initial_id);

        let init = scratch.init(
            r#"
local view = tvi.ui.current_view()
assert(view:open_link(view:links()[2].id))
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
        assert_eq!(session.app.links()[0].id, invalid_link);
        assert!(session.app.notice().is_some_and(|notice| {
            notice.contains("init.lua") && notice.contains("startup failed")
        }));
        session.app.apply(Action::JumpBack, &[], "");
        assert_eq!(session.app.document_id(), initial_id);
        Ok(())
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
            let replayed = keys.next_key()?;
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
