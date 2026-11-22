pub mod float;
mod images;
pub mod input;
pub mod layout;

use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    app::{App, Direction, Mode},
    colorscheme::{Colorscheme, Group},
    markdown::tui_markdown::apply_colorscheme,
};
use layout::Layout;

#[cfg(test)]
use crate::markdown::{MarkdownRenderer, RenderedMarkdown};

#[cfg(test)]
fn prepare(renderer: &impl MarkdownRenderer, markdown: &str, width: u16) -> RenderedMarkdown {
    renderer.render(markdown, width)
}

pub fn body_area(area: Rect, mode: Mode) -> Rect {
    let chrome = if mode == Mode::Normal { 1 } else { 2 };
    Rect {
        height: area.height.saturating_sub(chrome),
        ..area
    }
}

pub fn draw(
    frame: &mut Frame<'_>,
    app: &App,
    layout: &Layout<'_>,
    float_active: bool,
    scheme: &Colorscheme,
) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    let body = body_area(area, app.mode());
    let normal = scheme.apply(Group::DocumentNormal, Style::default());
    let text = layout.text();
    if app.help_open() && !body.is_empty() {
        frame.render_widget(
            Paragraph::new(HELP).style(normal).wrap(Wrap { trim: true }),
            body,
        );
    } else if !body.is_empty() {
        let lines = text
            .lines
            .iter()
            .skip(app.top_row())
            .take(usize::from(body.height))
            .cloned()
            .map(|mut line| {
                apply_colorscheme(&mut line, scheme);
                line
            });
        frame.render_widget(
            Paragraph::new(lines.collect::<Text<'_>>()).style(normal.patch(text.style)),
            body,
        );
        highlight(frame, app, body, layout, scheme);
        if !float_active {
            layout.draw_images(frame, body, app.top_row());
        }
    }
    let status = Rect::new(area.x, body.bottom(), area.width, 1);
    frame.render_widget(
        Paragraph::new(status_line(app, layout.rows().len()))
            .style(scheme.apply(Group::Status, Style::new().add_modifier(Modifier::REVERSED))),
        status,
    );

    if let Some(prefix) = prompt_prefix(app.mode()) {
        let prompt_y = area.bottom().saturating_sub(1);
        let prompt_area = Rect::new(area.x, prompt_y, area.width, 1);
        let (suffix, width) = prompt_suffix(app.prompt(), area.width - 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![prefix.into(), suffix.into()]))
                .style(scheme.apply(Group::Prompt, Style::default())),
            prompt_area,
        );
        if !float_active && width < area.width - 1 {
            frame.set_cursor_position(Position::new(area.x + 1 + width, prompt_y));
        }
    }
}

fn prompt_prefix(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Normal => None,
        Mode::Command => Some(":"),
        Mode::Search(Direction::Forward) => Some("/"),
        Mode::Search(Direction::Backward) => Some("?"),
    }
}

fn prompt_suffix(prompt: &str, budget: u16) -> (&str, u16) {
    let mut start = prompt.len();
    let mut width = 0;
    for (offset, grapheme) in prompt.grapheme_indices(true).rev() {
        let next = grapheme.cell_width();
        if next > budget - width {
            break;
        }
        start = offset;
        width += next;
    }
    (&prompt[start..], width)
}

fn status_line(app: &App, len: usize) -> String {
    use std::fmt::Write as _;

    let mode = match app.mode() {
        Mode::Normal => "NORMAL",
        Mode::Command => "COMMAND",
        Mode::Search(_) => "SEARCH",
    };
    let row = if len == 0 { 0 } else { app.top_row() + 1 };
    let mut text = if let Some(pdf) = app.document().as_pdf() {
        let page = pdf.page();
        format!(
            "{mode}  PDF {}/{}  {row}/{len}",
            page.index + 1,
            page.source.count
        )
    } else {
        format!("{mode}  {row}/{len}")
    };
    if let Some((index, total)) = app.search_selection() {
        let state = if app.search_highlights_enabled() {
            ""
        } else {
            " off"
        };
        let result = write!(text, "  [{index}/{total}{state}]");
        debug_assert!(result.is_ok(), "formatting to String cannot fail");
    } else if !app.search_query().is_empty() {
        text.push_str("  [0/0]");
    }
    if let Some(notice) = app.notice() {
        text.push_str("  ");
        text.push_str(notice);
    }
    text.push_str("  ");
    text.push_str(app.name());
    text
}

