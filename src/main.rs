use std::{env, io, path::Path, process::ExitCode};

mod app;
mod colorscheme;
mod files;
mod images;
mod logging;
mod lua;
mod markdown;
mod mermaid;
mod net;
mod pdf;
mod process;
mod session;
mod terminal;
#[cfg(all(test, windows))]
#[path = "../tests/support/pdf.rs"]
mod test_pdf;
mod ui;

fn main() -> ExitCode {
    logging::init();
    logging::emit(
        logging::Level::Debug,
        "tvi.start",
        serde_json::json!({"version": env!("CARGO_PKG_VERSION")}),
    );
    let status = match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tvi: {error}");
            ExitCode::FAILURE
        }
    };
    logging::shutdown();
    status
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let file = args
        .next()
        .filter(|_| args.next().is_none())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: tvi <file> (exactly one path required)",
            )
        })?;
    let path = Path::new(&file);
    if file.to_str().is_none() {
        session::log_document_failure(path, "startup", "path", "invalid_utf8", None);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path is not valid UTF-8",
        ));
    }
    if pdf::is_pdf(path) {
        return session::start_pdf(file.into());
    }
    let document = session::read_document(path)?;
    terminal::with_screen(|screen, input| session::run(screen, input, document))
}
