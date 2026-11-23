use std::{
    cell::{Ref, RefCell},
    io::ErrorKind,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
};

use mlua::{
    Error, Function, IntoLuaMulti, Lua, RegistryKey, Table, UserData, UserDataMethods, Value,
};

use crate::{
    colorscheme::Colorscheme,
    logging::{self, Level},
    ui::{
        float::FloatOptions,
        input::{Keymap, KeymapMode, PluginKey},
    },
};

mod colorscheme;
mod config;
mod document_handle;
mod net;
mod process;
#[cfg(test)]
mod tests;

pub use document_handle::{DocumentInfo, DocumentKind};

pub struct LuaLink {
    pub id: u64,
    pub label: String,
    pub target: String,
}

pub enum FileOpen {
    Opened,
    Pending,
}

pub trait Host {
    fn current_view(&self) -> Result<u64, String>;
    fn notify(&self, message: &str);
    fn document(&self, view: u64) -> Result<Option<DocumentInfo>, String>;
    fn document_path(&self, view: u64) -> Result<PathBuf, String>;
    fn project_root(&self, view: u64, markers: &[String]) -> Result<PathBuf, String>;
    fn links(&self, view: u64) -> Result<Vec<LuaLink>, String>;
    fn resolve_link(&self, view: u64, id: u64) -> Result<PathBuf, String>;
    fn open_file(&self, view: u64, path: &Path) -> Result<FileOpen, String>;
    fn supports_pdf(&self) -> bool {
        false
    }
    fn scroll_to_line(&self, view: u64, line: i64) -> Result<(), String>;
    fn pdf_zoom(&self, view: u64) -> Result<f64, String>;
    fn set_pdf_zoom(&self, view: u64, scale: f64) -> Result<(), String>;
    fn pan_pdf_x(&self, view: u64, columns: i64) -> Result<(), String>;
    fn open_float(&self, view: u64, options: FloatOptions) -> Result<u64, String>;
    fn update_float(&self, view: u64, float: u64, options: FloatOptions) -> Result<(), String>;
    fn close_float(&self, view: u64, float: u64) -> Result<(), String>;
}

pub struct Runtime {
    // Lua の終了時に実行されるファイナライザからホストを呼べないようにする。
    state: Rc<State>,
    lua: Lua,
}

struct State {
    host: Rc<dyn Host>,
    keymap: RefCell<Keymap>,
    colorscheme: RefCell<Colorscheme>,
    mappings: RefCell<Vec<RegistryKey>>,
    float: RefCell<Option<ActiveFloat>>,
    listener: RefCell<Option<net::Active>>,
}

struct ActiveFloat {
    view: u64,
    id: u64,
    callback: Option<RegistryKey>,
}

impl Runtime {
    pub fn load(host: Rc<dyn Host>) -> (Self, Option<String>) {
        Self::load_from(host, config::init_path())
    }

    fn new(host: Rc<dyn Host>) -> Self {
        Self {
            state: Rc::new(State {
                host,
                keymap: RefCell::default(),
                colorscheme: RefCell::default(),
                mappings: RefCell::default(),
                float: RefCell::default(),
                listener: RefCell::default(),
            }),
            lua: Lua::new(),
        }
    }

    pub(crate) fn load_from(host: Rc<dyn Host>, path: Option<PathBuf>) -> (Self, Option<String>) {
        let (runtime, error) = Self::with_defaults(Rc::clone(&host));
        if error.is_some() {
            return (runtime, error);
        }
        let Some(path) = path else {
            return (runtime, None);
        };
        let filename = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        let source = match config::read_source(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => return (runtime, None),
            Err(error) => {
                logging::emit(
                    Level::Warn,
                    "lua.failed",
                    serde_json::json!({
                        "source":"init.lua", "stage":"config_read", "reason":logging::io_reason(&error),
                        "action":"use_defaults",
                    }),
                );
                return (runtime, Some(format!("{filename}: {error}")));
            }
        };
        match runtime.configure(&path, &source) {
            Ok(()) => (runtime, None),
            Err(error) => {
                log_failure("config", "use_defaults", &error, Some("init.lua"));
                let mut message = lua_diagnostic(&error, Some(&filename));
                let active = runtime.state.float.borrow_mut().take();
                if let Some(active) = active
                    && let Err(error) = host.close_float(active.view, active.id)
                {
                    logging::emit(
                        Level::Warn,
                        "lua.failed",
                        serde_json::json!({
                            "stage":"float_close", "reason":"host_error", "action":"reset_session",
                        }),
                    );
                    message.push_str("\ncannot close configuration float: ");
                    message.push_str(&error);
                }
                drop(runtime);
                let (runtime, fallback_error) = Self::with_defaults(host);
                if let Some(error) = fallback_error {
                    message.push('\n');
                    message.push_str(&error);
                }
                (runtime, Some(message))
            }
        }
    }