fn highlight(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    layout: &Layout<'_>,
    scheme: &Colorscheme,
) {
    let (matches, current) = app.highlights();
    if matches.is_empty() {
        return;
    }
    for (y, row) in (area.top()..area.bottom()).zip(layout.rows().iter().skip(app.top_row())) {
        let Some((first, last)) = row.cells.first().zip(row.cells.last()) else {
            continue;
        };
        let start = matches.partition_point(|hit| hit.end <= first.bytes.start);
        for hit in matches[start..]
            .iter()
            .take_while(|hit| hit.start < last.bytes.end)
            .filter(|&hit| Some(hit) != current)
            .chain(current)
        {
            let style = if Some(hit) == current {
                scheme.apply(
                    Group::SearchCurrent,
                    Style::new().bg(Color::Yellow).fg(Color::Black),
                )
            } else {
                scheme.apply(
                    Group::SearchMatch,
                    Style::new().bg(Color::Blue).fg(Color::White),
                )
            };
            for cell in row.cells_for(hit.clone()) {
                let (Ok(left), Ok(right)) = (
                    u16::try_from(cell.columns.start),
                    u16::try_from(cell.columns.end),
                ) else {
                    continue;
                };
                frame
                    .buffer_mut()
                    .set_style(Rect::new(area.x + left, y, right - left, 1), style);
            }
        }
    }
}

