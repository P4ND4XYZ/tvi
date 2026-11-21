use std::{
    collections::VecDeque,
    fmt::Write,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{Action, Direction, Mode, Motion};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeymapMode {
    Normal,
    Command,
    Search,
}

impl From<Mode> for KeymapMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Normal => Self::Normal,
            Mode::Command => Self::Command,
            Mode::Search(_) => Self::Search,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    Builtin(Action),
    Lua(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Dispatch {
    pub action: InputAction,
    pub replay: Vec<KeyEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Chord {
    const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    const fn control(ch: char) -> Self {
        Self {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::CONTROL,
        }
    }

    fn new(code: KeyCode, mut modifiers: KeyModifiers) -> Self {
        // Printable keys already carry the character produced by Shift.
        if matches!(code, KeyCode::Char(_)) && modifiers == KeyModifiers::SHIFT {
            return Self::plain(code);
        }
        // Crossterm may report Shift-Tab as BackTab with or without Shift.
        if code == KeyCode::BackTab
            || code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT)
        {
            modifiers.remove(KeyModifiers::SHIFT);
            return Self {
                code: KeyCode::BackTab,
                modifiers,
            };
        }
        Self { code, modifiers }
    }

    fn unmodified(self) -> bool {
        self.modifiers.difference(KeyModifiers::SHIFT).is_empty()
    }

    fn matches_builtin(self, builtin: Self) -> bool {
        self.code == builtin.code
            && (self.modifiers == builtin.modifiers
                || builtin.modifiers.is_empty() && self.unmodified())
    }
}

impl From<KeyEvent> for Chord {
    fn from(key: KeyEvent) -> Self {
        Self::new(key.code, key.modifiers)
    }
}

use KeyCode::{Char, Down, F, PageDown, PageUp, Up};

const NORMAL_BINDINGS: &[(&[Chord], Action)] = &[
    (&[Chord::plain(Char('j'))], Action::Move(Motion::LineDown)),
    (&[Chord::plain(Down)], Action::Move(Motion::LineDown)),
    (&[Chord::plain(Char('k'))], Action::Move(Motion::LineUp)),
    (&[Chord::plain(Up)], Action::Move(Motion::LineUp)),
    (&[Chord::plain(PageDown)], Action::Move(Motion::PageDown)),
    (&[Chord::plain(PageUp)], Action::Move(Motion::PageUp)),
    (&[Chord::control('d')], Action::Move(Motion::HalfDown)),
    (&[Chord::control('u')], Action::Move(Motion::HalfUp)),
    (&[Chord::control('f')], Action::Move(Motion::PageDown)),
    (&[Chord::control('b')], Action::Move(Motion::PageUp)),
    (&[Chord::control('o')], Action::JumpBack),
    (&[Chord::plain(Char('G'))], Action::Move(Motion::End)),
    (
        &[Chord::plain(Char('g')), Chord::plain(Char('g'))],
        Action::Move(Motion::Start),
    ),
    (&[Chord::plain(Char(':'))], Action::StartCommand),
    (
        &[Chord::plain(Char('/'))],
        Action::StartSearch(Direction::Forward),
    ),
    (
        &[Chord::plain(Char('?'))],
        Action::StartSearch(Direction::Backward),
    ),
    (
        &[Chord::plain(Char('n'))],
        Action::RepeatSearch { reverse: false },
    ),
    (
        &[Chord::plain(Char('N'))],
        Action::RepeatSearch { reverse: true },
    ),
    (&[Chord::plain(F(1))], Action::Help),
];

struct Binding {
    mode: KeymapMode,
    keys: Vec<Chord>,
    id: u64,
    _description: Option<String>,
    is_default: bool,
}

#[derive(Default)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

#[derive(Default)]
struct KeyMatch {
    action: Option<InputAction>,
    prefix: bool,
}

impl KeyMatch {
    fn include(&mut self, length: usize, matched: usize, action: InputAction) {
        if length == matched {
            self.action = Some(action);
        } else {
            self.prefix = true;
        }
    }
}

impl Keymap {
    /// Registers a sequence of literal characters and Vim-style key tokens.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid notation, reserved prompt keys, or a
    /// sequence already mapped in the same mode.
    pub fn register(
        &mut self,
        mode: KeymapMode,
        keys: &str,
        id: u64,
        desc: Option<String>,
    ) -> Result<(), String> {
        self.register_binding(mode, keys, id, desc, false)
    }

    pub fn register_default(
        &mut self,
        mode: KeymapMode,
        keys: &str,
        id: u64,
    ) -> Result<(), String> {
        self.register_binding(mode, keys, id, None, true)
    }

    fn register_binding(
        &mut self,
        mode: KeymapMode,
        keys: &str,
        id: u64,
        desc: Option<String>,
        is_default: bool,
    ) -> Result<(), String> {
        let sequence = parse_sequence(keys)?;
        if mode != KeymapMode::Normal && sequence.iter().any(|&key| prompt_key(key).is_some()) {
            return Err(format!(
                "key sequence {keys:?} is reserved for prompt input"
            ));
        }
        if mode == KeymapMode::Normal
            && NORMAL_BINDINGS.iter().any(|(builtin, _)| {
                sequence.len() == builtin.len()
                    && sequence
                        .iter()
                        .zip(*builtin)
                        .all(|(&key, &builtin_key)| key.matches_builtin(builtin_key))
            })
        {
            return Err(format!(
                "key sequence {keys:?} conflicts with a built-in mapping"
            ));
        }
        if self.bindings.iter().any(|binding| {
            binding.mode == mode && binding.keys == sequence && (is_default || !binding.is_default)
        }) {
            return Err(format!(
                "key sequence {keys:?} conflicts with a registered mapping"
            ));
        }
        self.bindings.push(Binding {
            mode,
            keys: sequence,
            id,
            _description: desc,
            is_default,
        });
        Ok(())
    }

    fn find(&self, mode: KeymapMode, keys: &[KeyEvent]) -> KeyMatch {
        let mut matched = KeyMatch::default();
        if mode == KeymapMode::Normal {
            for &(sequence, action) in NORMAL_BINDINGS {
                if keys.len() <= sequence.len()
                    && keys
                        .iter()
                        .zip(sequence)
                        .all(|(&key, &builtin)| Chord::from(key).matches_builtin(builtin))
                {
                    matched.include(sequence.len(), keys.len(), InputAction::Builtin(action));
                }
            }
        } else if let [key] = keys {
            matched.action = prompt_key(Chord::from(*key)).map(InputAction::Builtin);
        }
        for binding in &self.bindings {
            if binding.mode == mode
                && keys.len() <= binding.keys.len()
                && keys
                    .iter()
                    .zip(&binding.keys)
                    .all(|(&key, &chord)| Chord::from(key) == chord)
            {
                matched.include(binding.keys.len(), keys.len(), InputAction::Lua(binding.id));
            }
        }
        matched
    }
}

const SEQUENCE_TIMEOUT: Duration = Duration::from_millis(300);

struct Fallback {
    dispatch: InputAction,
    matched: usize,
    deadline: Instant,
}

#[derive(Default)]
pub struct Input {
    pending: Vec<KeyEvent>,
    fallback: Option<Fallback>,
    mode: Option<Mode>,
}

impl Input {
    #[cfg(test)]
    pub fn action(&mut self, key: KeyEvent, mode: Mode, help_open: bool) -> Option<Action> {
        match self
            .dispatch(key, mode, help_open, &Keymap::default())?
            .action
        {
            InputAction::Builtin(action) => Some(action),
            InputAction::Lua(_) => None,
        }
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.fallback = None;
        self.mode = None;
    }

    pub fn expire(&mut self) -> Option<Dispatch> {
        self.expire_at(Instant::now())
    }

    fn expire_at(&mut self, now: Instant) -> Option<Dispatch> {
        if self
            .fallback
            .as_ref()
            .is_some_and(|fallback| now >= fallback.deadline)
        {
            return self.dispatch_fallback();
        }
        None
    }

    fn dispatch_fallback(&mut self) -> Option<Dispatch> {
        let fallback = self.fallback.take()?;
        let replay = self.pending.drain(fallback.matched..).collect();
        self.pending.clear();
        Some(Dispatch {
            action: fallback.dispatch,
            replay,
        })
    }

    pub fn dispatch(
        &mut self,
        key: KeyEvent,
        mode: Mode,
        help_open: bool,
        keymap: &Keymap,
    ) -> Option<Dispatch> {
        self.dispatch_at(key, mode, help_open, keymap, Instant::now())
    }

    fn dispatch_at(
        &mut self,
        key: KeyEvent,
        mode: Mode,
        help_open: bool,
        keymap: &Keymap,
        now: Instant,
    ) -> Option<Dispatch> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if help_open {
            self.reset();
            return (Chord::from(key).unmodified() && key.code == KeyCode::Esc).then_some(
                Dispatch {
                    action: InputAction::Builtin(Action::Cancel),
                    replay: Vec::new(),
                },
            );
        }
        if self.mode != Some(mode) {
            self.reset();
            self.mode = Some(mode);
        }
        let mode = KeymapMode::from(mode);
        if let Some(mut dispatch) = self.expire_at(now) {
            dispatch.replay.push(key);
            return Some(dispatch);
        }
        let mut first = Some(key);
        let mut queued = VecDeque::new();
        while let Some(key) = first.take().or_else(|| queued.pop_front()) {
            let repeat = key.kind == KeyEventKind::Repeat;
            // Holding a prefix key cannot start or complete a sequence.
            if repeat && self.pending.last().copied().map(Chord::from) == Some(Chord::from(key)) {
                continue;
            }
            self.pending.push(key);
            let matched = if repeat && self.pending.len() > 1 {
                KeyMatch::default()
            } else {
                keymap.find(mode, &self.pending)
            };
            if matched.prefix && !repeat {
                if let Some(dispatch) = matched.action {
                    self.fallback = Some(Fallback {
                        dispatch,
                        matched: self.pending.len(),
                        deadline: now + SEQUENCE_TIMEOUT,
                    });
                } else if let Some(fallback) = &mut self.fallback {
                    fallback.deadline = now + SEQUENCE_TIMEOUT;
                }
                continue;
            }
            if let Some(action) = matched.action {
                self.pending.clear();
                self.fallback = None;
                return Some(Dispatch {
                    action,
                    replay: queued.into(),
                });
            }
            if let Some(mut dispatch) = self.dispatch_fallback() {
                dispatch.replay.extend(queued);
                return Some(dispatch);
            }
            // Discard only the first unmatched key. The suffix can contain
            // complete mappings or start a new sequence.
            for key in self.pending.drain(1..).rev() {
                queued.push_front(key);
            }
            self.pending.clear();
        }
        None
    }
}

fn prompt_key(key: Chord) -> Option<Action> {
    if !key.unmodified() {
        return None;
    }
    match key.code {
        Char(ch) if !ch.is_control() => Some(Action::Insert(ch)),
        KeyCode::Backspace => Some(Action::Backspace),
        KeyCode::Enter => Some(Action::Submit),
        KeyCode::Esc => Some(Action::Cancel),
        _ => None,
    }
}

const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("Backspace", KeyCode::Backspace),
    ("Enter", KeyCode::Enter),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Tab", KeyCode::Tab),
    ("BackTab", KeyCode::BackTab),
    ("Delete", KeyCode::Delete),
    ("Insert", KeyCode::Insert),
    ("Esc", KeyCode::Esc),
    ("Space", Char(' ')),
    ("lt", Char('<')),
];