    fn with_defaults(host: Rc<dyn Host>) -> (Self, Option<String>) {
        let runtime = Self::new(host);
        if let Err(error) = runtime
            .install_api()
            .and_then(|()| runtime.install_defaults())
        {
            log_failure("defaults", "continue", &error, None);
            return (runtime, Some(lua_diagnostic(&error, None)));
        }
        (runtime, None)
    }

    fn install_defaults(&self) -> mlua::Result<()> {
        let files: Table = config::source_chunk(
            &self.lua,
            Path::new("plugins/files.lua"),
            include_bytes!("../plugins/files.lua"),
        )
        .eval()?;
        let callback = self
            .lua
            .create_registry_value(files.get::<Function>("open")?)?;
        let package: Table = self.lua.globals().get("package")?;
        let loaded: Table = package.get("loaded")?;
        loaded.set("plugins.files", files)?;
        self.state
            .keymap
            .borrow_mut()
            .register_default(KeymapMode::Normal, "ff", 0)
            .map_err(Error::runtime)?;
        self.state.mappings.borrow_mut().push(callback);
        Ok(())
    }

    fn configure(&self, path: &Path, source: &[u8]) -> mlua::Result<()> {
        config::install_searcher(&self.lua, path)?;
        let filename = path.file_name().map_or(path, Path::new);
        config::source_chunk(&self.lua, filename, source).exec()
    }

    fn install_api(&self) -> mlua::Result<()> {
        // os.exit は Rust 側の端末復元を通らずにプロセスを終了する。
        let os: Table = self.lua.globals().get("os")?;
        os.set(
            "exit",
            self.lua.create_function(|_, ()| {
                Err::<(), _>(Error::runtime(
                    "os.exit is unavailable while tvi is running",
                ))
            })?,
        )?;

        let tvi = self.lua.create_table()?;
        let keymap = self.lua.create_table()?;
        let state = Rc::downgrade(&self.state);
        keymap.set(
            "set",
            self.lua.create_function(
                move |lua, (mode, keys, callback, opts): (String, String, Function, Option<Table>)| {
                    let mode = match mode.as_str() {
                        "normal" => KeymapMode::Normal,
                        "command" => KeymapMode::Command,
                        "search" => KeymapMode::Search,
                        _ => return Err(Error::runtime(format!("unknown keymap mode: {mode}"))),
                    };
                    let desc = match opts {
                        Some(opts) => opts.get("desc")?,
                        None => None,
                    };
                    let state = live_state(&state)?;
                    let mut keymap = state.keymap.try_borrow_mut().map_err(Error::external)?;
                    let mut mappings = state.mappings.borrow_mut();
                    let id = u64::try_from(mappings.len()).map_err(Error::external)?;
                    let callback = lua.create_registry_value(callback)?;
                    keymap.register(mode, &keys, id, desc).map_err(Error::runtime)?;
                    mappings.push(callback);
                    Ok(())
                },
            )?,
        )?;
        tvi.set("keymap", keymap)?;

        let ui = self.lua.create_table()?;
        let state = Rc::downgrade(&self.state);
        ui.set(
            "current_view",
            self.lua.create_function(move |_, ()| {
                let id = live_state(&state)?
                    .host
                    .current_view()
                    .map_err(Error::runtime)?;
                Ok(ViewHandle {
                    id,
                    state: state.clone(),
                })
            })?,
        )?;
        let state = Rc::downgrade(&self.state);
        ui.set(
            "notify",
            self.lua.create_function(move |_, message: String| {
                live_state(&state)?.host.notify(&message);
                Ok(())
            })?,
        )?;
        let state = Rc::downgrade(&self.state);
        ui.set(
            "supports_pdf",
            self.lua
                .create_function(move |_, ()| Ok(live_state(&state)?.host.supports_pdf()))?,
        )?;
        tvi.set("ui", ui)?;
        process::install(&self.lua, &tvi)?;
        net::install(&self.lua, &tvi, Rc::downgrade(&self.state))?;
        colorscheme::install(&self.lua, &tvi, Rc::downgrade(&self.state))?;
        self.lua.globals().set("tvi", tvi)
    }

