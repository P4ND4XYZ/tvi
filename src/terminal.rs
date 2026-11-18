use std::{
    io::{self, IsTerminal, Stdout},
    time::Duration,
};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyEvent},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type Screen = Terminal<CrosstermBackend<Stdout>>;

/// 明示的な復元でエラーを返し、巻き戻し時にも Drop から復元を試みる。
#[derive(Default)]
struct Restore {
    raw: bool,
    screen: bool,
    cursor: bool,
}

impl Restore {
    fn run(&mut self) -> io::Result<()> {
        let mut output = io::stdout();
        let screen = if self.screen {
            let result = execute!(output, LeaveAlternateScreen);
            self.screen = result.is_err();
            result
        } else {
            Ok(())
        };
        let cursor = if self.cursor {
            let result = execute!(output, Show);
            self.cursor = result.is_err();
            result
        } else {
            Ok(())
        };
        let raw = if self.raw {
            let result = terminal::disable_raw_mode();
            self.raw = result.is_err();
            result
        } else {
            Ok(())
        };
        screen.and(cursor).and(raw)
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        // 戻り値を返せない巻き戻し時と、明示的な復元の失敗分だけ再試行する。
        drop(self.run());
    }
}

pub fn with_screen(run: impl FnOnce(&mut Screen) -> io::Result<()>) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("interactive terminal required"));
    }

    let mut restore = Restore::default();
    let result = (|| {
        restore.raw = true;
        terminal::enable_raw_mode()?;
        let mut output = io::stdout();
        // 端末への書き込みが一部完了してからエラーになる場合も復元する。
        restore.screen = true;
        restore.cursor = true;
        execute!(output, EnterAlternateScreen)?;
        let mut screen = Terminal::new(CrosstermBackend::new(output))?;
        run(&mut screen)
    })();
    match (result, restore.run()) {
        (Err(run_error), Err(restore_error)) => Err(io::Error::other(format!(
            "{run_error}; terminal restoration failed: {restore_error}"
        ))),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "terminal restoration failed: {error}"
        ))),
        (result, Ok(())) => result,
    }
}

pub fn poll_key() -> io::Result<Option<KeyEvent>> {
    // Resize が配送されない端末でも、呼び出し元が定期的に寸法を確認できるようにする。
    if !event::poll(Duration::from_millis(100))? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(key) => Some(key),
        _ => None,
    })
}
