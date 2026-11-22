use std::{
    io::{self, IsTerminal, Stdout, Write},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyEvent},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::logging::{self, Level};

pub type Screen = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageMode {
    Text,
    Halfblocks,
    Kitty { cell_width: u16, cell_height: u16 },
}

impl ImageMode {
    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Halfblocks => "halfblocks",
            Self::Kitty { .. } => "kitty",
        }
    }
}

fn failure(
    operation: &'static str,
    mode: Option<ImageMode>,
    reason: &'static str,
    action: &'static str,
    os_code: Option<i32>,
) {
    let mut fields = serde_json::json!({"operation":operation, "reason":reason, "action":action});
    if let Some(mode) = mode {
        fields["image_mode"] = mode.label().into();
    }
    if let Some(code) = os_code {
        fields["os_code"] = code.into();
    }
    logging::emit(
        if action == "fallback" {
            Level::Warn
        } else {
            Level::Error
        },
        "terminal.failed",
        fields,
    );
}

pub(crate) fn check<T>(
    result: io::Result<T>,
    operation: &'static str,
    mode: Option<ImageMode>,
    action: &'static str,
) -> io::Result<T> {
    result.inspect_err(|error| {
        failure(
            operation,
            mode,
            logging::io_reason(error),
            action,
            error.raw_os_error(),
        );
    })
}

pub fn next_image_id() -> u32 {
    static IDS: std::sync::OnceLock<AtomicU32> = std::sync::OnceLock::new();
    let next = IDS.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        AtomicU32::new((nanos ^ std::process::id()).max(1))
    });
    loop {
        let id = next.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

pub fn delete_images(ids: impl IntoIterator<Item = u32>) -> io::Result<()> {
    let mut ids = ids.into_iter().peekable();
    if ids.peek().is_none() {
        return Ok(());
    }
    let mut output = io::stdout().lock();
    for id in ids {
        write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
    }
    output.flush()
}

/// 明示的な復元でエラーを返し、巻き戻し時にも Drop から復元を試みる。
#[derive(Default)]
struct Restore {
    raw: bool,
    screen: bool,
    cursor: bool,
    image_mode: Option<ImageMode>,
}

impl Restore {
    fn run(&mut self) -> io::Result<()> {
        let mut output = io::stdout();
        let screen = if self.screen {
            let result = check(
                execute!(output, LeaveAlternateScreen),
                "leave_screen",
                self.image_mode,
                "restore",
            );
            self.screen = result.is_err();
            result
        } else {
            Ok(())
        };
        let cursor = if self.cursor {
            let result = check(
                execute!(output, Show),
                "show_cursor",
                self.image_mode,
                "restore",
            );
            self.cursor = result.is_err();
            result
        } else {
            Ok(())
        };
        let raw = if self.raw {
            let result = check(
                terminal::disable_raw_mode(),
                "disable_raw_mode",
                self.image_mode,
                "restore",
            );
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

pub fn with_screen(run: impl FnOnce(&mut Screen, &mut Input) -> io::Result<()>) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        failure(
            "preflight",
            None,
            "interactive_terminal_required",
            "exit",
            None,
        );
        return Err(io::Error::other("interactive terminal required"));
    }

    let mut restore = Restore::default();
    let mut diagnostic = None;
    let result = (|| {
        restore.raw = true;
        check(terminal::enable_raw_mode(), "enable_raw_mode", None, "exit")?;
        let mut output = io::stdout();
        // 端末への書き込みが一部完了してからエラーになる場合も復元する。
        restore.screen = true;
        restore.cursor = true;
        check(
            execute!(output, EnterAlternateScreen),
            "enter_screen",
            None,
            "exit",
        )?;
        let mut screen = check(
            Terminal::new(CrosstermBackend::new(output)),
            "create_screen",
            None,
            "exit",
        )?;
        let mut input = Input::new();
        restore.image_mode = Some(input.image_mode());
        let result = run(&mut screen, &mut input);
        diagnostic = Some(input.image_mode());
        result
    })();
    let restored = restore.run();
    if std::env::var("TVI_IMAGE_DIAGNOSTICS").as_deref() == Ok("1")
        && let Some(mode) = diagnostic
    {
        eprintln!("tvi: image mode: {mode:?}");
    }
    match (result, restored) {
        (Err(run_error), Err(restore_error)) => Err(io::Error::other(format!(
            "{run_error}; terminal restoration failed: {restore_error}"
        ))),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "terminal restoration failed: {error}"
        ))),
        (result, Ok(())) => result,
    }
}

