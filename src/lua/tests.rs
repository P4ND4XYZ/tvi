use std::{cell::Cell, fs, io};

use crate::ui::float::Floats;

use super::*;

#[derive(Default)]
struct TestHost {
    view: Cell<u64>,
    closed: Cell<bool>,
    document: Cell<Option<DocumentInfo>>,
    floats: RefCell<Floats>,
    opened_files: RefCell<Vec<(u64, PathBuf)>>,
    source_lines: RefCell<Vec<i64>>,
    zooms: RefCell<Vec<f64>>,
    horizontal_moves: RefCell<Vec<i64>>,
}

impl TestHost {
    fn shows(&self, kind: DocumentKind) {
        self.document.set(Some(DocumentInfo { epoch: 1, kind }));
    }
}

impl Host for TestHost {
    fn current_view(&self) -> Result<u64, String> {
        Ok(self.view.get())
    }

    fn notify(&self, _message: &str) {}

    fn document(&self, view: u64) -> Result<Option<DocumentInfo>, String> {
        if view != 0 || self.closed.get() {
            return Err("view is closed".into());
        }
        Ok(self.document.get())
    }

    fn document_path(&self, _view: u64) -> Result<PathBuf, String> {
        Ok(std::env::temp_dir().join("displayed"))
    }

    fn project_root(&self, _view: u64, _markers: &[String]) -> Result<PathBuf, String> {
        Ok(std::env::temp_dir())
    }

    fn links(&self, _view: u64) -> Result<Vec<LuaLink>, String> {
        Ok(Vec::new())
    }

    fn resolve_link(&self, view: u64, id: u64) -> Result<PathBuf, String> {
        match (view, id) {
            (0, 1) => Ok(std::env::temp_dir().join("resolved.md")),
            _ => Err("unknown link".into()),
        }
    }

    fn open_file(&self, view: u64, path: &Path) -> Result<FileOpen, String> {
        self.opened_files.borrow_mut().push((view, path.to_owned()));
        Ok(FileOpen::Opened)
    }

    fn scroll_to_line(&self, view: u64, line: i64) -> Result<(), String> {
        if view != 0 {
            return Err("view is closed".into());
        }
        self.source_lines.borrow_mut().push(line);
        Ok(())
    }

    fn pdf_zoom(&self, _view: u64) -> Result<f64, String> {
        Ok(1.0)
    }

    fn set_pdf_zoom(&self, _view: u64, scale: f64) -> Result<(), String> {
        self.zooms.borrow_mut().push(scale);
        Ok(())
    }

    fn pan_pdf_x(&self, _view: u64, columns: i64) -> Result<(), String> {
        self.horizontal_moves.borrow_mut().push(columns);
        Ok(())
    }

    fn open_float(&self, view: u64, options: FloatOptions) -> Result<u64, String> {
        self.floats.borrow_mut().open(view, options)
    }

    fn update_float(&self, view: u64, float: u64, options: FloatOptions) -> Result<(), String> {
        self.floats.borrow_mut().update(view, float, options)
    }

    fn close_float(&self, view: u64, float: u64) -> Result<(), String> {
        self.floats.borrow_mut().close(view, float)
    }
}

