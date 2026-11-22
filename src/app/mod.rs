mod command;
mod document;
mod search;
#[cfg(test)]
mod test_support;

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use command::Command;
pub(crate) use document::PdfDocument;
pub use document::{Document, DocumentLink};
pub use search::Direction;
pub(crate) use search::PdfSearch;

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

#[derive(Clone, Copy)]
pub(crate) enum ScrollEdge {
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
    JumpBack,
    Help,
}

pub struct App {
    document: Document,
    history: Vec<HistoryEntry>,
    mode: Mode,
    prompt: String,
    notice: Option<String>,
    viewport: Viewport,
    help: bool,
    quit: bool,
}

struct HistoryEntry {
    document: Document,
    anchor: ViewAnchor,
}

/// 画像の行番号も記憶する。同じバイト位置では画像がキャプションより前に並ぶ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ViewAnchor {
    pub byte: usize,
    pub image_row: u16,
}

impl ViewAnchor {
    pub const fn text(byte: usize) -> Self {
        Self {
            byte,
            image_row: u16::MAX,
        }
    }
}

/// 表示行と、幅に依存しない文書内位置の対応。
pub trait ViewRows {
    fn len(&self) -> usize;
    fn anchor_at(&self, row: usize) -> Option<ViewAnchor>;

    fn row_at(&self, position: ViewAnchor) -> usize;

    fn row_at_ordered(&self, position: ViewAnchor) -> usize {
        let (mut start, mut end) = (0, self.len());
        while start < end {
            let mid = usize::midpoint(start, end);
            if self.anchor_at(mid).is_some_and(|anchor| anchor <= position) {
                start = mid + 1;
            } else {
                end = mid;
            }
        }
        // 画像の表示方式が変わっても、同じ論理行の先頭を優先する。
        if self
            .anchor_at(start)
            .is_some_and(|anchor| anchor.byte == position.byte)
            && start
                .checked_sub(1)
                .and_then(|row| self.anchor_at(row))
                .is_none_or(|anchor| anchor.byte != position.byte)
        {
            start
        } else {
            start.saturating_sub(1)
        }
    }
}

#[cfg(test)]
impl<const N: usize> ViewRows for [ViewAnchor; N] {
    fn len(&self) -> usize {
        N
    }

    fn anchor_at(&self, row: usize) -> Option<ViewAnchor> {
        self.get(row).copied()
    }

    fn row_at(&self, position: ViewAnchor) -> usize {
        self.row_at_ordered(position)
    }
}

#[derive(Default)]
struct Viewport {
    top: usize,
    anchor: ViewAnchor,
    height: usize,
}

impl Viewport {
    fn last_top(&self, rows: &impl ViewRows) -> usize {
        rows.len().saturating_sub(self.height.max(1))
    }

    fn reflow(&mut self, rows: &impl ViewRows, height: usize) {
        self.height = height;
        self.top = rows.row_at(self.anchor).min(self.last_top(rows));
    }

    fn show(&mut self, row: usize, rows: &impl ViewRows) {
        self.top = row.min(self.last_top(rows));
        if let Some(anchor) = rows.anchor_at(self.top) {
            self.anchor = anchor;
        }
    }

    fn navigate(&mut self, motion: Motion, rows: &impl ViewRows) {
        if self.height == 0 || rows.len() == 0 {
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
            Motion::End => self.last_top(rows),
        };
        self.show(next, rows);
    }

    fn search_target(&mut self, offset: usize, rows: &impl ViewRows) {
        // 実表示が末尾でクランプしても、リサイズでは一致の開始位置を維持する。
        self.anchor = ViewAnchor::text(offset);
        self.reflow(rows, self.height);
    }
}

