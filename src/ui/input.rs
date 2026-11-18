use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{Action, Direction, Mode, Motion};

#[derive(Default)]
pub struct Input {
    pending_g: bool,
}

impl Input {
    pub fn action(&mut self, key: KeyEvent, mode: Mode, help_open: bool) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let unmodified = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let pending_g = std::mem::take(&mut self.pending_g);
        if help_open {
            return (unmodified && key.code == KeyCode::Esc).then_some(Action::Cancel);
        }
        if mode != Mode::Normal {
            return self::prompt_key(key.code, unmodified);
        }
        if key.modifiers == KeyModifiers::CONTROL {
            let motion = match key.code {
                KeyCode::Char('d') => Motion::HalfDown,
                KeyCode::Char('u') => Motion::HalfUp,
                KeyCode::Char('f') => Motion::PageDown,
                KeyCode::Char('b') => Motion::PageUp,
                _ => return None,
            };
            return Some(Action::Move(motion));
        }
        if !unmodified {
            return None;
        }
        Some(match key.code {
            KeyCode::Char('j') | KeyCode::Down => Action::Move(Motion::LineDown),
            KeyCode::Char('k') | KeyCode::Up => Action::Move(Motion::LineUp),
            KeyCode::PageDown => Action::Move(Motion::PageDown),
            KeyCode::PageUp => Action::Move(Motion::PageUp),
            KeyCode::Char('G') => Action::Move(Motion::End),
            KeyCode::Char('g') if key.kind == KeyEventKind::Repeat => {
                self.pending_g = pending_g;
                return None;
            }
            KeyCode::Char('g') if pending_g => Action::Move(Motion::Start),
            KeyCode::Char('g') => {
                self.pending_g = true;
                return None;
            }
            KeyCode::Char(':') => Action::StartCommand,
            KeyCode::Char('/') => Action::StartSearch(Direction::Forward),
            KeyCode::Char('?') => Action::StartSearch(Direction::Backward),
            KeyCode::Char('n' | 'N') => Action::RepeatSearch {
                reverse: key.code == KeyCode::Char('N'),
            },
            KeyCode::Char('q') => Action::Quit,
            KeyCode::F(1) => Action::Help,
            _ => return None,
        })
    }
}

fn prompt_key(code: KeyCode, unmodified: bool) -> Option<Action> {
    if !unmodified {
        return None;
    }
    match code {
        KeyCode::Char(ch) if !ch.is_control() => Some(Action::Insert(ch)),
        KeyCode::Backspace => Some(Action::Backspace),
        KeyCode::Enter => Some(Action::Submit),
        KeyCode::Esc => Some(Action::Cancel),
        _ => None,
    }
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
    fn prompt_characters_are_not_normal_bindings_and_control_c_is_unbound() {
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
        assert_eq!(input.action(q, Mode::Normal, false), Some(Action::Quit));
    }
}
