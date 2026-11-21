use std::rc::Weak;

use mlua::{Error, Lua, Table, Value};
use ratatui::style::{Color, Modifier, Style};

use super::{State, live_state};
use crate::colorscheme::{Colorscheme, Group};

pub(super) fn install(lua: &Lua, tvi: &Table, state: Weak<State>) -> mlua::Result<()> {
    let api = lua.create_table()?;
    api.set(
        "set",
        lua.create_function(move |_, styles: Value| {
            let scheme = parse(styles)?;
            *live_state(&state)?.colorscheme.borrow_mut() = scheme;
            Ok(())
        })?,
    )?;
    tvi.set("colorscheme", api)
}

fn parse(styles: Value) -> mlua::Result<Colorscheme> {
    let Value::Table(styles) = styles else {
        return Err(Error::runtime("styles: expected a table"));
    };
    let mut scheme = Colorscheme::default();
    for pair in styles.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let name = string(&key, "styles group key")?;
        let group = Group::from_name(&name)
            .ok_or_else(|| Error::runtime(format!("styles[{name:?}]: unknown group")))?;
        let Value::Table(attrs) = value else {
            return Err(Error::runtime(format!(
                "styles[{name:?}]: expected a table"
            )));
        };
        let mut style = Style::default();
        for pair in attrs.pairs::<Value, Value>() {
            let (key, value) = pair?;
            let attr = string(&key, &format!("{name} attribute key"))?;
            let path = format!("{name}.{attr}");
            style = match attr.as_ref() {
                "fg" => style.fg(color(&value, &path)?),
                "bg" => style.bg(color(&value, &path)?),
                "bold" => modifier(style, Modifier::BOLD, &value, &path)?,
                "italic" => modifier(style, Modifier::ITALIC, &value, &path)?,
                "underline" => modifier(style, Modifier::UNDERLINED, &value, &path)?,
                "reverse" => modifier(style, Modifier::REVERSED, &value, &path)?,
                _ => return Err(Error::runtime(format!("{path}: unknown attribute"))),
            };
        }
        scheme.set(group, style);
    }
    Ok(scheme)
}

fn string<'a>(value: &'a Value, path: &str) -> mlua::Result<mlua::BorrowedStr<'a>> {
    if let Value::String(value) = value
        && let Ok(value) = value.to_str()
    {
        return Ok(value);
    }
    Err(Error::runtime(format!("{path}: expected a UTF-8 string")))
}

fn color(value: &Value, path: &str) -> mlua::Result<Color> {
    let name = string(value, path)?;
    match name.as_ref() {
        "black" => Ok(Color::Black),
        "red" => Ok(Color::Red),
        "green" => Ok(Color::Green),
        "yellow" => Ok(Color::Yellow),
        "blue" => Ok(Color::Blue),
        "magenta" => Ok(Color::Magenta),
        "cyan" => Ok(Color::Cyan),
        "white" => Ok(Color::Gray),
        "bright_black" => Ok(Color::DarkGray),
        "bright_red" => Ok(Color::LightRed),
        "bright_green" => Ok(Color::LightGreen),
        "bright_yellow" => Ok(Color::LightYellow),
        "bright_blue" => Ok(Color::LightBlue),
        "bright_magenta" => Ok(Color::LightMagenta),
        "bright_cyan" => Ok(Color::LightCyan),
        "bright_white" => Ok(Color::White),
        "default" => Ok(Color::Reset),
        hex => {
            if let Some(hex) = hex.strip_prefix('#')
                && hex.len() == 6
                && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                && let Ok(rgb) = u32::from_str_radix(hex, 16)
            {
                let [_, red, green, blue] = rgb.to_be_bytes();
                return Ok(Color::Rgb(red, green, blue));
            }
            Err(Error::runtime(format!("{path}: invalid color {name:?}")))
        }
    }
}

fn modifier(style: Style, flag: Modifier, value: &Value, path: &str) -> mlua::Result<Style> {
    match value {
        Value::Boolean(true) => Ok(style.add_modifier(flag)),
        Value::Boolean(false) => Ok(style.remove_modifier(flag)),
        _ => Err(Error::runtime(format!("{path}: expected a boolean"))),
    }
}
