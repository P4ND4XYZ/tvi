mod command;
mod document;
mod search;

use std::ops::Range;

use command::Command;
use document::Document;
pub use search::Direction;
use search::Search;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command,
    Search(Direction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    LineDown,
    LineUp,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Motion),
    StartCommand,
    StartSearch(Direction),
    Insert(char),
    Backspace,
    Cancel,
    Submit,
    RepeatSearch { reverse: bool },
    Help,
    Quit,
}

pub struct App {
    document: Document,
    mode: Mode,
    prompt: String,
    notice: Option<String>,
    search: Search,
    viewport: Viewport,
    help: bool,
    quit: bool,
}

#[derive(Default)]
struct Viewport {
    top: usize,
    anchor: usize,
    height: usize,
}

impl Viewport {
    fn last_top(&self, anchors: &[usize]) -> usize {
        anchors.len().saturating_sub(self.height.max(1))
    }

    fn row_at(anchors: &[usize], offset: usize) -> usize {
        anchors
            .partition_point(|&anchor| anchor <= offset)
            .saturating_sub(1)
    }

    fn reflow(&mut self, anchors: &[usize], height: usize) {
        self.height = height;
        self.top = Self::row_at(anchors, self.anchor).min(self.last_top(anchors));
    }

    fn show(&mut self, row: usize, anchors: &[usize]) {
        self.top = row.min(self.last_top(anchors));
        if let Some(&anchor) = anchors.get(self.top) {
            self.anchor = anchor;
        }
    }

    fn navigate(&mut self, motion: Motion, anchors: &[usize]) {
        if self.height == 0 || anchors.is_empty() {
            return;
        }
        let half = (self.height / 2).max(1);
        let next = match motion {
            Motion::LineDown => self.top.saturating_add(1),
            Motion::LineUp => self.top.saturating_sub(1),
            Motion::HalfDown => self.top.saturating_add(half),
            Motion::HalfUp => self.top.saturating_sub(half),
            Motion::PageDown => self.top.saturating_add(self.height),
            Motion::PageUp => self.top.saturating_sub(self.height),
            Motion::Start => 0,
            Motion::End => self.last_top(anchors),
        };
        self.show(next, anchors);
    }

    fn search_target(&mut self, offset: usize, anchors: &[usize]) {
        // 実表示が末尾でクランプしても、リサイズでは一致の開始位置を維持する。
        self.anchor = offset;
        self.reflow(anchors, self.height);
    }
}