/// 色の能力が不明な端末には画像セルを送らない。端末への問い合わせや設定変更はしない。
pub fn supports_images() -> bool {
    if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
        return false;
    }
    std::env::var("COLORTERM").is_ok_and(|color| {
        color.eq_ignore_ascii_case("truecolor") || color.eq_ignore_ascii_case("24bit")
    }) || std::env::var("TERM").is_ok_and(|term| term.ends_with("-direct"))
        || (cfg!(windows) && std::env::var_os("WT_SESSION").is_some_and(|id| !id.is_empty()))
}

pub struct Input {
    fallback: ImageMode,
    #[cfg(windows)]
    console: Option<windows::ConsoleInput>,
}

impl Input {
    fn new() -> Self {
        let input = Self {
            fallback: if supports_images() {
                ImageMode::Halfblocks
            } else {
                ImageMode::Text
            },
            #[cfg(windows)]
            console: windows::ConsoleInput::probe(),
        };
        let mode = input.image_mode();
        let reason = match mode {
            ImageMode::Kitty { .. } => "kitty_probe_confirmed",
            ImageMode::Halfblocks => "truecolor_supported",
            ImageMode::Text => "image_capability_not_confirmed",
        };
        logging::emit(
            Level::Debug,
            "terminal.image_mode",
            serde_json::json!({
                "mode":mode.label(), "reason":reason,
            }),
        );
        input
    }

    pub fn image_mode(&self) -> ImageMode {
        #[cfg(windows)]
        if let Some(console) = &self.console
            && let Some(mode) = console.image_mode()
        {
            return mode;
        }
        self.fallback
    }

    pub fn refresh_cell_size(&mut self) {
        #[cfg(windows)]
        if let Some(console) = &mut self.console {
            console.refresh_cell_size();
        }
    }

    pub fn poll_key(&mut self) -> io::Result<Option<KeyEvent>> {
        self.poll_key_for(Duration::from_millis(100))
    }

    pub fn poll_key_for(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
        let mode = Some(self.image_mode());
        #[cfg(windows)]
        if let Some(console) = &mut self.console {
            return check(console.poll_key(timeout), "input", mode, "exit");
        }
        check(poll_key(timeout), "input", mode, "exit")
    }
}

fn poll_key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
    // Resize が配送されない端末でも、呼び出し元が定期的に寸法を確認できるようにする。
    if !event::poll(timeout)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(key) => Some(key),
        _ => None,
    })
}

#[cfg(windows)]
fn report_console_error(operation: &'static str, mode: Option<ImageMode>, action: &'static str) {
    drop(check(
        Err::<(), _>(io::Error::last_os_error()),
        operation,
        mode,
        action,
    ));
}

// ReadConsoleInputW の合成 VT レコードを通常の Win32 キーと混ぜずに振り分ける。
#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "Windows コンソールモードと入力レコードの操作には Win32 API が必要"
)]
mod windows {
    use std::{collections::VecDeque, thread, time::Instant};

