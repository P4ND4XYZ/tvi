use std::path::Path;

use mlua::{Lua, Table};

use crate::process;

pub(super) fn install(lua: &Lua, tvi: &Table) -> mlua::Result<()> {
    let api = lua.create_table()?;
    api.set(
        "run",
        lua.create_function(|lua, (program, opts): (String, Table)| {
            let args: Vec<String> = opts.get::<Option<_>>("args")?.unwrap_or_default();
            let cwd: Option<String> = opts.get("cwd")?;
            match process::run(&program, &args, cwd.as_deref().map(Path::new)) {
                Ok(output) => {
                    let result = lua.create_table()?;
                    result.set("stdout", lua.create_string(&output.stdout)?)?;
                    result.set("ok", output.status.success())?;
                    result.set("code", output.status.code())?;
                    Ok((Some(result), None))
                }
                Err(error) => {
                    let failure = lua.create_table()?;
                    failure.set("kind", error.kind)?;
                    failure.set("message", error.source.to_string())?;
                    Ok((None, Some(failure)))
                }
            }
        })?,
    )?;
    tvi.set("process", api)
}