impl App {
    pub fn new(name: String, markdown: String) -> Self {
        Self {
            document: Document { name, markdown },
            mode: Mode::Normal,
            prompt: String::new(),
            notice: None,
            search: Search::default(),
            viewport: Viewport::default(),
            help: false,
            quit: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.document.name
    }

    pub fn markdown(&self) -> &str {
        &self.document.markdown
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn help_open(&self) -> bool {
        self.help
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn top_row(&self) -> usize {
        self.viewport.top
    }

    pub fn search_query(&self) -> &str {
        self.search.query()
    }

    pub fn search_selection(&self) -> Option<(usize, usize)> {
        self.search.selection()
    }

    pub fn highlights(&self) -> (&[Range<usize>], Option<&Range<usize>>) {
        if self.search.highlights {
            (self.search.matches(), self.search.current())
        } else {
            (&[], None)
        }
    }

    pub fn reflow(&mut self, anchors: &[usize], height: usize) {
        self.viewport.reflow(anchors, height);
    }

    pub fn apply(&mut self, action: Action, anchors: &[usize], text: &str) {
        if self.help {
            if action == Action::Cancel {
                self.help = false;
            }
            return;
        }
        match (self.mode, action) {
            (Mode::Normal, Action::Move(motion)) => self.viewport.navigate(motion, anchors),
            (Mode::Normal, Action::StartCommand) => self.start_input(Mode::Command),
            (Mode::Normal, Action::StartSearch(direction)) => {
                self.start_input(Mode::Search(direction));
            }
            (Mode::Normal, Action::RepeatSearch { reverse }) => {
                self.search.step(reverse);
                self.reveal_search(anchors);
            }
            (Mode::Normal, Action::Help) => self.help = true,
            (Mode::Normal, Action::Quit) => self.quit = true,
            (Mode::Command | Mode::Search(_), Action::Insert(ch)) if !ch.is_control() => {
                self.prompt.push(ch);
            }
            (Mode::Command | Mode::Search(_), Action::Backspace) => {
                if let Some((offset, _)) = self.prompt.char_indices().next_back() {
                    self.prompt.truncate(offset);
                }
            }
            (Mode::Command | Mode::Search(_), Action::Cancel) => {
                self.prompt.clear();
                self.mode = Mode::Normal;
            }
            (Mode::Command, Action::Submit) => self.execute_command(),
            (Mode::Search(direction), Action::Submit) => {
                let query = std::mem::take(&mut self.prompt);
                self.mode = Mode::Normal;
                let offset = anchors
                    .get(self.viewport.top)
                    .copied()
                    .unwrap_or(self.viewport.anchor);
                self.search.submit(query, direction, text, offset);
                self.reveal_search(anchors);
            }
            _ => {}
        }
    }

    fn start_input(&mut self, mode: Mode) {
        self.mode = mode;
        self.prompt.clear();
        self.notice = None;
    }

    fn reveal_search(&mut self, anchors: &[usize]) {
        self.notice = if let Some(hit) = self.search.current() {
            self.viewport.search_target(hit.start, anchors);
            None
        } else if self.search.query().is_empty() {
            Some("No previous search".into())
        } else {
            Some("No matches".into())
        };
    }

    fn execute_command(&mut self) {
        self.mode = Mode::Normal;
        match command::parse(&self.prompt) {
            Some(Command::Quit) => self.quit = true,
            Some(Command::Help) => self.help = true,
            Some(Command::HideHighlights) => self.search.highlights = false,
            None => self.notice = Some("Unknown command".into()),
        }
        self.prompt.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motions_clamp_to_a_full_final_viewport_and_anchor_reflows() {
        let mut app = App::new("test.md".into(), String::new());
        let anchors = [0, 3, 6, 9, 12, 15, 18, 21, 24, 27];
        app.reflow(&anchors, 4);
        for (motion, row) in [
            (Motion::LineUp, 0),
            (Motion::LineDown, 1),
            (Motion::HalfDown, 3),
            (Motion::HalfUp, 1),
            (Motion::PageDown, 5),
            (Motion::PageDown, 6),
            (Motion::LineDown, 6),
            (Motion::PageUp, 2),
            (Motion::End, 6),
        ] {
            app.apply(Action::Move(motion), &anchors, "");
            assert_eq!(app.top_row(), row, "{motion:?}");
        }
        app.reflow(&[0, 9, 18, 27], 3);
        assert_eq!(app.top_row(), 1);
        app.reflow(&anchors, 4);
        assert_eq!(app.top_row(), 6);
        app.apply(Action::Move(Motion::Start), &anchors, "");
        assert_eq!(app.top_row(), 0);
    }

    #[test]
    fn missing_or_empty_search_does_not_move_the_view() {
        let mut app = App::new("test.md".into(), "猫\n犬".into());
        let anchors = [0, 4];
        let text = "猫\n犬";
        app.reflow(&anchors, 1);
        app.apply(Action::Move(Motion::End), &anchors, text);
        for query in ["", "犬q"] {
            app.apply(Action::StartSearch(Direction::Forward), &anchors, text);
            for ch in query.chars() {
                app.apply(Action::Insert(ch), &anchors, text);
            }
            app.apply(Action::Submit, &anchors, text);
            assert_eq!(app.mode(), Mode::Normal);
            assert_eq!(app.top_row(), 1);
            assert!(app.highlights().0.is_empty());
            assert!(!app.should_quit());
            assert_eq!(
                app.notice(),
                Some(if query.is_empty() {
                    "No previous search"
                } else {
                    "No matches"
                })
            );
        }
    }

    #[test]
    fn command_backspace_cancel_and_errors_keep_normal_interaction() {
        let mut app = App::new("test.md".into(), String::new());
        let mut apply = |action| {
            app.apply(action, &[], "");
            (app.mode(), app.should_quit(), app.notice().is_some())
        };
        apply(Action::StartCommand);
        apply(Action::Insert('q'));
        assert_eq!(apply(Action::Cancel), (Mode::Normal, false, false));
        apply(Action::StartCommand);
        apply(Action::Insert('z'));
        assert_eq!(apply(Action::Submit), (Mode::Normal, false, true));
        apply(Action::StartCommand);
        apply(Action::Insert('q'));
        apply(Action::Insert('猫'));
        apply(Action::Backspace);
        assert_eq!(apply(Action::Submit), (Mode::Normal, true, false));
    }
}