#[test]
fn local_listener_reports_bind_errors_and_rebinds_after_close_or_failed_init()
-> Result<(), Box<dyn std::error::Error>> {
    use std::net::{Ipv4Addr, TcpListener};

    let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = reserved.local_addr()?.port();
    let host = Rc::new(TestHost::default());
    let runtime = Runtime::new(host.clone());
    runtime.install_api()?;
    runtime.lua.globals().set("port", port)?;
    runtime
        .lua
        .load(
            r#"
        local noop = function() end
        for _, invalid in ipairs({0, -1, 65536, 1.5, 1.0, '80', false}) do
            local handle, err = tvi.net.listen_local(invalid, noop)
            assert(handle == nil and err:find('integer', 1, true))
        end
        local handle, err = tvi.net.listen_local(port, noop)
        assert(handle == nil and err:find('cannot listen', 1, true))
    "#,
        )
        .exec()?;
    drop(reserved);
    runtime
        .lua
        .load(
            r#"
        local noop = function() end
        local first = assert(tvi.net.listen_local(port, noop))
        assert(tvi.net.listen_local(port, noop) == nil)
        first:close()
        first:close()
        local second = assert(tvi.net.listen_local(port, noop))
        first:close()
        assert(tvi.net.listen_local(port, noop) == nil)
        second:close()
    "#,
        )
        .exec()?;
    let scratch = Scratch::new()?;
    let init = scratch.0.join("init.lua");
    fs::write(
        &init,
        format!("assert(tvi.net.listen_local({port}, function() end)); error('init failed')"),
    )?;
    let (_fallback, error) = Runtime::load_from(host, Some(init));
    assert!(error.is_some_and(|error| error.contains("init failed")));
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    drop(socket);
    runtime
        .lua
        .load("assert(tvi.net.listen_local(port, function() end))")
        .exec()?;
    drop(runtime);
    let _socket = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    Ok(())
}

