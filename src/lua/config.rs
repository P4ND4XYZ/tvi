use std::{
    fs,
    io::{self, ErrorKind},
    path::{Component, Path, PathBuf},
};

use mlua::{Chunk, ChunkMode, Error, Lua, MultiValue, Table, Value};

pub(super) fn init_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let directory = linux_config_dir(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::home_dir,
    );
    #[cfg(target_os = "windows")]
    let directory = std::env::var_os("LOCALAPPDATA")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let directory: Option<PathBuf> = None;

    directory.map(|directory| directory.join("tvi").join("init.lua"))
}

#[cfg(target_os = "linux")]
fn linux_config_dir(
    xdg: Option<PathBuf>,
    home: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| home().map(|path| path.join(".config")))
}

pub(super) fn read_source(path: &Path) -> io::Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    fs::read(path)
}

pub(super) fn install_searcher(lua: &Lua, init_path: &Path) -> mlua::Result<()> {
    let directory = init_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_owned();
    let package: Table = lua.globals().get("package")?;
    let searchers: Table = package.get("searchers")?;
    searchers.raw_set(
        2,
        lua.create_function(move |lua, name: String| {
            let mut module = PathBuf::new();
            for part in name.split('.') {
                let path = Path::new(part);
                if part.is_empty()
                    || !path
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
                {
                    return Err(Error::runtime(format!(
                        "invalid config module name: {name}"
                    )));
                }
                module.push(path);
            }
            let direct = directory.join(module.with_extension("lua"));
            let nested = directory.join(&module).join("init.lua");
            for path in [&direct, &nested] {
                match read_source(path) {
                    Ok(source) => {
                        let filename = path.strip_prefix(&directory).unwrap_or(path);
                        let loader = source_chunk(lua, filename, &source).into_function()?;
                        let filename = lua.create_string(path.as_os_str().as_encoded_bytes())?;
                        return Ok(MultiValue::from_vec(vec![
                            Value::Function(loader),
                            Value::String(filename),
                        ]));
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        let filename = path.strip_prefix(&directory).unwrap_or(path);
                        return Err(Error::runtime(format!("{}: {error}", filename.display())));
                    }
                }
            }
            let missing = format!(
                "no file '{}'\n\tno file '{}'",
                direct.display(),
                nested.display()
            );
            Ok(MultiValue::from_vec(vec![Value::String(
                lua.create_string(missing)?,
            )]))
        })?,
    )
}

pub(super) fn source_chunk<'source>(
    lua: &Lua,
    path: &Path,
    source: &'source [u8],
) -> Chunk<'source> {
    lua.load(source)
        .set_name(format!("@{}", path.display()))
        .set_mode(ChunkMode::Text)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_xdg_paths_override_the_home_directory() {
        let home = || Some(PathBuf::from("/home/reader"));
        for xdg in [None, Some(PathBuf::new()), Some(PathBuf::from("relative"))] {
            assert_eq!(
                linux_config_dir(xdg, home),
                Some(PathBuf::from("/home/reader/.config"))
            );
        }
        let xdg = PathBuf::from("/config");
        assert_eq!(linux_config_dir(Some(xdg.clone()), || None), Some(xdg));
    }
}