// A sequence contains literal Unicode characters and optional Vim-style tokens
// such as <C-x>, <F2> or <Up>. Use <lt> for a literal opening angle bracket.
fn parse_sequence(keys: &str) -> Result<Vec<Chord>, String> {
    let mut remaining = keys.chars();
    let mut sequence = Vec::new();
    while let Some(ch) = remaining.next() {
        if ch == '<' {
            let rest = remaining.as_str();
            let (token, rest) = rest
                .split_once('>')
                .ok_or_else(|| format!("unclosed key token in {keys:?}"))?;
            sequence
                .push(parse_token(token).ok_or_else(|| format!("invalid key token <{token}>"))?);
            remaining = rest.chars();
        } else if ch.is_control() {
            return Err(format!(
                "use key notation for control characters in {keys:?}"
            ));
        } else {
            sequence.push(Chord::plain(Char(ch)));
        }
    }
    if sequence.is_empty() {
        return Err("key sequence must not be empty".to_owned());
    }
    Ok(sequence)
}

fn parse_token(mut token: &str) -> Option<Chord> {
    let mut modifiers = KeyModifiers::NONE;
    while let Some((name, rest)) = token.split_once('-') {
        let modifier = match name.to_ascii_lowercase().as_str() {
            "c" => KeyModifiers::CONTROL,
            "a" => KeyModifiers::ALT,
            "s" => KeyModifiers::SHIFT,
            "d" => KeyModifiers::SUPER,
            "m" => KeyModifiers::META,
            "h" => KeyModifiers::HYPER,
            "" => break,
            _ => return None,
        };
        if modifiers.contains(modifier) {
            return None;
        }
        modifiers.insert(modifier);
        token = rest;
    }
    let mut chars = token.chars();
    let first = chars.next()?;
    let code = if chars.as_str().is_empty() && !first.is_control() {
        let ch = if modifiers == KeyModifiers::SHIFT {
            first.to_ascii_uppercase()
        } else {
            first
        };
        Char(ch)
    } else if let Some((_, code)) = NAMED_KEYS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(token))
    {
        *code
    } else if matches!(first, 'f' | 'F') {
        let number = chars.as_str().parse::<u8>().ok()?;
        if number == 0 {
            return None;
        }
        F(number)
    } else {
        return None;
    };
    Some(Chord::new(code, modifiers))
}