    pub fn keymap(&self) -> Ref<'_, Keymap> {
        self.state.keymap.borrow()
    }

    pub(crate) fn colorscheme(&self) -> Ref<'_, Colorscheme> {
        self.state.colorscheme.borrow()
    }

    pub fn run_mapping(&self, id: u64) -> Result<(), String> {
        self.call_mapping(id).map_err(|error| {
            log_failure("key", "continue", &error, None);
            lua_diagnostic(&error, None)
        })
    }

    fn call_mapping(&self, id: u64) -> mlua::Result<()> {
        let callback = {
            let mappings = self.state.mappings.borrow();
            let callback = usize::try_from(id)
                .ok()
                .and_then(|index| mappings.get(index))
                .ok_or_else(|| Error::runtime("key mapping has expired"))?;
            self.lua.registry_value::<Function>(callback)?
        };
        callback.call(())
    }

    pub fn run_float_key(&self, view: u64, float: u64, key: PluginKey) -> Result<(), String> {
        self.call_float_key(view, float, key).map_err(|error| {
            log_failure("float_key", "close_float", &error, None);
            lua_diagnostic(&error, None)
        })
    }

    fn call_float_key(&self, view: u64, id: u64, key: PluginKey) -> mlua::Result<()> {
        let callback = {
            let active = self.state.float.borrow();
            let active = active
                .as_ref()
                .filter(|active| active.view == view && active.id == id);
            let Some(callback) = active.and_then(|active| active.callback.as_ref()) else {
                return Ok(());
            };
            self.lua.registry_value::<Function>(callback)?
        };
        let key_table = self.lua.create_table()?;
        key_table.set("name", key.name)?;
        key_table.set("text", key.text.map(|text| text.to_string()))?;
        callback.call((
            FloatHandle {
                view,
                id,
                state: Rc::downgrade(&self.state),
            },
            key_table,
        ))
    }

    pub fn forget_float(&self, float: u64) {
        self.state.forget_float(float);
        self.lua.expire_registry_values();
    }
}

fn log_failure(stage: &str, action: &str, error: &Error, source: Option<&str>) {
    let reason = match error {
        Error::SyntaxError { .. } => "syntax_error",
        Error::RuntimeError(_) => "runtime_error",
        Error::CallbackError { .. } => "callback_error",
        Error::MemoryError(_) => "memory_error",
        _ => "lua_error",
    };
    let mut fields = serde_json::json!({"stage":stage, "reason":reason, "action":action});
    if let Some(source) = source {
        fields["source"] = source.into();
        // Only use a numeric location attributed to the known configuration chunk by Lua's parser.
        if let Error::SyntaxError { message, .. } = error
            && let Some((name, rest)) = message.split_once(':')
            && name == source
            && let Some((line, _)) = rest.split_once(':')
            && let Ok(line) = line.parse::<u32>()
        {
            fields["line"] = line.into();
        }
    }
    logging::emit(Level::Warn, "lua.failed", fields);
}

