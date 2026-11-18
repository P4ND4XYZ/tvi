use std::{env, io, path::Path, process::ExitCode};

mod app;
mod lua;
mod markdown;
mod session;
mod terminal;
mod ui;

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
    let document = session::read_document(path)?;
    terminal::with_screen(|screen| session::run(screen, document))
}