pub struct PluginKey {
    pub name: String,
    pub text: Option<char>,
}

#[must_use]
pub fn is_escape(key: KeyEvent) -> bool {
    key.kind != KeyEventKind::Release && key.code == KeyCode::Esc
}

#[must_use]
pub fn plugin_key(key: KeyEvent) -> Option<PluginKey> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let chord = Chord::new(key.code, key.modifiers);
    let text = match chord.code {
        Char(ch) if chord.unmodified() && !ch.is_control() => Some(ch),
        _ => None,
    };
    let mut name = String::new();
    for (modifier, prefix) in [
        (KeyModifiers::CONTROL, "C-"),
        (KeyModifiers::ALT, "A-"),
        (KeyModifiers::SHIFT, "S-"),
        (KeyModifiers::SUPER, "D-"),
        (KeyModifiers::META, "M-"),
        (KeyModifiers::HYPER, "H-"),
    ] {
        if chord.modifiers.contains(modifier) {
            name.push_str(prefix);
        }
    }
    match chord.code {
        Char(ch) => name.push(ch),
        code => {
            if let Some((key_name, _)) = NAMED_KEYS.iter().find(|(_, key)| *key == code) {
                name.push_str(key_name);
            } else {
                write!(name, "{code}").ok()?;
            }
        }
    }
    Some(PluginKey { name, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_g_requires_consecutive_presses() {
        let mut input = Input::default();
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        assert_eq!(input.action(key('g'), Mode::Normal, false), None);
        assert_eq!(
            input.action(key('j'), Mode::Normal, false),
            Some(Action::Move(Motion::LineDown))
        );
        assert_eq!(input.action(key('g'), Mode::Normal, false), None);
        assert_eq!(
            input.action(key('g'), Mode::Normal, false),
            Some(Action::Move(Motion::Start))
        );
    }

    #[test]
    fn normal_motion_keys_map_to_display_row_movements() {
        use KeyCode::{Char, Down, PageDown, PageUp, Up};
        use KeyModifiers as Mods;

        let mut input = Input::default();
        for (code, modifier, motion) in [
            (Char('j'), Mods::NONE, Motion::LineDown),
            (Down, Mods::NONE, Motion::LineDown),
            (Char('k'), Mods::NONE, Motion::LineUp),
            (Up, Mods::NONE, Motion::LineUp),
            (Char('d'), Mods::CONTROL, Motion::HalfDown),
            (Char('u'), Mods::CONTROL, Motion::HalfUp),
            (Char('f'), Mods::CONTROL, Motion::PageDown),
            (Char('b'), Mods::CONTROL, Motion::PageUp),
            (PageDown, Mods::NONE, Motion::PageDown),
            (PageUp, Mods::NONE, Motion::PageUp),
            (Char('G'), Mods::SHIFT, Motion::End),
        ] {
            assert_eq!(
                input.action(KeyEvent::new(code, modifier), Mode::Normal, false),
                Some(Action::Move(motion)),
            );
        }
    }

    #[test]
    fn q_is_only_prompt_text_and_control_c_is_unbound() {
        let mut input = Input::default();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        for mode in [
            Mode::Normal,
            Mode::Command,
            Mode::Search(Direction::Forward),
        ] {
            assert_eq!(input.action(ctrl_c, mode, false), None);
        }
        assert_eq!(
            input.action(q, Mode::Command, false),
            Some(Action::Insert('q'))
        );
        assert_eq!(
            input.action(q, Mode::Search(Direction::Forward), false),
            Some(Action::Insert('q'))
        );
        assert_eq!(input.action(q, Mode::Normal, true), None);
        assert_eq!(input.action(q, Mode::Normal, false), None);
    }

    #[test]
    fn builtins_and_lua_share_prefixes_and_retry_unmatched_keys() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Normal, "gl", 1, None)?;
        keymap.register(KeymapMode::Normal, "xx", 2, None)?;
        let mut input = Input::default();
        let mut dispatch = |ch| {
            input
                .dispatch(
                    KeyEvent::new(Char(ch), KeyModifiers::NONE),
                    Mode::Normal,
                    false,
                    &keymap,
                )
                .map(|dispatch| dispatch.action)
        };
        assert_eq!(dispatch('g'), None);
        assert_eq!(
            dispatch('g'),
            Some(InputAction::Builtin(Action::Move(Motion::Start)))
        );
        assert_eq!(dispatch('g'), None);
        assert_eq!(dispatch('l'), Some(InputAction::Lua(1)));
        assert_eq!(dispatch('g'), None);
        assert_eq!(
            dispatch('j'),
            Some(InputAction::Builtin(Action::Move(Motion::LineDown)))
        );
        assert_eq!(dispatch('g'), None);
        assert_eq!(dispatch('x'), None);
        assert_eq!(dispatch('x'), Some(InputAction::Lua(2)));
        Ok(())
    }

    #[test]
    fn failed_prefix_recovers_complete_mappings_in_its_suffix() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Normal, "xggl", 1, None)?;
        keymap.register(KeymapMode::Normal, "g:foo", 2, None)?;
        let start = Instant::now();
        for (prefix, unmatched, kind, action) in [
            ("xgg", 'j', KeyEventKind::Press, Action::Move(Motion::Start)),
            (
                "xgg",
                'j',
                KeyEventKind::Repeat,
                Action::Move(Motion::Start),
            ),
            ("g:", 'x', KeyEventKind::Press, Action::StartCommand),
        ] {
            let mut input = Input::default();
            for ch in prefix.chars() {
                let key = KeyEvent::new(Char(ch), KeyModifiers::NONE);
                assert_eq!(
                    input.dispatch_at(key, Mode::Normal, false, &keymap, start),
                    None
                );
            }
            let key = KeyEvent::new_with_kind(Char(unmatched), KeyModifiers::NONE, kind);
            assert_eq!(
                input.dispatch_at(key, Mode::Normal, false, &keymap, start),
                Some(Dispatch {
                    action: InputAction::Builtin(action),
                    replay: vec![key]
                })
            );
        }
        Ok(())
    }

    #[test]
    fn duplicates_are_rejected_and_chord_notation_dispatches() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Normal, "gl", 1, Some("links".to_owned()))?;
        for keys in ["", "gg", "gl", "<C-o>", "<s-f1>", "<oops>"] {
            assert!(
                keymap.register(KeymapMode::Normal, keys, 2, None).is_err(),
                "{keys}"
            );
        }
        keymap.register(KeymapMode::Normal, "<F2><C-x>", 3, None)?;
        assert!(
            keymap
                .register(KeymapMode::Normal, "<f2><c-x>", 4, None)
                .is_err()
        );
        keymap.register(KeymapMode::Normal, "<F2><lt>", 5, None)?;
        let mut input = Input::default();
        let mut dispatch = |code, modifiers| {
            input
                .dispatch(KeyEvent::new(code, modifiers), Mode::Normal, false, &keymap)
                .map(|dispatch| dispatch.action)
        };
        assert_eq!(
            dispatch(Char('o'), KeyModifiers::CONTROL),
            Some(InputAction::Builtin(Action::JumpBack))
        );
        assert_eq!(dispatch(F(2), KeyModifiers::NONE), None);
        assert_eq!(
            dispatch(Char('x'), KeyModifiers::CONTROL),
            Some(InputAction::Lua(3))
        );
        assert_eq!(dispatch(F(2), KeyModifiers::NONE), None);
        assert_eq!(
            dispatch(Char('<'), KeyModifiers::SHIFT),
            Some(InputAction::Lua(5))
        );
        Ok(())
    }

    #[test]
    fn prompt_mappings_respect_existing_input_and_mode_changes() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Command, "<F2><C-x>", 1, None)?;
        keymap.register(KeymapMode::Search, "<F2><C-x>", 2, None)?;
        for keys in ["あ", "<Backspace>", "<Enter>", "<Esc>", "<F3>q"] {
            assert!(keymap.register(KeymapMode::Command, keys, 3, None).is_err());
        }
        let mut input = Input::default();
        let f2 = KeyEvent::new(F(2), KeyModifiers::NONE);
        let ctrl_x = KeyEvent::new(Char('x'), KeyModifiers::CONTROL);
        let text = KeyEvent::new(Char('あ'), KeyModifiers::NONE);
        let search = Mode::Search(Direction::Forward);
        assert_eq!(input.dispatch(f2, Mode::Command, false, &keymap), None);
        assert_eq!(
            input
                .dispatch(text, Mode::Command, false, &keymap)
                .map(|dispatch| dispatch.action),
            Some(InputAction::Builtin(Action::Insert('あ')))
        );
        assert_eq!(input.dispatch(ctrl_x, Mode::Command, false, &keymap), None);
        assert_eq!(input.dispatch(f2, Mode::Command, false, &keymap), None);
        assert_eq!(input.dispatch(ctrl_x, search, false, &keymap), None);
        assert_eq!(input.dispatch(f2, search, false, &keymap), None);
        assert_eq!(
            input
                .dispatch(ctrl_x, search, false, &keymap)
                .map(|dispatch| dispatch.action),
            Some(InputAction::Lua(2))
        );
        Ok(())
    }

    #[test]
    fn ambiguous_shortcuts_wait_for_longer_sequences() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Normal, "z", 1, None)?;
        keymap.register(KeymapMode::Normal, "zz", 2, None)?;
        keymap.register(KeymapMode::Normal, "jx", 3, None)?;
        let mut input = Input::default();
        let start = Instant::now();
        let z = KeyEvent::new(Char('z'), KeyModifiers::NONE);
        let j = KeyEvent::new(Char('j'), KeyModifiers::NONE);
        let x = KeyEvent::new(Char('x'), KeyModifiers::NONE);
        let repeat = KeyEvent::new_with_kind(Char('j'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            input.dispatch_at(z, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input
                .dispatch_at(z, Mode::Normal, false, &keymap, start)
                .map(|dispatch| dispatch.action),
            Some(InputAction::Lua(2))
        );
        assert_eq!(
            input.dispatch_at(z, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(input.expire_at(start + SEQUENCE_TIMEOUT / 2), None);
        assert_eq!(
            input.expire_at(start + SEQUENCE_TIMEOUT),
            Some(Dispatch {
                action: InputAction::Lua(1),
                replay: vec![]
            })
        );
        assert_eq!(
            input.dispatch_at(j, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input.dispatch_at(repeat, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input
                .dispatch_at(x, Mode::Normal, false, &keymap, start)
                .map(|dispatch| dispatch.action),
            Some(InputAction::Lua(3))
        );
        assert_eq!(
            input
                .dispatch_at(repeat, Mode::Normal, false, &keymap, start)
                .map(|dispatch| dispatch.action),
            Some(InputAction::Builtin(Action::Move(Motion::LineDown)))
        );
        Ok(())
    }

    #[test]
    fn ambiguous_shortcut_replays_unmatched_input_after_the_action() -> Result<(), String> {
        let mut keymap = Keymap::default();
        keymap.register(KeymapMode::Normal, ":xy", 1, None)?;
        keymap.register(KeymapMode::Search, "<F2>", 2, None)?;
        keymap.register(KeymapMode::Search, "<F2><F3>", 3, None)?;
        let mut input = Input::default();
        let start = Instant::now();
        let command = KeyEvent::new(Char(':'), KeyModifiers::NONE);
        let x = KeyEvent::new(Char('x'), KeyModifiers::NONE);
        let q = KeyEvent::new(Char('q'), KeyModifiers::NONE);
        assert_eq!(
            input.dispatch_at(command, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input.dispatch_at(x, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input.dispatch_at(q, Mode::Normal, false, &keymap, start),
            Some(Dispatch {
                action: InputAction::Builtin(Action::StartCommand),
                replay: vec![x, q]
            })
        );
        assert_eq!(
            input.dispatch_at(command, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input.dispatch_at(x, Mode::Normal, false, &keymap, start),
            None
        );
        assert_eq!(
            input.expire_at(start + SEQUENCE_TIMEOUT),
            Some(Dispatch {
                action: InputAction::Builtin(Action::StartCommand),
                replay: vec![x]
            })
        );
        let search = Mode::Search(Direction::Forward);
        let f2 = KeyEvent::new(F(2), KeyModifiers::NONE);
        let f3 = KeyEvent::new(F(3), KeyModifiers::NONE);
        assert_eq!(input.dispatch_at(f2, search, false, &keymap, start), None);
        assert_eq!(
            input.dispatch_at(q, search, false, &keymap, start),
            Some(Dispatch {
                action: InputAction::Lua(2),
                replay: vec![q]
            })
        );
        assert_eq!(input.dispatch_at(f2, search, false, &keymap, start), None);
        assert_eq!(
            input.dispatch_at(f3, search, false, &keymap, start + SEQUENCE_TIMEOUT),
            Some(Dispatch {
                action: InputAction::Lua(2),
                replay: vec![f3]
            })
        );
        Ok(())
    }

    #[test]
    fn holding_or_releasing_a_key_does_not_complete_a_sequence() {
        let mut input = Input::default();
        let g = |kind| KeyEvent::new_with_kind(Char('g'), KeyModifiers::NONE, kind);
        for kind in [
            KeyEventKind::Repeat,
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            assert_eq!(input.action(g(kind), Mode::Normal, false), None);
        }
        assert_eq!(
            input.action(g(KeyEventKind::Press), Mode::Normal, false),
            Some(Action::Move(Motion::Start))
        );
        assert_eq!(
            input.action(g(KeyEventKind::Repeat), Mode::Normal, false),
            None
        );
        assert_eq!(
            input.action(g(KeyEventKind::Press), Mode::Normal, false),
            None
        );
        input.reset();
        assert_eq!(
            input.action(g(KeyEventKind::Press), Mode::Normal, false),
            None
        );
    }

    #[test]
    fn plugin_keys_carry_names_and_printable_text() {
        for (code, modifiers, name, text) in [
            (KeyCode::Enter, KeyModifiers::NONE, "Enter", None),
            (Up, KeyModifiers::NONE, "Up", None),
            (Down, KeyModifiers::NONE, "Down", None),
            (KeyCode::Backspace, KeyModifiers::NONE, "Backspace", None),
            (Char('界'), KeyModifiers::NONE, "界", Some('界')),
            (Char('X'), KeyModifiers::SHIFT, "X", Some('X')),
            (Char('x'), KeyModifiers::CONTROL, "C-x", None),
        ] {
            let key = plugin_key(KeyEvent::new(code, modifiers));
            assert_eq!(key.as_ref().map(|key| key.name.as_str()), Some(name));
            assert_eq!(key.and_then(|key| key.text), text);
        }
        assert!(
            plugin_key(KeyEvent::new_with_kind(
                Up,
                KeyModifiers::NONE,
                KeyEventKind::Release
            ))
            .is_none()
        );
        assert!(is_escape(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_escape(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release
        )));
    }
}