#[test]
fn local_listener_stops_callbacks_in_the_same_poll_after_close()
-> Result<(), Box<dyn std::error::Error>> {
    use std::{
        io::Write,
        net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
        time::{Duration, Instant},
    };
    let reserved = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let runtime = Runtime::new(Rc::new(TestHost::default()));
    runtime.install_api()?;
    runtime.lua.globals().set("port", address.port())?;
    runtime
        .lua
        .load(
            r#"
        calls = 0
        local handle
        handle = assert(tvi.net.listen_local(port, function(payload)
            assert(payload == 'one' or payload == 'two')
            calls = calls + 1
            handle:close()
            error('callback error is isolated')
        end))
    "#,
        )
        .exec()?;
    for line in [b"one\n", b"two\n"] {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        stream.write_all(line)?;
        stream.shutdown(Shutdown::Write)?;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while !runtime.poll_net(Instant::now(), || true) {
        assert!(Instant::now() < deadline, "no callback");
        std::thread::yield_now();
    }
    assert!(!runtime.poll_net(Instant::now(), || true));
    assert_eq!(runtime.lua.globals().get::<u32>("calls")?, 1);
    Ok(())
}

#[test]
fn scroll_to_line_accepts_only_lua_integers_and_returns_nothing() -> mlua::Result<()> {
    let host = Rc::new(TestHost::default());
    host.shows(DocumentKind::Markdown);
    let runtime = Runtime::new(host.clone());
    runtime.install_api()?;
    runtime.lua.load(r##"
        doc = tvi.ui.current_view():document()
        for _, line in ipairs({ "120", 120.0, 120.5, {}, true, function() end }) do
            local ok, err = pcall(doc.scroll_to_line, doc, line)
            assert(not ok and tostring(err):find("scroll_to_line: line must be an integer", 1, true))
        end
        for _, args in ipairs({table.pack(), table.pack(nil)}) do
            local ok, err = pcall(doc.scroll_to_line, doc, table.unpack(args, 1, args.n))
            assert(not ok and tostring(err):find("scroll_to_line: line must be an integer", 1, true))
        end
        for _, line in ipairs({0, math.mininteger, math.maxinteger}) do
            assert(select("#", doc:scroll_to_line(line)) == 0)
        end
    "##).exec()?;
    assert_eq!(*host.source_lines.borrow(), [0, i64::MIN, i64::MAX]);

    host.closed.set(true);
    runtime
        .lua
        .load(
            r#"
        local ok, err = pcall(doc.scroll_to_line, doc, 1)
        assert(not ok and tostring(err):find("view is closed", 1, true))
        local path, path_err = doc:path()
        assert(path == nil and path_err == "view is closed")
        assert(not pcall(function() return tvi.ui.current_view():document() end))
    "#,
        )
        .exec()?;
    assert_eq!(host.source_lines.borrow().len(), 3);
    Ok(())
}

#[test]
fn view_returns_only_the_displayed_formats_document_methods() -> mlua::Result<()> {
    let host = Rc::new(TestHost::default());
    let runtime = Runtime::new(host.clone());
    runtime.install_api()?;
    runtime.lua.load(r##"
        view = tvi.ui.current_view()
        assert(type(view.open_file) == "function" and type(view.open_float) == "function")
        for _, name in ipairs({"path", "project_root", "links", "resolve_link", "scroll_to_line", "zoom", "set_zoom", "pan_x"}) do
            assert(view[name] == nil, name)
        end
        local doc = view:document()
        assert(doc == nil and select("#", view:document()) == 1)
    "##).exec()?;

    host.shows(DocumentKind::Markdown);
    runtime
        .lua
        .load(
            r##"
        md = view:document()
        assert(type(md) == "userdata" and md:type() == "md")
        assert(select("#", view:document()) == 1)
        assert(md:path() and md:project_root() and #md:links() == 0 and md:resolve_link(1))
        assert(md.zoom == nil and md.set_zoom == nil and md.pan_x == nil)
    "##,
        )
        .exec()?;

    host.shows(DocumentKind::Pdf);
    runtime
        .lua
        .load(
            r##"
        local pdf = view:document()
        assert(type(pdf) == "userdata" and pdf:type() == "pdf")
        assert(pdf:path() and pdf:project_root())
        assert(pdf.links == nil and pdf.resolve_link == nil and pdf.scroll_to_line == nil)
    "##,
        )
        .exec()?;
    Ok(())
}

#[test]
fn pdf_zoom_and_pan_validate_lua_values_and_reject_expired_handles() -> mlua::Result<()> {
    let host = Rc::new(TestHost::default());
    host.shows(DocumentKind::Pdf);
    let runtime = Runtime::new(host.clone());
    runtime.install_api()?;
    runtime
        .lua
        .load(
            r"
        doc = tvi.ui.current_view():document()
        assert(doc:zoom() == 1.0)
        for _, invalid in ipairs({0, -1, 1/0, -1/0, 0/0, '2.0', true, {}}) do
            local ok, err = doc:set_zoom(invalid)
            assert(ok == false and type(err) == 'string')
        end
        local ok, err = doc:set_zoom()
        assert(ok == false and type(err) == 'string')
        for _, scale in ipairs({2, 0.625, 1.0}) do
            local result = table.pack(doc:set_zoom(scale))
            assert(result.n == 2 and result[1] == true and result[2] == nil)
        end
        for _, invalid in ipairs({1.0, 1.5, '2', true, {}}) do
            assert(not pcall(doc.pan_x, doc, invalid))
        end
        assert(not pcall(doc.pan_x, doc))
        for _, columns in ipairs({0, math.mininteger, math.maxinteger}) do
            assert(select('#', doc:pan_x(columns)) == 0)
        end
    ",
        )
        .exec()?;

    for kind in [DocumentKind::Pdf, DocumentKind::Markdown] {
        host.document.set(Some(DocumentInfo { epoch: 2, kind }));
        runtime
            .lua
            .load(
                r"
            assert(not pcall(doc.zoom, doc))
            local ok, err = doc:set_zoom(1.5)
            assert(ok == false and err == 'document is no longer displayed')
            assert(not pcall(doc.pan_x, doc, 1))
        ",
            )
            .exec()?;
    }
    assert_eq!(*host.zooms.borrow(), [2.0, 0.625, 1.0]);
    assert_eq!(*host.horizontal_moves.borrow(), [0, i64::MIN, i64::MAX]);
    Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        for suffix in 0..100 {
            let path =
                std::env::temp_dir().join(format!("tvi-lua-{}-{suffix}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("could not allocate Lua test directory"))
    }

    fn init(&self, script: &str) -> io::Result<PathBuf> {
        let path = self.0.join("init.lua");
        fs::write(&path, script)?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("could not clean {}: {error}", self.0.display());
        }
    }
}

#[test]
fn colorscheme_validates_atomically_and_replaces_overrides() -> mlua::Result<()> {
    use crate::colorscheme::Group;
    use ratatui::style::{Color, Modifier, Style};

    let (runtime, diagnostic) = Runtime::with_defaults(Rc::new(TestHost::default()));
    assert_eq!(diagnostic, None);
    runtime
        .lua
        .load(
            r##"
        local set = tvi.colorscheme.set
        set({ ["ui.status"] = { fg = "#1aB2c3", reverse = false } })
        local invalid = {
            { false, "styles" },
            { { [1] = {} }, "group key" },
            { { typo = {} }, "typo" },
            { { ["ui.status"] = true }, "ui.status" },
            { { ["ui.status"] = { [1] = false } }, "attribute key" },
            { { ["ui.status"] = { bold = 1 } }, "ui.status.bold" },
            { { ["ui.status"] = { fg = 1 } }, "ui.status.fg" },
            { { ["ui.status"] = { fg = "cyan " } }, "ui.status.fg" },
            { { ["ui.status"] = { fg = "#12" } }, "ui.status.fg" },
            { { ["ui.status"] = { fg = "#+12345" } }, "ui.status.fg" },
            { { ["ui.status"] = { fg = string.char(255) } }, "ui.status.fg" },
            { { ["ui.status"] = { color = "cyan" } }, "ui.status.color" },
        }
        for _, case in ipairs(invalid) do
            local ok, err = pcall(set, case[1])
            assert(not ok and tostring(err):find(case[2], 1, true), tostring(err))
        end
    "##,
        )
        .exec()?;
    let builtin = Style::new().add_modifier(Modifier::REVERSED);
    assert_eq!(
        runtime.colorscheme().apply(Group::Status, builtin),
        Style::new()
            .fg(Color::Rgb(0x1a, 0xb2, 0xc3))
            .remove_modifier(Modifier::REVERSED)
    );
    runtime
        .lua
        .load(
            r#"
        tvi.colorscheme.set({ ["ui.prompt"] = { fg = "default", bg = "bright_red" } })
    "#,
        )
        .exec()?;
    assert_eq!(runtime.colorscheme().apply(Group::Status, builtin), builtin);
    assert_eq!(
        runtime
            .colorscheme()
            .apply(Group::Prompt, Style::new().fg(Color::Blue)),
        Style::new().fg(Color::Reset).bg(Color::LightRed),
    );
    runtime.lua.load("tvi.colorscheme.set({})").exec()?;
    assert_eq!(
        runtime.colorscheme().apply(Group::Prompt, Style::new()),
        Style::new()
    );
    Ok(())
}

#[test]
fn plugin_tests() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let runner = root.join("plugins/tests/testrunner.lua");
    let runtime = Runtime::new(Rc::new(TestHost::default()));
    runtime.install_api()?;
    config::install_searcher(&runtime.lua, &root.join("init.lua"))?;
    config::source_chunk(&runtime.lua, &runner, &fs::read(&runner)?).exec()?;
    Ok(())
}

#[test]
fn config_relative_module_can_register_a_mapping_that_uses_a_captured_view()
-> Result<(), Box<dyn std::error::Error>> {
    let config = Scratch::new()?;
    fs::create_dir(config.0.join("modules"))?;
    fs::write(
        config.0.join("modules/action.lua"),
        r#"
        local view = tvi.ui.current_view()
        return function()
            local path = assert(view:document():resolve_link(1))
            assert(view:open_file(path))
        end
        "#,
    )?;
    let init = config.init(
        r#"
        local action = require("modules.action")
        tvi.keymap.set("normal", "gl", action)
        "#,
    )?;
    let host = Rc::new(TestHost::default());
    host.shows(DocumentKind::Markdown);
    let (runtime, diagnostic) = Runtime::load_from(host.clone(), Some(init));
    assert_eq!(diagnostic, None);
    host.view.set(17);
    runtime.run_mapping(1)?;
    assert_eq!(
        &*host.opened_files.borrow(),
        &[(0, std::env::temp_dir().join("resolved.md"))]
    );
    Ok(())
}

#[test]
fn broken_config_closes_its_float_and_discards_partial_mappings()
-> Result<(), Box<dyn std::error::Error>> {
    let config = Scratch::new()?;
    let init = config.init(
        r#"
        local view = tvi.ui.current_view()
        tvi.keymap.set("normal", "gl", function() error("discard me") end)
        view:open_float({})
        finalizer = setmetatable({}, { __gc = function() view:open_float({}) end })
        tvi.keymap.set("normal", "gg", function() end)
        "#,
    )?;
    let host = Rc::new(TestHost::default());
    let (runtime, diagnostic) = Runtime::load_from(host.clone(), Some(init));
    let diagnostic = diagnostic.ok_or("expected a configuration diagnostic")?;
    assert!(diagnostic.starts_with("init.lua:"));
    assert!(diagnostic.contains("built-in mapping"));
    assert_eq!(host.floats.borrow().active(), None);
    assert_eq!(
        runtime.run_mapping(1).err().as_deref(),
        Some("key mapping has expired")
    );
    Ok(())
}

#[test]
fn startup_and_callback_diagnostics_show_the_reason_before_source_details()
-> Result<(), Box<dyn std::error::Error>> {
    let config = Scratch::new()?;
    fs::write(config.0.join("test_module.lua"), "error('module problem')")?;
    let init = config.init("require('test_module')")?;
    let host = Rc::new(TestHost::default());
    let (_, diagnostic) = Runtime::load_from(host.clone(), Some(init));
    assert_eq!(
        diagnostic.as_deref().and_then(|text| text.lines().next()),
        Some("init.lua: module problem (test_module.lua:1)")
    );
    let init =
        config.init("tvi.keymap.set('normal', 'gl', function() error('mapping problem') end)")?;
    let (runtime, diagnostic) = Runtime::load_from(host, Some(init));
    assert_eq!(diagnostic, None);
    assert_eq!(
        runtime
            .run_mapping(1)
            .err()
            .as_deref()
            .and_then(|text| text.lines().next()),
        Some("mapping problem (init.lua:1)")
    );
    Ok(())
}

#[test]
fn a_non_regular_config_file_is_reported() -> io::Result<()> {
    let config = Scratch::new()?;
    let init = config.0.join("init.lua");
    fs::create_dir(&init)?;
    let (_, diagnostic) = Runtime::load_from(Rc::new(TestHost::default()), Some(init));
    assert_eq!(diagnostic.as_deref(), Some("init.lua: not a regular file"));
    Ok(())
}

#[test]
fn os_exit_reports_a_configuration_error_instead_of_terminating() -> io::Result<()> {
    let config = Scratch::new()?;
    let init = config.init("os.exit(42)")?;
    let (_, diagnostic) = Runtime::load_from(Rc::new(TestHost::default()), Some(init));
    assert_eq!(
        diagnostic.as_deref().and_then(|text| text.lines().next()),
        Some("init.lua: os.exit is unavailable while tvi is running")
    );
    Ok(())
}

#[test]
fn process_passes_literal_arguments_and_cwd_and_preserves_nul_stdout()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = Scratch::new()?;
    let cwd = scratch.0.join("資料 space");
    fs::create_dir(&cwd)?;
    fs::write(cwd.join("cwd-marker"), "process fixture")?;

    let runtime = Runtime::new(Rc::new(TestHost::default()));
    runtime.install_api()?;
    let exe = std::env::current_exe()?;
    runtime
        .lua
        .globals()
        .set("exe", exe.to_str().ok_or("non-UTF-8 test executable")?)?;
    runtime
        .lua
        .globals()
        .set("cwd", cwd.to_str().ok_or("non-UTF-8 scratch path")?)?;
    runtime
        .lua
        .load(
            r#"
        local result = assert(tvi.process.run(exe, {
            cwd = cwd,
            args = { "--ignored", "--nocapture", "--exact", "lua::tests::process_fixture",
                     "tvi-cwd-fixture", '資料 a "quoted" & |' },
        }))
        assert(result.ok and result.code == 0, result.stdout)
        assert(result.stdout:find("TVI\0fixture", 1, true), result.stdout)
    "#,
        )
        .exec()?;
    Ok(())
}

#[test]
#[ignore = "run in a child test harness as a process fixture"]
fn process_fixture() -> io::Result<()> {
    use std::io::Write as _;

    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|arg| arg == "tvi-cwd-fixture") {
        assert_eq!(
            args.last().map(String::as_str),
            Some("資料 a \"quoted\" & |")
        );
        assert_eq!(fs::read_to_string("cwd-marker")?, "process fixture");
        io::stdout().write_all(b"TVI\0fixture")?;
    }
    Ok(())
}

#[test]
fn process_distinguishes_missing_executable_invalid_cwd_and_nonzero_exit()
-> Result<(), Box<dyn std::error::Error>> {
    let scratch = Scratch::new()?;
    let runtime = Runtime::new(Rc::new(TestHost::default()));
    runtime.install_api()?;
    let exe = std::env::current_exe()?;
    let missing = scratch.0.join("missing.exe");
    let bad_cwd = scratch.0.join("no-directory");
    for (key, path) in [("exe", exe), ("missing", missing), ("bad_cwd", bad_cwd)] {
        runtime
            .lua
            .globals()
            .set(key, path.to_str().ok_or("non-UTF-8 test path")?)?;
    }
    runtime
        .lua
        .load(
            r#"
        local result, err = tvi.process.run(missing, {})
        assert(result == nil and err.kind == "not_found")
        result, err = tvi.process.run(exe, { cwd = bad_cwd })
        assert(result == nil and err.kind == "cwd")
        result, err = tvi.process.run(exe, { args = { "--tvi-invalid-argument" } })
        assert(result and not result.ok and result.code ~= 0 and err == nil)
    "#,
        )
        .exec()?;
    Ok(())
}

#[test]
fn closing_during_callback_or_from_host_expires_only_that_float()
-> Result<(), Box<dyn std::error::Error>> {
    let host = Rc::new(TestHost::default());
    let runtime = Runtime::new(host.clone());
    runtime.install_api()?;
    runtime
        .lua
        .load(
            r#"
        local view = tvi.ui.current_view()
        original = view:open_float({
          on_key = function(float, key)
            assert(key.name == "Down" and key.text == nil)
            float:close()
            view:open_float({ on_key = function() error("replacement callback") end })
          end,
        })
    "#,
        )
        .exec()?;
    let (view, original) = host.floats.borrow().active().ok_or("expected a float")?;
    let send = |float| {
        runtime.run_float_key(
            view,
            float,
            PluginKey {
                name: "Down".into(),
                text: None,
            },
        )
    };
    send(original)?;
    let (_, replacement) = host
        .floats
        .borrow()
        .active()
        .ok_or("expected a replacement")?;
    assert_ne!(original, replacement);
    runtime
        .lua
        .load("assert(not pcall(function() original:update({}) end))")
        .exec()?;
    send(original)?;
    let error = send(replacement).err().ok_or("expected a callback error")?;
    assert!(error.contains("replacement callback"));
    host.close_float(view, replacement)?;
    runtime.forget_float(replacement);
    send(replacement)?;
    Ok(())
}