    use crossterm::event::{KeyCode, KeyModifiers};
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::Console::{
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
            GetStdHandle, INPUT_RECORD, KEY_EVENT, PeekConsoleInputW, ReadConsoleInputW,
            STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
        },
    };

    use super::{Duration, ImageMode, IsTerminal, KeyEvent, Write, io, next_image_id};

    const CELL_QUERY: &str = "\x1b[16t";
    const PROBE_BUDGET: Duration = Duration::from_millis(160);
    const ESCAPE_WAIT: Duration = Duration::from_millis(25);

    pub(super) struct ConsoleInput {
        input: HANDLE,
        original_input_mode: u32,
        output: HANDLE,
        original_output_mode: u32,
        query_id: u32,
        startup: Option<Instant>,
        kitty_ok: bool,
        cells: Option<(u16, u16)>,
        resize_reply: Option<Instant>,
        last_resize_query: Option<Instant>,
        resize_pending: bool,
        parser: VtInput,
        keys: VecDeque<KeyEvent>,
    }

    impl ConsoleInput {
        pub(super) fn probe() -> Option<Self> {
            let var = |name| std::env::var(name).unwrap_or_default();
            if !io::stdin().is_terminal()
                || !io::stdout().is_terminal()
                || !virtual_candidate(
                    &var("TERM"),
                    &var("TERM_PROGRAM"),
                    &var("TERM_PROGRAM_VERSION"),
                    &var("WEZTERM_EXECUTABLE"),
                    &var("KITTY_WINDOW_ID"),
                    std::env::var_os("TMUX").is_some(),
                )
            {
                return None;
            }
            // SAFETY: std handles are borrowed for this foreground console session. The mode
            // pointers point to live u32 values, and mode changes are restored by Drop.
            let (input, original_input_mode, output, original_output_mode) = unsafe {
                let input = GetStdHandle(STD_INPUT_HANDLE);
                let output = GetStdHandle(STD_OUTPUT_HANDLE);
                let (mut im, mut om) = (0, 0);
                if [input, output]
                    .iter()
                    .any(|h| h.is_null() || *h == INVALID_HANDLE_VALUE)
                    || GetConsoleMode(input, &raw mut im) == 0
                    || GetConsoleMode(output, &raw mut om) == 0
                    || SetConsoleMode(input, im | ENABLE_VIRTUAL_TERMINAL_INPUT) == 0
                {
                    if !input.is_null()
                        && input != INVALID_HANDLE_VALUE
                        && !output.is_null()
                        && output != INVALID_HANDLE_VALUE
                    {
                        super::report_console_error("kitty_console_setup", None, "fallback");
                    }
                    return None;
                }
                if SetConsoleMode(output, om | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                    super::report_console_error("kitty_console_output", None, "fallback");
                    if SetConsoleMode(input, im) == 0 {
                        super::report_console_error("restore_console_input", None, "restore");
                    }
                    return None;
                }
                (input, im, output, om)
            };
            let query_id = next_image_id();
            let deadline = Instant::now() + PROBE_BUDGET;
            let mut console = Self {
                input,
                original_input_mode,
                output,
                original_output_mode,
                query_id,
                startup: Some(deadline),
                kitty_ok: false,
                cells: None,
                resize_reply: None,
                last_resize_query: None,
                resize_pending: false,
                parser: VtInput::default(),
                keys: VecDeque::new(),
            };
            // Even on partial write or timeout keep the demultiplexer alive: a response may
            // already be in flight. Only a timely, matching OK plus cell size selects Kitty.
            let sent = send(format_args!(
                "\x1b_Gi={query_id},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\{CELL_QUERY}"
            ));
            if super::check(sent, "kitty_query", None, "fallback").is_ok() {
                while Instant::now() < deadline && console.image_mode().is_none() {
                    if super::check(console.pump(deadline), "kitty_reply", None, "fallback")
                        .is_err()
                    {
                        break;
                    }
                }
            }
            console.startup = None;
            Some(console)
        }

        pub(super) fn image_mode(&self) -> Option<ImageMode> {
            let (cell_width, cell_height) = self.cells?;
            self.kitty_ok.then_some(ImageMode::Kitty {
                cell_width,
                cell_height,
            })
        }

        pub(super) fn refresh_cell_size(&mut self) {
            self.resize_pending = self.image_mode().is_some();
        }

        fn send_resize_if_due(&mut self, now: Instant) {
            if self.resize_pending
                && self
                    .last_resize_query
                    .is_none_or(|previous| now - previous >= PROBE_BUDGET)
            {
                self.resize_pending = false;
                self.last_resize_query = Some(now);
                if super::check(
                    send(format_args!("{CELL_QUERY}")),
                    "query_cell_size",
                    self.image_mode(),
                    "fallback",
                )
                .is_ok()
                {
                    self.resize_reply = Some(now + PROBE_BUDGET);
                }
            }
        }

        pub(super) fn poll_key(&mut self, timeout: Duration) -> io::Result<Option<KeyEvent>> {
            let end = Instant::now() + timeout;
            loop {
                if let Some(key) = self.keys.pop_front() {
                    return Ok(Some(key));
                }
                if Instant::now() >= end {
                    return Ok(None);
                }
                let mode = self.image_mode();
                self.pump(end)?;
                if self.image_mode() != mode {
                    return Ok(None);
                }
            }
        }

        fn pump(&mut self, deadline: Instant) -> io::Result<()> {
            let now = Instant::now();
            self.send_resize_if_due(now);
            // Peek, then consume only one record. crossterm's zero-time poll handles the
            // non-VT record at the head (including native keyboard layout and modifiers).
            let mut record = INPUT_RECORD::default();
            let mut count = 0;
            // SAFETY: valid borrowed console handle; one initialized record and count.
            if unsafe { PeekConsoleInputW(self.input, &raw mut record, 1, &raw mut count) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                // ConPTY queues a raw string as adjacent records. Do not treat time
                // spent processing already queued records as a gap in that string.
                self.parser.expire(now, &mut self.keys);
                if !self.keys.is_empty() {
                    return Ok(());
                }
                thread::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(4)),
                );
                return Ok(());
            }
            if let Some(unit) = vt_unit(&record) {
                // SAFETY: sole reader in the foreground app; peek confirmed one record is
                // available. The buffers have capacity for exactly the requested record.
                if unsafe { ReadConsoleInputW(self.input, &raw mut record, 1, &raw mut count) } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if let Some(reply) = self.parser.push(unit, now, &mut self.keys) {
                    self.accept_reply(reply, now);
                }
            } else if let Some(key) = super::poll_key(Duration::ZERO)? {
                self.parser.flush_escape(&mut self.keys);
                self.keys.push_back(key);
            }
            Ok(())
        }

        fn accept_reply(&mut self, reply: Reply, now: Instant) {
            match reply {
                Reply::Kitty(id)
                    if id == self.query_id && self.startup.is_some_and(|end| now < end) =>
                {
                    self.kitty_ok = true;
                }
                Reply::Cell(w, h)
                    if self.startup.is_some_and(|end| now < end)
                        || self.resize_reply.take().is_some_and(|end| now < end) =>
                {
                    self.cells = Some((w, h));
                }
                _ => {}
            }
        }
    }

    impl Drop for ConsoleInput {
        fn drop(&mut self) {
            // SAFETY: handles are borrowed and valid for the duration of the screen session.
            // Restore even if probing or rendering did not succeed.
            unsafe {
                if SetConsoleMode(self.input, self.original_input_mode) == 0 {
                    super::report_console_error(
                        "restore_console_input",
                        self.image_mode(),
                        "restore",
                    );
                }
                if SetConsoleMode(self.output, self.original_output_mode) == 0 {
                    super::report_console_error(
                        "restore_console_output",
                        self.image_mode(),
                        "restore",
                    );
                }
            }
        }
    }

    fn send(query: std::fmt::Arguments<'_>) -> io::Result<()> {
        let mut output = io::stdout().lock();
        output.write_fmt(query).and_then(|()| output.flush())
    }

    fn virtual_candidate(
        term: &str,
        program: &str,
        version: &str,
        wezterm: &str,
        kitty: &str,
        tmux: bool,
    ) -> bool {
        if tmux || term == "dumb" || term.starts_with("screen") || term.starts_with("tmux") {
            return false;
        }
        let herdr = program == "herdr" && !wezterm.is_empty() && {
            let mut parts = version.split('.').map(str::parse::<u16>);
            matches!((parts.next(), parts.next(), parts.next(), parts.next()),
                (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None)
                if (major, minor, patch) >= (0, 9, 5))
        };
        herdr
            || (program != "herdr"
                && term == "xterm-kitty"
                && kitty.parse::<u32>().is_ok_and(|id| id > 0))
    }

    fn vt_unit(record: &INPUT_RECORD) -> Option<u16> {
        if record.EventType != KEY_EVENT as u16 {
            return None;
        }
        // SAFETY: EventType discriminates the union member, and UnicodeChar is the W result.
        let key = unsafe { record.Event.KeyEvent };
        (key.bKeyDown != 0
            && key.wVirtualKeyCode == 0
            && key.wVirtualScanCode == 0
            && key.dwControlKeyState == 0
            && key.wRepeatCount == 1)
            .then_some(unsafe { key.uChar.UnicodeChar })
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Reply {
        Kitty(u32),
        Cell(u16, u16),
    }

    #[derive(Default)]
    enum Sequence {
        #[default]
        Ground,
        Escape(Instant),
        Csi {
            body: String,
            ss3: bool,
            since: Instant,
        },
        ApcStart(Instant),
        Apc {
            body: String,
            st: bool,
            overflow: bool,
        },
    }

    #[derive(Default)]
    struct VtInput {
        sequence: Sequence,
        surrogate: Option<u16>,
    }

    impl VtInput {
        fn expire(&mut self, now: Instant, keys: &mut VecDeque<KeyEvent>) {
            match &self.sequence {
                Sequence::Escape(at) | Sequence::ApcStart(at) if now - *at >= ESCAPE_WAIT => {
                    self.flush_escape(keys);
                }
                // The query deadline controls acceptance, not framing. Once `ESC _ G`
                // or `CSI 6;` arrives, keep consuming the frame even past that deadline.
                // `CSI 6;` can also begin a modified PageDown; the final byte decides.
                Sequence::Csi { body, ss3, since }
                    if (*ss3 || !body.starts_with("6;")) && now - *since >= PROBE_BUDGET =>
                {
                    self.sequence = Sequence::Ground;
                }
                _ => {}
            }
        }

        fn flush_escape(&mut self, keys: &mut VecDeque<KeyEvent>) {
            match self.sequence {
                Sequence::Escape(_) => push_char(keys, '\x1b', KeyModifiers::NONE),
                Sequence::ApcStart(_) => push_char(keys, '_', KeyModifiers::ALT),
                _ => return,
            }
            self.sequence = Sequence::Ground;
        }

        fn push(
            &mut self,
            unit: u16,
            now: Instant,
            keys: &mut VecDeque<KeyEvent>,
        ) -> Option<Reply> {
            if (0xd800..=0xdbff).contains(&unit) {
                self.surrogate = Some(unit);
                return None;
            }
            let lead = self.surrogate.take();
            let ch = if let Some(high) = lead
                && (0xdc00..=0xdfff).contains(&unit)
            {
                char::decode_utf16([high, unit]).next()?.ok()?
            } else {
                char::from_u32(u32::from(unit))?
            };
            match &mut self.sequence {
                Sequence::ApcStart(_) => {
                    if ch == 'G' {
                        self.sequence = Sequence::Apc {
                            body: String::from('G'),
                            st: false,
                            overflow: false,
                        };
                    } else {
                        self.flush_escape(keys);
                        return self.push(unit, now, keys);
                    }
                }
                Sequence::Apc { body, st, overflow } => {
                    if *st && ch == '\\' {
                        let response = (!*overflow).then(|| kitty_reply(body)).flatten();
                        self.sequence = Sequence::Ground;
                        return response;
                    }
                    *st = ch == '\x1b';
                    if body.len() < 512 && !*st {
                        body.push(ch);
                    } else if !*st {
                        *overflow = true;
                    }
                }
                Sequence::Csi { body, ss3, .. } => {
                    if ('@'..='~').contains(&ch) {
                        let reply = (!*ss3 && ch == 't').then(|| cell_reply(body)).flatten();
                        if let Some(key) = vt_special(body, ch) {
                            keys.push_back(key);
                        }
                        self.sequence = Sequence::Ground;
                        return reply;
                    }
                    if ch == '\x1b' {
                        self.sequence = Sequence::Escape(now);
                    } else if body.len() < 64 && (' '..='?').contains(&ch) {
                        body.push(ch);
                    } else if !(' '..='?').contains(&ch) {
                        self.sequence = Sequence::Ground;
                        push_char(keys, ch, KeyModifiers::NONE);
                    }
                }
                Sequence::Escape(_) => {
                    self.sequence = match ch {
                        '[' | 'O' => Sequence::Csi {
                            body: String::new(),
                            ss3: ch == 'O',
                            since: now,
                        },
                        '_' => Sequence::ApcStart(now),
                        '\x1b' => {
                            push_char(keys, ch, KeyModifiers::NONE);
                            Sequence::Escape(now)
                        }
                        _ => {
                            push_char(keys, ch, KeyModifiers::ALT);
                            Sequence::Ground
                        }
                    };
                }
                Sequence::Ground => {
                    if ch == '\x1b' {
                        self.sequence = Sequence::Escape(now);
                    } else {
                        push_char(keys, ch, KeyModifiers::NONE);
                    }
                }
            }
            None
        }
    }

    fn kitty_reply(body: &str) -> Option<Reply> {
        let (header, message) = body.strip_prefix('G')?.split_once(';')?;
        if message != "OK" {
            return None;
        }
        let mut ids = header
            .split(',')
            .filter_map(|field| field.strip_prefix("i="));
        let id = ids.next()?.parse().ok()?;
        ids.next().is_none().then_some(Reply::Kitty(id))
    }

    fn cell_reply(body: &str) -> Option<Reply> {
        let (height, width) = body.strip_prefix("6;")?.split_once(';')?;
        if ![height, width]
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let (h, w) = (height.parse().ok()?, width.parse().ok()?);
        ((1..=128).contains(&h) && (1..=96).contains(&w)).then_some(Reply::Cell(w, h))
    }

    fn push_char(keys: &mut VecDeque<KeyEvent>, ch: char, mut modifiers: KeyModifiers) {
        let code = match ch {
            '\x1b' => KeyCode::Esc,
            '\x08' | '\x7f' => KeyCode::Backspace,
            '\r' | '\n' => KeyCode::Enter,
            '\t' => KeyCode::Tab,
            '\x01'..='\x1a' => {
                modifiers |= KeyModifiers::CONTROL;
                KeyCode::Char(char::from(b'a' + ch as u8 - 1))
            }
            ch => {
                if ch.is_uppercase() {
                    modifiers |= KeyModifiers::SHIFT;
                }
                KeyCode::Char(ch)
            }
        };
        keys.push_back(KeyEvent::new(code, modifiers));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn feed(
            input: &mut VtInput,
            keys: &mut VecDeque<KeyEvent>,
            at: Instant,
            bytes: &str,
        ) -> Vec<Reply> {
            bytes
                .encode_utf16()
                .filter_map(|unit| input.push(unit, at, keys))
                .collect()
        }

        #[test]
        fn terminal_replies_are_separate_from_pending_keys_even_across_reads() {
            let (mut input, mut keys) = (VtInput::default(), VecDeque::new());
            let now = Instant::now();
            assert!(feed(&mut input, &mut keys, now, "j\x1b[A\x1b_Gi=31;O").is_empty());
            assert_eq!(
                feed(
                    &mut input,
                    &mut keys,
                    now,
                    "K\x1b\\\x1b[6;16;8t\x1b[6;0;8t\x1b_Gi=31;EINVAL\x1b\\k"
                ),
                [Reply::Kitty(31), Reply::Cell(8, 16)]
            );
            assert_eq!(
                keys.into_iter().map(|key| key.code).collect::<Vec<_>>(),
                [KeyCode::Char('j'), KeyCode::Up, KeyCode::Char('k')]
            );
        }

        #[test]
        fn split_late_replies_are_framed_without_eating_following_keys() {
            let now = Instant::now();
            let late = now + PROBE_BUDGET + Duration::from_millis(10);
            for (prefix, suffix, expected) in [
                ("\x1b_Gi=31;OK\x1b", "\\j", Reply::Kitty(31)),
                ("\x1b[6;", "16;8tj", Reply::Cell(8, 16)),
            ] {
                let (mut input, mut keys) = (VtInput::default(), VecDeque::new());
                assert!(feed(&mut input, &mut keys, now, prefix).is_empty());
                input.expire(late, &mut keys);
                assert!(keys.is_empty());
                assert_eq!(feed(&mut input, &mut keys, late, suffix), [expected]);
                assert_eq!(
                    keys,
                    [KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)]
                );
            }
        }

        #[test]
        fn lone_escape_does_not_wait_for_probe_deadline() {
            let (mut input, mut keys) = (VtInput::default(), VecDeque::new());
            let now = Instant::now();
            assert!(feed(&mut input, &mut keys, now, "\x1b").is_empty());
            input.expire(now + ESCAPE_WAIT, &mut keys);
            assert_eq!(keys, [KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)]);
        }

        #[test]
        fn chooses_kitty_only_after_both_matching_timely_replies() {
            let now = Instant::now();
            let end = now + PROBE_BUDGET;
            let mut console = std::mem::ManuallyDrop::new(ConsoleInput {
                input: std::ptr::null_mut(),
                original_input_mode: 0,
                output: std::ptr::null_mut(),
                original_output_mode: 0,
                query_id: 31,
                startup: Some(end),
                kitty_ok: false,
                cells: None,
                resize_reply: None,
                last_resize_query: None,
                resize_pending: false,
                parser: VtInput::default(),
                keys: VecDeque::new(),
            });
            console.accept_reply(Reply::Cell(10, 24), end);
            assert!(console.cells.is_none());
            console.accept_reply(Reply::Cell(8, 16), now);
            console.accept_reply(Reply::Kitty(32), now);
            console.accept_reply(Reply::Kitty(31), end);
            assert!(console.image_mode().is_none());
            console.accept_reply(Reply::Kitty(31), now);
            assert_eq!(
                console.image_mode(),
                Some(ImageMode::Kitty {
                    cell_width: 8,
                    cell_height: 16
                })
            );
            console.startup = None;
            console.accept_reply(Reply::Cell(12, 24), end);
            assert_eq!(console.cells, Some((8, 16)));
            console.resize_reply = Some(end + PROBE_BUDGET);
            console.accept_reply(Reply::Cell(12, 24), end);
            assert_eq!(console.cells, Some((12, 24)));
        }

        #[test]
        fn ordinary_wezterm_is_not_treated_as_the_known_virtual_placement_route() {
            assert!(virtual_candidate(
                "xterm-256color",
                "herdr",
                "0.9.5",
                "wezterm-gui.exe",
                "",
                false
            ));
            assert!(virtual_candidate("xterm-kitty", "", "", "", "1", false));
            for (program, version, tmux) in [
                ("WezTerm", "", false),
                ("herdr", "0.9.4", false),
                ("herdr", "0.9.5", true),
            ] {
                assert!(!virtual_candidate(
                    "xterm-256color",
                    program,
                    version,
                    "wezterm-gui.exe",
                    "",
                    tmux
                ));
            }
        }
    }

    fn vt_special(body: &str, final_char: char) -> Option<KeyEvent> {
        let (parameter, modifiers) = if let Some((first, mask)) = body.split_once(';') {
            (first, mask.parse::<u8>().ok()?.checked_sub(1)?)
        } else {
            (body, 0)
        };
        let mut flags = KeyModifiers::NONE;
        if modifiers & 1 != 0 {
            flags |= KeyModifiers::SHIFT;
        }
        if modifiers & 2 != 0 {
            flags |= KeyModifiers::ALT;
        }
        if modifiers & 4 != 0 {
            flags |= KeyModifiers::CONTROL;
        }
        let code = match (parameter, final_char) {
            ("" | "1", 'A') => KeyCode::Up,
            ("" | "1", 'B') => KeyCode::Down,
            ("" | "1", 'C') => KeyCode::Right,
            ("" | "1", 'D') => KeyCode::Left,
            ("" | "1", 'H') | ("1" | "7", '~') => KeyCode::Home,
            ("" | "1", 'F') | ("4" | "8", '~') => KeyCode::End,
            ("2", '~') => KeyCode::Insert,
            ("3", '~') => KeyCode::Delete,
            ("5", '~') => KeyCode::PageUp,
            ("6", '~') => KeyCode::PageDown,
            ("" | "1", 'P') | ("11", '~') => KeyCode::F(1),
            ("" | "1", 'Q') | ("12", '~') => KeyCode::F(2),
            ("" | "1", 'R') | ("13", '~') => KeyCode::F(3),
            ("" | "1", 'S') | ("14", '~') => KeyCode::F(4),
            ("15", '~') => KeyCode::F(5),
            ("17", '~') => KeyCode::F(6),
            ("18", '~') => KeyCode::F(7),
            ("19", '~') => KeyCode::F(8),
            ("20", '~') => KeyCode::F(9),
            ("21", '~') => KeyCode::F(10),
            ("23", '~') => KeyCode::F(11),
            ("24", '~') => KeyCode::F(12),
            ("", 'Z') => {
                flags |= KeyModifiers::SHIFT;
                KeyCode::BackTab
            }
            _ => return None,
        };
        Some(KeyEvent::new(code, flags))
    }
}
