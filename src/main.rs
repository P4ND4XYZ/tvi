use std::{env, fs, io, path::Path, process::ExitCode};

mod app;
mod markdown;
mod terminal;
mod ui;

use app::App;
use markdown::tui_markdown::TuiMarkdown;
use ui::{input::Input, layout::Layout};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tvi: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let file = args
        .next()
        .filter(|_| args.next().is_none())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: tvi <markdown-file> (exactly one path required)",
            )
        })?;
    let path = Path::new(file.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "file path is not valid UTF-8")
    })?);
    let label = path.to_string_lossy();
    let label = label.escape_debug();
    let bytes = fs::read(path)
        .map_err(|error| io::Error::new(error.kind(), format!("cannot read {label}: {error}")))?;
    let markdown = String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} is not valid UTF-8: {error}"),
        )
    })?;
    let name = path
        .file_name()
        .unwrap_or(&file)
        .to_string_lossy()
        .replace(char::is_control, "�");
    let mut app = App::new(name, markdown);
    terminal::with_screen(|screen| {
        let document = ui::prepare(&TuiMarkdown, app.markdown(), screen.size()?.width);
        let mut width = document.width();
        let mut layout = Layout::new(&document);
        let mut anchors = ui::anchors(&layout);
        let mut input = Input::default();
        loop {
            let mut drawn_size = (0, 0);
            screen.draw(|frame| {
                drawn_size = (frame.area().width, frame.area().height);
                if width != frame.area().width {
                    width = frame.area().width;
                    layout = Layout::at_width(&document, width);
                    anchors = ui::anchors(&layout);
                }
                app.reflow(
                    &anchors,
                    usize::from(ui::body_area(frame.area(), app.mode()).height),
                );
                ui::draw(frame, &app, &layout);
            })?;
            loop {
                if let Some(key) = terminal::poll_key()?
                    && let Some(action) = input.action(key, app.mode(), app.help_open())
                {
                    app.apply(action, &anchors, document.search_text());
                    if app.should_quit() {
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
    })
}
