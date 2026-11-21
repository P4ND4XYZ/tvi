use ratatui::{
    Frame,
    buffer::{Buffer, CellWidth},
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::colorscheme::{Colorscheme, Group};

#[derive(Default)]
pub struct FloatOptions {
    pub title: Option<String>,
    pub header: Option<String>,
    pub lines: Option<Vec<String>>,
    /// 外側の `None` は省略、内側の `None` は強調解除、数値は1始まりの行番号。
    #[expect(
        clippy::option_option,
        reason = "omitted, clear, and set are distinct updates"
    )]
    pub focus_line: Option<Option<usize>>,
}

#[derive(Default)]
pub struct Floats {
    current: Option<Float>,
    last_id: u64,
}

#[derive(Default)]
struct Float {
    handle: (u64, u64),
    title: String,
    header: Option<String>,
    lines: Vec<String>,
    lines_width: u16,
    focus_line: Option<usize>,
    top_row: usize,
}

impl Floats {
    pub fn open(&mut self, view: u64, options: FloatOptions) -> Result<u64, String> {
        if self.current.is_some() {
            return Err("a float is already open".into());
        }
        let id = self.last_id.checked_add(1).ok_or("float IDs exhausted")?;
        let mut state = Float {
            handle: (view, id),
            ..Float::default()
        };
        state.apply(options)?;
        self.last_id = id;
        self.current = Some(state);
        Ok(id)
    }

    pub fn update(&mut self, view: u64, float: u64, options: FloatOptions) -> Result<(), String> {
        self.current
            .as_mut()
            .filter(|state| state.handle == (view, float))
            .ok_or("float is closed or belongs to a different view")?
            .apply(options)
    }

    pub fn close(&mut self, view: u64, float: u64) -> Result<(), String> {
        if self.active() != Some((view, float)) {
            return Err("float is closed or belongs to a different view".into());
        }
        self.current = None;
        Ok(())
    }

    #[must_use]
    pub fn active(&self) -> Option<(u64, u64)> {
        self.current.as_ref().map(|state| state.handle)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, area: Rect, scheme: &Colorscheme) {
        let area = frame.area().intersection(area);
        if let Some(state) = &mut self.current
            && !area.is_empty()
        {
            state.draw(frame, area, scheme);
        }
    }
}