const HELP: &str = "Help (Esc to close)\n\
    j/↓ k/↑  Line down/up\n\
    Ctrl-d/u  Half page   Ctrl-f/b Page   PgDn/PgUp Page\n\
    gg/G  Start/end   : Command   / ? Search forward/backward\n\
    n/N  Next/reverse match   :noh / :nohlsearch Hide highlights\n\
    ff  Find files   Ctrl-o  Return from a file jump\n\
    Backspace Edit   Enter Confirm   Esc Cancel\n\
    F1 / :help Help   :q / :quit Quit";

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::markdown::tui_markdown::TuiMarkdown;
    use input::Input;

    fn paint(
        terminal: &mut Terminal<TestBackend>,
        app: &mut App,
        doc: &RenderedMarkdown,
    ) -> Result<(), Infallible> {
        terminal.draw(|frame| {
            let layout = Layout::at_width(doc, frame.area().width);
            app.reflow(
                &layout,
                usize::from(body_area(frame.area(), app.mode()).height),
            );
            draw(frame, app, &layout, false, &Colorscheme::default());
        })?;
        Ok(())
    }

    fn press(
        input: &mut Input,
        app: &mut App,
        doc: &RenderedMarkdown,
        width: u16,
        keys: impl IntoIterator<Item = KeyCode>,
    ) {
        let locations = Layout::at_width(doc, width);
        for code in keys {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            if let Some(action) = input.action(key, app.mode(), app.help_open()) {
                app.apply(action, &locations, doc.search_text());
            }
        }
    }

    #[test]
    fn colorscheme_layers_document_markdown_and_search_without_tinting_code_or_images()
    -> Result<(), Infallible> {
        use KeyCode::{Char, Enter};

        let source = "# find second find\n\n[link](path) `inline` ![`alt`](x.png)\n\n> quote\n\n```\nfind code\n```\n\n```mermaid\ngraph TD\n```";
        let mut app = App::from_test_markdown("test.md".into(), source.into());
        let doc = prepare(&TuiMarkdown, source, 40);
        let layout = Layout::at_width(&doc, 40);
        let mut terminal = Terminal::new(TestBackend::new(40, 25))?;
        let mut input = Input::default();
        let mut scheme = Colorscheme::default();
        scheme.set(
            Group::DocumentNormal,
            Style::new().fg(Color::Red).bg(Color::Green),
        );
        scheme.set(
            Group::Heading1,
            Style::new()
                .bg(Color::Reset)
                .remove_modifier(Modifier::UNDERLINED),
        );
        scheme.set(Group::Link, Style::new().fg(Color::Magenta));
        scheme.set(
            Group::Quote,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::ITALIC),
        );
        scheme.set(Group::Code, Style::new().fg(Color::LightRed));
        scheme.set(Group::SearchMatch, Style::new().bg(Color::LightBlue));
        scheme.set(
            Group::SearchCurrent,
            Style::new()
                .bg(Color::LightYellow)
                .add_modifier(Modifier::REVERSED),
        );

        let row = |needle| -> u16 {
            layout
                .text()
                .lines
                .iter()
                .position(|line| line.to_string().contains(needle))
                .and_then(|row| u16::try_from(row).ok())
                .unwrap_or_else(|| panic!("missing line: {needle}"))
        };
        app.reflow(&layout, 24);
        press(
            &mut input,
            &mut app,
            &doc,
            40,
            "/find".chars().map(Char).chain([Enter]),
        );
        terminal.draw(|frame| draw(frame, &app, &layout, false, &scheme))?;
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(9, 23)].bg, Color::Green);
        assert_eq!(buffer[(0, 0)].fg, Color::Red);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(2, 0)].bg, Color::LightYellow);
        assert_eq!(buffer[(14, 0)].bg, Color::LightBlue);
        assert_eq!(buffer[(2, 0)].modifier, Modifier::BOLD | Modifier::REVERSED);
        assert_eq!(buffer[(0, row("link"))].fg, Color::Magenta);
        assert_eq!(buffer[(12, row("inline"))].fg, Color::LightRed);
        assert_eq!(buffer[(20, row("[img]"))].bg, Color::Reset);
        assert_eq!(buffer[(2, row("quote"))].modifier, Modifier::ITALIC);
        assert_eq!(buffer[(0, row("find code"))].bg, Color::LightBlue);
        assert_eq!(buffer[(5, row("find code"))].fg, Color::Reset);
        assert_eq!(buffer[(5, row("find code"))].bg, Color::Reset);
        assert_eq!(buffer[(0, row("graph TD"))].bg, Color::Reset);

        press(&mut input, &mut app, &doc, 40, [KeyCode::F(1)]);
        terminal.draw(|frame| draw(frame, &app, &layout, false, &scheme))?;
        let buffer = terminal.backend().buffer();
        assert_eq!(
            (buffer[(0, 0)].symbol(), buffer[(0, 0)].fg),
            ("H", Color::Red)
        );
        assert_eq!(buffer[(39, 23)].bg, Color::Green);
        Ok(())
    }

    #[test]
    fn colorscheme_patches_only_explicit_quote_attributes_on_gfm_alert_labels()
    -> Result<(), Infallible> {
        let source = "> [!NOTE]\n> body";
        let app = App::from_test_markdown("test.md".into(), source.into());
        let doc = prepare(&TuiMarkdown, source, 40);
        let layout = Layout::at_width(&doc, 40);
        let mut terminal = Terminal::new(TestBackend::new(40, 5))?;
        for (override_style, expected_fg, expected_modifier) in [
            (None, Color::Blue, Modifier::BOLD),
            (
                Some(Style::new().fg(Color::Red)),
                Color::Red,
                Modifier::BOLD,
            ),
            (
                Some(Style::new().remove_modifier(Modifier::BOLD)),
                Color::Blue,
                Modifier::empty(),
            ),
            (
                Some(Style::new().fg(Color::Red).remove_modifier(Modifier::BOLD)),
                Color::Red,
                Modifier::empty(),
            ),
        ] {
            let mut scheme = Colorscheme::default();
            if let Some(style) = override_style {
                scheme.set(Group::Quote, style);
            }
            terminal.draw(|frame| draw(frame, &app, &layout, false, &scheme))?;
            let buffer = terminal.backend().buffer();
            let x = (0..40)
                .find(|&x| buffer[(x, 0)].symbol() == "N")
                .unwrap_or_else(|| panic!("missing Note label"));
            assert_eq!(buffer[(x, 0)].fg, expected_fg, "{override_style:?}");
            assert_eq!(
                buffer[(x, 0)].modifier,
                expected_modifier,
                "{override_style:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn search_spans_wrapped_rows_near_end_and_resize_preserves_selection() -> Result<(), Infallible>
    {
        use KeyCode::{Char, Enter, Esc};

        let mut app =
            App::from_test_markdown("test.md".into(), "first\n\nabcdefgMATCH tail".into());
        let doc = prepare(&TuiMarkdown, app.document().markdown().unwrap().text(), 8);
        let mut terminal = Terminal::new(TestBackend::new(8, 5))?;
        let mut input = Input::default();
        paint(&mut terminal, &mut app, &doc)?;
        press(
            &mut input,
            &mut app,
            &doc,
            8,
            "/MATCH".chars().map(Char).chain([Enter]),
        );
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.top_row(), 1);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            (buffer[(7, 1)].symbol(), buffer[(7, 1)].bg),
            ("M", Color::Yellow)
        );
        assert_eq!(
            (buffer[(0, 2)].symbol(), buffer[(0, 2)].bg),
            ("A", Color::Yellow)
        );

        terminal.backend_mut().resize(32, 5);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.top_row(), 0);
        assert_eq!(app.search_selection(), Some((1, 1)));
        let buffer = terminal.backend().buffer();
        assert_eq!(
            (buffer[(8, 2)].symbol(), buffer[(8, 2)].bg),
            ("A", Color::Yellow)
        );
        let status: String = (0..32).map(|x| buffer[(x, 4)].symbol()).collect();
        assert!(
            status.contains("NORMAL") && status.contains("[1/1]") && status.contains("test.md")
        );

        press(
            &mut input,
            &mut app,
            &doc,
            32,
            ":noh".chars().map(Char).chain([Enter]),
        );
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(terminal.backend().buffer()[(8, 2)].bg, Color::Reset);
        press(&mut input, &mut app, &doc, 32, [Char('?'), Char('q'), Esc]);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(terminal.backend().buffer()[(8, 2)].bg, Color::Reset);
        assert_eq!(app.search_selection(), Some((1, 1)));
        press(&mut input, &mut app, &doc, 32, [Char('n')]);

        terminal.backend_mut().resize(8, 5);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.top_row(), 1);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(7, 1)].bg, Color::Yellow);
        assert_eq!(buffer[(0, 2)].bg, Color::Yellow);
        Ok(())
    }

    #[test]
    fn table_highlights_both_columns_across_cell_wraps() -> Result<(), Infallible> {
        let source = "| L | R |\n| --- | ---: |\n| **abcdef** | │XYZUV |\n| **abcdef** | │XYZUV |";
        let mut app = App::from_test_markdown("test.md".into(), source.into());
        let doc = prepare(&TuiMarkdown, app.document().markdown().unwrap().text(), 13);
        let mut terminal = Terminal::new(TestBackend::new(13, 10))?;
        let mut input = Input::default();
        paint(&mut terminal, &mut app, &doc)?;
        press(
            &mut input,
            &mut app,
            &doc,
            13,
            "/cdef │ │XY"
                .chars()
                .map(KeyCode::Char)
                .chain([KeyCode::Enter]),
        );
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.search_selection(), Some((1, 2)));
        let buffer = terminal.backend().buffer();
        for (x, y, glyph) in [(4, 3, "c"), (2, 4, "d"), (8, 3, "│"), (10, 3, "Y")] {
            assert_eq!(
                (buffer[(x, y)].symbol(), buffer[(x, y)].bg),
                (glyph, Color::Yellow)
            );
        }
        assert!(buffer[(2, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(
            (buffer[(6, 4)].symbol(), buffer[(6, 4)].bg),
            ("│", Color::Reset)
        );
        assert_eq!(buffer[(8, 5)].bg, Color::Blue);
        press(&mut input, &mut app, &doc, 13, [KeyCode::Char('N')]);
        assert_eq!(app.search_selection(), Some((2, 2)));
        terminal.backend_mut().resize(19, 10);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.search_selection(), Some((2, 2)));
        Ok(())
    }

    #[test]
    fn image_cells_scroll_clip_and_leave_caption_search_visible() -> Result<(), Infallible> {
        use crate::app::{Action, Motion};

        let source = "before same\n\n![same](red.png)\n\n![same](blue.jpg)\n\nafter";
        let mut app = App::from_test_markdown("test.md".into(), source.into());
        let resources = TuiMarkdown.resources(source);
        let doc = TuiMarkdown.render_document(source, 16, &resources);
        let solid = |rgb| Some(image::RgbImage::from_pixel(40, 80, image::Rgb(rgb)).into());
        let loaded = [solid([255, 0, 0]), solid([0, 0, 255])];
        let mut scheme = Colorscheme::default();
        scheme.set(
            Group::DocumentNormal,
            Style::new()
                .bg(Color::Green)
                .add_modifier(Modifier::REVERSED),
        );
        let mut terminal = Terminal::new(TestBackend::new(16, 5))?;
        let mode = crate::terminal::ImageMode::Kitty {
            cell_width: 10,
            cell_height: 20,
        };
        let paint = |terminal: &mut Terminal<TestBackend>, app: &mut App, float| {
            terminal
                .draw(|frame| {
                    let layout = Layout::with_images(&doc, frame.area().width, &loaded, mode);
                    app.reflow(
                        &layout,
                        usize::from(body_area(frame.area(), app.mode()).height),
                    );
                    draw(frame, app, &layout, float, &scheme);
                })
                .map(|_| ())
        };
        let layout = Layout::with_images(&doc, 16, &loaded, mode);
        let locations = &layout;
        let text = doc.search_text();
        assert_eq!(text, TuiMarkdown.render(source, 16).search_text());
        assert_eq!(layout.text().lines[6].to_string(), "same");
        assert_eq!(layout.text().lines[12].to_string(), "same");
        assert_eq!(
            Layout::at_width(&doc, 16).text().lines[2].to_string(),
            "[img] same"
        );

        paint(&mut terminal, &mut app, false)?;
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "b");
        assert!(
            terminal.backend().buffer()[(0, 2)]
                .symbol()
                .contains('\u{10eeee}')
        );
        assert!(
            !terminal.backend().buffer()[(0, 2)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(terminal.backend().buffer()[(15, 2)].bg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(0, 4)].symbol(), "N");
        for motion in [Motion::PageDown, Motion::LineDown] {
            app.apply(Action::Move(motion), locations, text);
        }
        paint(&mut terminal, &mut app, false)?;
        assert_eq!(app.top_row(), 5);
        assert!(
            terminal.backend().buffer()[(0, 0)]
                .symbol()
                .contains('\u{10eeee}')
        );
        assert_eq!(terminal.backend().buffer()[(0, 1)].symbol(), "s");
        assert!(
            terminal.backend().buffer()[(0, 3)]
                .symbol()
                .contains('\u{10eeee}')
        );
        paint(&mut terminal, &mut app, true)?;
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Green);
        app.apply(Action::Help, locations, text);
        paint(&mut terminal, &mut app, false)?;
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "H");
        app.apply(Action::Cancel, locations, text);
        app.apply(Action::StartSearch(Direction::Forward), locations, text);
        for ch in "same".chars() {
            app.apply(Action::Insert(ch), locations, text);
        }
        paint(&mut terminal, &mut app, false)?;
        assert_eq!(terminal.backend().buffer()[(0, 4)].symbol(), "/");
        assert_eq!(terminal.backend().buffer()[(0, 4)].bg, Color::Reset);
        app.apply(Action::Submit, locations, text);
        paint(&mut terminal, &mut app, false)?;
        assert_eq!(app.search_selection(), Some((2, 3)));
        assert_eq!(terminal.backend().buffer()[(0, 0)].bg, Color::Yellow);
        terminal.backend_mut().resize(2, 5);
        paint(&mut terminal, &mut app, false)?;
        assert_eq!(app.search_selection(), Some((2, 3)));
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "s");
        assert_eq!(terminal.backend().buffer()[(0, 1)].bg, Color::Yellow);
        Ok(())
    }

    #[test]
    fn kitty_layout_resize_preserves_caption_search() -> Result<(), Infallible> {
        use crate::{app::Action, terminal::ImageMode};

        let source = "above\n\n![label](fine.png)\n\nbelow\n\ntail";
        let mut app = App::from_test_markdown("test.md".into(), source.into());
        let resources = TuiMarkdown.resources(source);
        let doc = TuiMarkdown.render_document(source, 12, &resources);
        let loaded = [Some(image::DynamicImage::new_rgb8(80, 128))];
        let text = doc.search_text();
        let mut terminal = Terminal::new(TestBackend::new(12, 5))?;
        let mut mode = ImageMode::Kitty {
            cell_width: 8,
            cell_height: 16,
        };
        let initial = Layout::with_images(&doc, 12, &loaded, mode);
        let locations = &initial;
        app.reflow(locations, 4);
        app.apply(Action::StartSearch(Direction::Forward), locations, text);
        for ch in "label".chars() {
            app.apply(Action::Insert(ch), locations, text);
        }
        app.apply(Action::Submit, locations, text);
        let offset = text.find("label").unwrap_or_default();
        assert_eq!(initial.row_for_offset(offset), Some(10));
        for (width, height, caption_row) in [(12, 5, 10), (6, 4, 6)] {
            terminal.backend_mut().resize(width, height);
            if width == 6 {
                mode = ImageMode::Kitty {
                    cell_width: 10,
                    cell_height: 24,
                };
            }
            let layout = Layout::with_images(&doc, width, &loaded, mode);
            assert_eq!(layout.row_for_offset(offset), Some(caption_row));
            terminal.draw(|frame| {
                app.reflow(
                    &layout,
                    usize::from(body_area(frame.area(), app.mode()).height),
                );
                draw(frame, &app, &layout, false, &Colorscheme::default());
            })?;
            let buffer = terminal.backend().buffer();
            let y = caption_row.saturating_sub(app.top_row()) as u16;
            assert_eq!(
                (buffer[(0, y)].symbol(), buffer[(0, y)].bg),
                ("l", Color::Yellow)
            );
            assert!(!buffer[(0, height - 1)].symbol().contains('\u{10eeee}'));
            assert_eq!(app.search_selection(), Some((1, 1)));
        }
        Ok(())
    }

    #[test]
    fn help_is_an_overlay_closed_by_escape() -> Result<(), Infallible> {
        let mut app = App::from_test_markdown("test.md".into(), "body".into());
        let doc = prepare(&TuiMarkdown, app.document().markdown().unwrap().text(), 42);
        let mut terminal = Terminal::new(TestBackend::new(42, 10))?;
        let mut input = Input::default();
        press(&mut input, &mut app, &doc, 42, [KeyCode::F(1)]);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "H");
        press(&mut input, &mut app, &doc, 42, [KeyCode::Esc]);
        paint(&mut terminal, &mut app, &doc)?;
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "b");
        Ok(())
    }
}