impl App {
    pub fn from_document(document: Document) -> Self {
        Self {
            document,
            history: Vec::new(),
            mode: Mode::Normal,
            prompt: String::new(),
            notice: None,
            viewport: Viewport::default(),
            help: false,
            quit: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.document.name
    }

    pub(crate) fn document(&self) -> &Document {
        &self.document
    }

    pub fn document_path(&self) -> &Path {
        &self.document.path
    }

    pub(crate) fn reset_viewport(&mut self, anchor: ViewAnchor) {
        self.viewport = Viewport {
            anchor,
            ..Viewport::default()
        };
    }

    /// A repeated scroll at the actual edge, not a scroll that just reaches the edge.
    pub(crate) fn scroll_edge(&self, motion: Motion, rows: &impl ViewRows) -> Option<ScrollEdge> {
        if self.mode != Mode::Normal || self.help || self.viewport.height == 0 || rows.len() == 0 {
            return None;
        }
        match motion {
            Motion::LineDown | Motion::HalfDown | Motion::PageDown
                if self.viewport.top == self.viewport.last_top(rows) =>
            {
                Some(ScrollEdge::End)
            }
            Motion::LineUp | Motion::HalfUp | Motion::PageUp if self.viewport.top == 0 => {
                Some(ScrollEdge::Start)
            }
            _ => None,
        }
    }

    pub fn document_id(&self) -> u64 {
        self.document.id
    }

    pub fn links(&self) -> &[DocumentLink] {
        self.document.links()
    }

    pub fn resolve_link(&self, id: u64) -> Result<PathBuf, String> {
        self.document.resolve_link(id)
    }

    pub(crate) fn replace_document(&mut self, mut document: Document) {
        document.inherit_search(&mut self.document);
        self.document = document;
    }

    pub fn open_document(&mut self, document: Document) {
        self.document.freeze_for_history();
        self.history.push(HistoryEntry {
            document: std::mem::replace(&mut self.document, document),
            anchor: self.viewport.anchor,
        });
        self.viewport = Viewport::default();
        self.mode = Mode::Normal;
        self.prompt.clear();
        self.notice = None;
    }

    pub fn reset_to_initial_document(&mut self) {
        let initial = self.history.drain(..).next();
        if let Some(mut entry) = initial {
            entry.document.reset_search();
            *self = Self::from_document(entry.document);
        }
    }

    fn jump_back(&mut self) {
        if let Some(entry) = self.history.pop() {
            self.document = entry.document;
            self.viewport = Viewport {
                anchor: entry.anchor,
                ..Viewport::default()
            };
            self.notice = None;
        }
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

    pub fn set_notice(&mut self, message: String) {
        self.notice = Some(message);
    }

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub(crate) fn clear_notice_if(&mut self, message: &str) -> bool {
        if self.notice() != Some(message) {
            return false;
        }
        self.notice = None;
        true
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
        self.document.search_query()
    }

    pub fn search_selection(&self) -> Option<(usize, usize)> {
        self.document.search_selection()
    }

    pub(crate) fn search_highlights_enabled(&self) -> bool {
        self.document.search_highlights_enabled()
    }

    pub fn highlights(&self) -> (&[Range<usize>], Option<&Range<usize>>) {
        self.document.text_highlights()
    }

    pub fn reflow(&mut self, rows: &impl ViewRows, height: usize) {
        self.viewport.reflow(rows, height);
    }

    pub(crate) fn reflow_current_height(&mut self, rows: &impl ViewRows) {
        self.viewport.reflow(rows, self.viewport.height);
    }

    pub(crate) fn center_row(&mut self, rows: &impl ViewRows, row: usize) {
        let top = row.saturating_sub(self.viewport.height / 2);
        self.viewport.show(top, rows);
    }

    pub fn apply(&mut self, action: Action, rows: &impl ViewRows, text: &str) {
        if self.help {
            if action == Action::Cancel {
                self.help = false;
            }
            return;
        }
        match (self.mode, action) {
            (Mode::Normal, Action::Move(motion)) => {
                self.document.navigate(&mut self.viewport, motion, rows);
            }
            (Mode::Normal, Action::StartCommand) => self.start_input(Mode::Command),
            (Mode::Normal, Action::StartSearch(direction)) => {
                self.start_input(Mode::Search(direction));
            }
            (Mode::Normal, Action::RepeatSearch { reverse }) => {
                self.notice = self
                    .document
                    .repeat_search(&mut self.viewport, rows, reverse);
            }
            (Mode::Normal, Action::JumpBack) => self.jump_back(),
            (Mode::Normal, Action::Help) => self.help = true,
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
                let query = self.finish_search_input();
                self.notice =
                    self.document
                        .submit_search(&mut self.viewport, rows, text, query, direction);
            }
            _ => {}
        }
    }

    pub(crate) fn finish_search_input(&mut self) -> String {
        self.mode = Mode::Normal;
        std::mem::take(&mut self.prompt)
    }

    fn start_input(&mut self, mode: Mode) {
        self.mode = mode;
        self.prompt.clear();
        self.notice = None;
    }

    fn execute_command(&mut self) {
        self.mode = Mode::Normal;
        match command::parse(&self.prompt) {
            Some(Command::Quit) => self.quit = true,
            Some(Command::Help) => self.help = true,
            Some(Command::HideHighlights) => self.document.hide_highlights(),
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
        let mut app = App::from_test_markdown("test.md".into(), String::new());
        let anchors = [0, 3, 6, 9, 12, 15, 18, 21, 24, 27].map(ViewAnchor::text);
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
        app.reflow(&[0, 9, 18, 27].map(ViewAnchor::text), 3);
        assert_eq!(app.top_row(), 1);
        app.reflow(&anchors, 4);
        assert_eq!(app.top_row(), 6);
        app.apply(Action::Move(Motion::Start), &anchors, "");
        assert_eq!(app.top_row(), 0);
    }

    #[test]
    fn missing_or_empty_search_does_not_move_the_view() {
        let mut app = App::from_test_markdown("test.md".into(), "猫\n犬".into());
        let anchors = [0, 4].map(ViewAnchor::text);
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
        let mut app = App::from_test_markdown("test.md".into(), String::new());
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

    #[test]
    fn link_ids_are_document_bound_and_paths_resolve_relatively() -> Result<(), String> {
        let make_document = || {
            Document::new(
                "index.md".into(),
                String::new(),
                PathBuf::from("docs/index.md"),
                vec![("Read me".into(), "../README.md".into())],
            )
        };
        let mut app = App::from_document(make_document());
        let original = app.document_id();
        let id = app.links()[0].id;
        assert_eq!(app.links()[0].label, "Read me");
        assert_eq!(app.resolve_link(id)?, PathBuf::from("docs/../README.md"));

        app.open_document(make_document());
        let second = app.document_id();
        let second_link = app.links()[0].id;
        assert_ne!(second, original);
        assert!(app.resolve_link(id).is_err());

        app.open_document(make_document());
        assert!(app.resolve_link(second_link).is_err());
        app.apply(Action::JumpBack, &[], "");
        assert_eq!(app.document_id(), second);
        assert_eq!(app.links()[0].id, second_link);
        app.apply(Action::JumpBack, &[], "");
        assert_eq!(app.document_id(), original);
        assert_eq!(app.links()[0].id, id);
        assert!(app.resolve_link(second_link).is_err());
        app.apply(Action::JumpBack, &[], "");
        assert_eq!(app.document_id(), original);
        Ok(())
    }

    #[test]
    fn resetting_startup_discards_document_jumps() {
        let mut app = App::from_test_markdown("initial.md".into(), String::new());
        let original = app.document_id();
        for path in ["first.md", "second.md"] {
            app.open_document(Document::new(
                path.into(),
                String::new(),
                PathBuf::from(path),
                vec![],
            ));
        }
        app.reset_to_initial_document();
        assert_eq!(app.document_id(), original);
        app.apply(Action::JumpBack, &[], "");
        assert_eq!(app.document_id(), original);
    }

    #[test]
    fn jump_back_restores_the_logical_position_and_hidden_search() {
        let text = "hit\none\nhit\ntwo\nhit\nend";
        let anchors = [0, 4, 8, 12, 16, 20].map(ViewAnchor::text);
        let mut app = App::from_test_markdown("test.md".into(), text.into());
        app.reflow(&anchors, 1);
        for action in [
            Action::Move(Motion::End),
            Action::StartSearch(Direction::Backward),
            Action::Insert('h'),
            Action::Insert('i'),
            Action::Insert('t'),
            Action::Submit,
            Action::StartCommand,
            Action::Insert('n'),
            Action::Insert('o'),
            Action::Insert('h'),
            Action::Submit,
            Action::Move(Motion::LineUp),
        ] {
            app.apply(action, &anchors, text);
        }
        assert_eq!(app.top_row(), 3);
        assert_eq!(app.search_selection(), Some((3, 3)));
        assert_eq!(app.highlights(), (&[][..], None));

        app.open_document(Document::new(
            "next.md".into(),
            "next".into(),
            PathBuf::from("next.md"),
            vec![],
        ));
        app.reflow(&[ViewAnchor::text(0)], 1);
        assert_eq!(app.top_row(), 0);
        assert_eq!(app.search_query(), "");
        assert_eq!(app.search_selection(), None);

        app.apply(Action::JumpBack, &[ViewAnchor::text(0)], "next");
        app.reflow(&[0, 8, 16, 20].map(ViewAnchor::text), 2);
        assert_eq!(app.top_row(), 1);
        app.reflow(&anchors, 1);
        assert_eq!(app.top_row(), 3);
        assert_eq!(app.search_query(), "hit");
        assert_eq!(app.search_selection(), Some((3, 3)));
        assert_eq!(app.highlights(), (&[][..], None));
        app.apply(Action::RepeatSearch { reverse: false }, &anchors, text);
        assert_eq!(app.top_row(), 2);
        assert_eq!(app.search_selection(), Some((2, 3)));
        assert_eq!(app.highlights().1, Some(&(8..11)));
    }
}