impl Float {
    fn apply(&mut self, options: FloatOptions) -> Result<(), String> {
        let count = options.lines.as_ref().map_or(self.lines.len(), Vec::len);
        if let Some(Some(line)) = options.focus_line
            && !(1..=count).contains(&line)
        {
            return Err(format!("focus_line {line} is outside the float body"));
        }

        if let Some(title) = options.title {
            self.title = single_line(title);
        }
        if let Some(header) = options.header {
            self.header = Some(single_line(header));
        }
        if let Some(lines) = options.lines {
            self.lines = lines.into_iter().map(single_line).collect();
            self.lines_width = self
                .lines
                .iter()
                .map(|line| cell_width(line))
                .max()
                .unwrap_or(0);
        }
        if let Some(focus) = options.focus_line {
            self.focus_line = focus;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame<'_>, bounds: Rect, scheme: &Colorscheme) {
        let header = self.header.as_deref();
        let framed = bounds.width >= 3
            && bounds.height >= 3 + u16::from(header.is_some() && !self.lines.is_empty());
        let border = if framed { 2 } else { 0 };
        let width = cell_width(&self.title)
            .max(header.map_or(0, cell_width))
            .max(self.lines_width)
            .max(1)
            .saturating_add(border)
            .min(bounds.width);
        let content_height = (self.lines.len() + usize::from(header.is_some()))
            .max(1)
            .saturating_add(usize::from(border));
        let height = u16::try_from(content_height)
            .unwrap_or(u16::MAX)
            .min(bounds.height);
        let area = Rect::new(
            bounds.x + (bounds.width - width) / 2,
            bounds.y + (bounds.height - height) / 2,
            width,
            height,
        );
        clear_crossing_graphemes(frame.buffer_mut(), area);
        frame.render_widget(Clear, area);
        let normal = scheme.apply(Group::FloatNormal, Style::default());
        frame.buffer_mut().set_style(area, normal);
        let mut content = area;
        if framed {
            let border_style = scheme.apply(Group::FloatBorder, normal);
            let block = Block::bordered()
                .border_style(border_style)
                .title_style(border_style)
                .title(clip(&self.title, area.width - border));
            content = block.inner(area);
            frame.render_widget(block, area);
        }

        if let Some(header) = header.filter(|_| content.height > 1 || self.lines.is_empty()) {
            draw_row(
                frame,
                content,
                header,
                scheme.apply(
                    Group::FloatNormal,
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            );
            content.y += 1;
            content.height -= 1;
        } else if !framed && self.lines.is_empty() {
            draw_row(
                frame,
                content,
                &self.title,
                scheme.apply(
                    Group::FloatBorder,
                    scheme.apply(
                        Group::FloatNormal,
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                ),
            );
        }
        if content.is_empty() || self.lines.is_empty() {
            return;
        }

        let count = usize::from(content.height);
        self.top_row = self.top_row.min(self.lines.len().saturating_sub(count));
        let focus = self
            .focus_line
            .filter(|&line| line <= self.lines.len())
            .map(|line| line - 1);
        if let Some(focus) = focus {
            self.top_row = self.top_row.clamp((focus + 1).saturating_sub(count), focus);
        }
        for (y, (index, line)) in
            (content.top()..content.bottom()).zip(self.lines.iter().enumerate().skip(self.top_row))
        {
            let modifier = if Some(index) == focus {
                Modifier::REVERSED
            } else {
                Modifier::empty()
            };
            draw_row(
                frame,
                Rect { y, ..content },
                line,
                scheme.apply(Group::FloatNormal, Style::new().add_modifier(modifier)),
            );
        }
    }
}

fn clear_crossing_graphemes(buffer: &mut Buffer, area: Rect) {
    let bounds = buffer.area;
    if area.left() == bounds.left() && area.right() == bounds.right() {
        return;
    }
    // Ratatui の差分描画では、全角セルの後続セルには別の文字を置けない。
    for y in area.top()..area.bottom() {
        let mut x = bounds.left();
        while x < area.right() {
            let end = x.saturating_add(buffer[(x, y)].cell_width().max(1));
            if (x < area.left() && area.left() < end) || area.right() < end {
                for clear_x in x..end.min(bounds.right()) {
                    buffer[(clear_x, y)].reset();
                }
            }
            x = end;
        }
    }
}

fn single_line(text: String) -> String {
    if text.contains(char::is_control) {
        text.replace(char::is_control, " ")
    } else {
        text
    }
}

fn cell_width(text: &str) -> u16 {
    text.graphemes(true).fold(0u16, |width, grapheme| {
        width.saturating_add(grapheme.cell_width())
    })
}

fn clip(text: &str, mut width: u16) -> &str {
    for (start, grapheme) in text.grapheme_indices(true) {
        let cells = grapheme.cell_width();
        if cells > width {
            return &text[..start];
        }
        width -= cells;
    }
    text
}

fn draw_row(frame: &mut Frame<'_>, area: Rect, text: &str, style: Style) {
    frame.render_widget(
        Paragraph::new(Line::from(clip(text, area.width))).style(style),
        Rect { height: 1, ..area },
    );
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    use super::*;

    #[test]
    fn only_the_current_handle_can_change_the_float() -> Result<(), String> {
        let mut floats = Floats::default();
        let first = floats.open(5, FloatOptions::default())?;
        assert!(floats.open(5, FloatOptions::default()).is_err());
        assert!(floats.close(6, first).is_err());
        assert!(floats.update(6, first, FloatOptions::default()).is_err());
        floats.close(5, first)?;
        let next = floats.open(5, FloatOptions::default())?;
        assert_ne!(first, next);
        assert!(floats.update(5, first, FloatOptions::default()).is_err());
        assert!(floats.close(5, first).is_err());
        assert_eq!(floats.active(), Some((5, next)));
        Ok(())
    }

    #[test]
    fn updates_preserve_omitted_fields_and_reject_invalid_focus_atomically() -> Result<(), String> {
        let mut floats = Floats::default();
        let id = floats.open(
            1,
            FloatOptions {
                title: Some("Links".into()),
                header: Some("Find".into()),
                lines: Some(vec!["first".into(), "second".into()]),
                focus_line: Some(Some(2)),
            },
        )?;
        let replacement = || FloatOptions {
            title: Some("New".into()),
            lines: Some(vec!["one".into()]),
            ..FloatOptions::default()
        };
        assert!(
            floats
                .update(
                    1,
                    id,
                    FloatOptions {
                        focus_line: Some(Some(2)),
                        ..replacement()
                    }
                )
                .is_err()
        );
        let state = floats.current.as_ref().ok_or("expected an open float")?;
        assert_eq!(state.title, "Links");
        assert_eq!(state.lines, ["first", "second"]);
        assert_eq!(state.focus_line, Some(2));

        floats.update(1, id, replacement())?;
        let state = floats.current.as_ref().ok_or("expected an open float")?;
        assert_eq!(state.title, "New");
        assert_eq!(state.header.as_deref(), Some("Find"));
        assert_eq!(state.lines, ["one"]);
        assert_eq!(state.focus_line, Some(2));

        floats.update(
            1,
            id,
            FloatOptions {
                focus_line: Some(None),
                ..FloatOptions::default()
            },
        )?;
        let state = floats.current.as_ref().ok_or("expected an open float")?;
        assert_eq!(state.focus_line, None);
        Ok(())
    }

    #[test]
    fn focus_scrolls_the_body_under_a_fixed_header_and_survives_resize()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut floats = Floats::default();
        let id = floats.open(
            1,
            FloatOptions {
                title: Some("Links".into()),
                header: Some("Find".into()),
                lines: Some((1..=8).map(|number| format!("item {number}")).collect()),
                focus_line: Some(Some(8)),
            },
        )?;
        let mut terminal = Terminal::new(TestBackend::new(12, 7))?;
        let paint = |terminal: &mut Terminal<TestBackend>, floats: &mut Floats| {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let body = Rect {
                        height: area.height.saturating_sub(2),
                        ..area
                    };
                    frame.render_widget(
                        Paragraph::new("Status\nPrompt"),
                        Rect {
                            y: body.bottom(),
                            height: 2,
                            ..area
                        },
                    );
                    floats.draw(frame, body, &Colorscheme::default());
                })
                .map(|_| ())
        };
        paint(&mut terminal, &mut floats)?;
        let buffer = terminal.backend().buffer();
        assert!(row(buffer, 0).contains("Links"));
        assert!(row(buffer, 1).contains("Find"));
        assert!(row(buffer, 2).contains("item 7"));
        assert!(row(buffer, 3).contains("item 8"));
        assert!(buffer[(3, 3)].modifier.contains(Modifier::REVERSED));
        assert!(row(buffer, 5).starts_with("Status"));
        assert!(row(buffer, 6).starts_with("Prompt"));

        floats.update(
            1,
            id,
            FloatOptions {
                focus_line: Some(Some(2)),
                ..FloatOptions::default()
            },
        )?;
        paint(&mut terminal, &mut floats)?;
        let buffer = terminal.backend().buffer();
        assert!(row(buffer, 1).contains("Find"));
        assert!(row(buffer, 2).contains("item 2"));
        assert!(buffer[(3, 2)].modifier.contains(Modifier::REVERSED));
        assert!(row(buffer, 3).contains("item 3"));

        terminal.backend_mut().resize(4, 3);
        paint(&mut terminal, &mut floats)?;
        let buffer = terminal.backend().buffer();
        assert_eq!(row(buffer, 0), "item");
        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(row(buffer, 1), "Stat");
        Ok(())
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn float_edges_repaint_over_wide_body_glyphs() -> Result<(), Box<dyn std::error::Error>> {
        let mut floats = Floats::default();
        let mut terminal = Terminal::new(TestBackend::new(23, 7))?;
        let paint = |terminal: &mut Terminal<TestBackend>, floats: &mut Floats| {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(
                        Paragraph::new("日本日本日本A日本日本日本"),
                        Rect {
                            y: 3,
                            height: 1,
                            ..area
                        },
                    );
                    frame.buffer_mut()[(14, 3)].set_bg(Color::Blue);
                    floats.draw(frame, area, &Colorscheme::default());
                })
                .map(|completed| completed.buffer[(14, 3)].bg)
        };
        assert_eq!(paint(&mut terminal, &mut floats)?, Color::Blue);
        let id = floats.open(
            1,
            FloatOptions {
                lines: Some(vec!["xxx".into()]),
                ..FloatOptions::default()
            },
        )?;
        assert_eq!(paint(&mut terminal, &mut floats)?, Color::Reset);
        let buffer = terminal.backend().buffer();
        let middle: String = (8..=14).map(|x| buffer[(x, 3)].symbol()).collect();
        assert_eq!(middle, " │xxx│ ");
        assert_eq!(buffer[(6, 3)].symbol(), "本");
        assert_eq!(buffer[(15, 3)].symbol(), "本");

        floats.close(1, id)?;
        assert_eq!(paint(&mut terminal, &mut floats)?, Color::Blue);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(8, 3)].symbol(), "日");
        assert_eq!(buffer[(13, 3)].symbol(), "日");
        Ok(())
    }

    #[test]
    fn clipping_preserves_grapheme_boundaries() {
        assert_eq!(clip("a\u{301}日b", 1), "a\u{301}");
        assert_eq!(clip("a\u{301}日b", 2), "a\u{301}");
        assert_eq!(clip("a\u{301}日b", 3), "a\u{301}日");
        assert_eq!(
            single_line("one\rtwo\nthree\tend".into()),
            "one two three end"
        );
    }
}