fn lua_diagnostic(error: &Error, config_name: Option<&str>) -> String {
    let message = error.to_string();
    let (summary, traceback) = message.split_once('\n').unwrap_or((&message, ""));
    let summary = summary
        .strip_prefix("runtime error: ")
        .or_else(|| summary.strip_prefix("syntax error: "))
        .unwrap_or(summary);
    let summary = if let Some((source, tail)) = summary.split_once(':')
        && Path::new(source)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lua"))
        && let Some((line, reason)) = tail.split_once(": ")
        && line.parse::<usize>().is_ok()
    {
        if config_name == Some(source) {
            format!("{reason} (line {line})")
        } else {
            format!("{reason} ({source}:{line})")
        }
    } else {
        summary.to_owned()
    };
    let mut diagnostic = match config_name {
        Some(name) => format!("{name}: {summary}"),
        None => summary,
    };
    if !traceback.is_empty() {
        diagnostic.push('\n');
        diagnostic.push_str(traceback);
    }
    diagnostic
}

fn live_state(state: &Weak<State>) -> mlua::Result<Rc<State>> {
    state
        .upgrade()
        .ok_or_else(|| Error::runtime("Lua runtime has expired"))
}

impl State {
    fn check_float(&self, view: u64, id: u64) -> mlua::Result<()> {
        if self
            .float
            .borrow()
            .as_ref()
            .is_some_and(|active| active.view == view && active.id == id)
        {
            Ok(())
        } else {
            Err(Error::runtime("float is closed"))
        }
    }

    fn forget_float(&self, id: u64) {
        let mut float = self.float.borrow_mut();
        if float.as_ref().is_some_and(|active| active.id == id) {
            *float = None;
        }
    }
}

struct ViewHandle {
    id: u64,
    state: Weak<State>,
}

impl UserData for ViewHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |lua, view, ()| {
            document_handle::for_view(lua, view)
        });
        methods.add_method("open_file", |lua, view, path: mlua::String| {
            let state = live_state(&view.state)?;
            let result = match path.to_str() {
                Ok(path) => state.host.open_file(view.id, Path::new(&*path)),
                Err(_) => Err("file path is not valid UTF-8".into()),
            };
            match result {
                Ok(FileOpen::Opened) => (true, Value::Nil).into_lua_multi(lua),
                Ok(FileOpen::Pending) => (true, Value::Nil, true).into_lua_multi(lua),
                Err(error) => (false, error).into_lua_multi(lua),
            }
        });
        methods.add_method("open_float", |lua, view, opts: Table| {
            let state = live_state(&view.state)?;
            let options = display_options(&opts)?;
            let callback = opts
                .get::<Option<Function>>("on_key")?
                .map(|callback| lua.create_registry_value(callback))
                .transpose()?;
            if state.float.borrow().is_some() {
                return Err(Error::runtime("a float is already open"));
            }
            let id = state
                .host
                .open_float(view.id, options)
                .map_err(Error::runtime)?;
            *state.float.borrow_mut() = Some(ActiveFloat {
                view: view.id,
                id,
                callback,
            });
            Ok(FloatHandle {
                view: view.id,
                id,
                state: view.state.clone(),
            })
        });
    }
}

struct FloatHandle {
    view: u64,
    id: u64,
    state: Weak<State>,
}

impl UserData for FloatHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("update", |_, float, opts: Table| {
            let state = live_state(&float.state)?;
            let options = display_options(&opts)?;
            state.check_float(float.view, float.id)?;
            state
                .host
                .update_float(float.view, float.id, options)
                .map_err(Error::runtime)
        });
        methods.add_method("close", |lua, float, ()| {
            let state = live_state(&float.state)?;
            state.check_float(float.view, float.id)?;
            state
                .host
                .close_float(float.view, float.id)
                .map_err(Error::runtime)?;
            state.forget_float(float.id);
            lua.expire_registry_values();
            Ok(())
        });
    }
}

fn display_options(options: &Table) -> mlua::Result<FloatOptions> {
    let focus_line = match options.get("focus_line")? {
        Value::Nil => None,
        Value::Boolean(false) => Some(None),
        Value::Integer(line) if line > 0 => {
            Some(Some(usize::try_from(line).map_err(Error::external)?))
        }
        _ => {
            return Err(Error::runtime(
                "focus_line must be a positive integer or false",
            ));
        }
    };
    Ok(FloatOptions {
        title: options.get("title")?,
        header: options.get("header")?,
        lines: options.get("lines")?,
        focus_line,
    })
}
