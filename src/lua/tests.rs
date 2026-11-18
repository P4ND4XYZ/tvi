use std::{cell::Cell, fs, io};

use crate::ui::float::Floats;

use super::*;

#[derive(Default)]
struct TestHost {
    view: Cell<u64>,
    floats: RefCell<Floats>,
    opened_links: RefCell<Vec<(u64, u64)>>,
}

impl Host for TestHost {
    fn current_view(&self) -> Result<u64, String> {
        Ok(self.view.get())
    }

    fn links(&self, _view: u64) -> Result<Vec<LuaLink>, String> {
        Ok(Vec::new())
    }

    fn open_link(&self, view: u64, id: u64) -> Result<(), String> {
        self.opened_links.borrow_mut().push((view, id));
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
fn plugin_tests() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let runner = root.join("plugins/tests/testrunner.lua");
    let lua = Lua::new();
    config::install_searcher(&lua, &root.join("init.lua"))?;
    config::source_chunk(&lua, &runner, &fs::read(&runner)?).exec()?;
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
            assert(view:open_link(1))
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
    let (runtime, diagnostic) = Runtime::load_from(host.clone(), Some(init));
    assert_eq!(diagnostic, None);
    host.view.set(17);
    runtime.run_mapping(0)?;
    assert_eq!(&*host.opened_links.borrow(), &[(0, 1)]);
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
        runtime.run_mapping(0).err().as_deref(),
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
            .run_mapping(0)
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
