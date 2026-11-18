use std::ops::Range;

use ratatui::{
    buffer::CellWidth,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::markdown::RenderedMarkdown;

/// ひとつの書記素が占めるセルと、対応する検索用UTF-8バイト範囲。
#[derive(Debug, PartialEq, Eq)]
pub struct Cell {
    pub bytes: Range<usize>,
    pub columns: Range<usize>,
}

/// 画面上の一行。空行も閲覧アンカーを持つ。
#[derive(Debug)]
pub struct Row {
    pub anchor: usize,
    pub cells: Vec<Cell>,
}

impl Row {
    /// 一致範囲と重なる書記素を返す。部分一致でも書記素全体を強調できる。
    pub fn cells_for(&self, bytes: Range<usize>) -> impl Iterator<Item = &Cell> {
        let start = self
            .cells
            .partition_point(|cell| cell.bytes.end <= bytes.start);
        self.cells[start..]
            .iter()
            .take_while(move |cell| !bytes.is_empty() && cell.bytes.start < bytes.end)
    }
}

/// 折り返し済みの左揃え本文。描画時は `Paragraph` を再折り返しなしで使う。
pub struct Layout<'a> {
    text: Text<'a>,
    rows: Vec<Row>,
    search_len: usize,
}

impl<'a> Layout<'a> {
    #[must_use]
    pub fn new(document: &'a RenderedMarkdown) -> Self {
        Self::at_width(document, document.width())
    }

    #[must_use]
    pub fn at_width(document: &'a RenderedMarkdown, width: u16) -> Self {
        let original = document.text();
        let mut layout = Self {
            text: Text::default().style(original.style),
            rows: Vec::new(),
            search_len: document.search_text().len(),
        };
        if width != 0 {
            for (line, bytes) in original.lines.iter().zip(document.line_ranges()) {
                layout.wrap_line(line, document.search_text(), bytes, usize::from(width));
            }
        }
        layout
    }

    #[must_use]
    pub fn text(&self) -> &Text<'a> {
        &self.text
    }

    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// 検索開始バイトまたは閲覧アンカーを表示行へ変換する。
    /// 論理行末のLFと文書末尾は直前の表示行に属する。
    #[must_use]
    pub fn row_for_offset(&self, offset: usize) -> Option<usize> {
        if offset > self.search_len {
            return None;
        }
        self.rows
            .partition_point(|row| row.anchor <= offset)
            .checked_sub(1)
    }

    fn wrap_line(
        &mut self,
        source: &Line<'_>,
        search: &'a str,
        bytes: &Range<usize>,
        width: usize,
    ) {
        let fresh_line = || Line::default().style(source.style);
        let mut line = fresh_line();
        let mut row = Row {
            anchor: bytes.start,
            cells: Vec::new(),
        };
        let mut column = 0;
        let mut spans = source.spans.iter();
        let mut span_end = 0;
        let mut style = ratatui::style::Style::default();
        for (relative, grapheme) in search[bytes.clone()].grapheme_indices(true) {
            // 装飾の境界が書記素の途中なら、先頭のスタイルで一体として描く。
            while relative >= span_end {
                let Some(span) = spans.next() else { break };
                span_end += span.content.len();
                style = span.style;
            }
            let measured = usize::from(grapheme.cell_width());
            let (glyph, cells) = if measured == 0 || measured > width {
                ("�", 1)
            } else {
                (grapheme, measured)
            };
            let start = bytes.start + relative;
            if column + cells > width {
                self.text.lines.push(line);
                self.rows.push(row);
                line = fresh_line();
                row = Row {
                    anchor: start,
                    cells: Vec::new(),
                };
                column = 0;
            }
            if let Some(previous) = line.spans.last_mut().filter(|span| span.style == style) {
                previous.content.to_mut().push_str(glyph);
            } else {
                line.spans.push(Span::styled(glyph, style));
            }
            row.cells.push(Cell {
                bytes: start..start + grapheme.len(),
                columns: column..column + cells,
            });
            column += cells;
        }
        self.text.lines.push(line);
        self.rows.push(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{MarkdownRenderer, tui_markdown::TuiMarkdown};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
        widgets::{Paragraph, Widget},
    };

    const EXAMPLE: &str = include_str!("../../tests/fixtures/rendering.md");

    #[test]
    fn adapter_renders_literal_html_image_fallback_and_fenced_code() {
        let document = TuiMarkdown.render(EXAMPLE, 40);
        let text = document.search_text();
        assert!(text.contains("HTML: <em>文字</em>"));
        assert!(text.contains("<div>\n文字列\n</div>"));
        assert!(text.contains("[img] 代替テキスト"));
        assert!(!text.contains("example.png"));
        assert!(text.contains("```mermaid\ngraph TD; A --> B\n```"));
        assert!(document.text().lines.iter().any(|line| {
            line.to_string() == "let long_identifier = \"long code line\";"
                && line
                    .spans
                    .iter()
                    .any(|span| matches!(span.style.fg, Some(Color::Rgb(..))))
                && line
                    .spans
                    .windows(2)
                    .any(|pair| pair[0].style != pair[1].style)
        }));
    }

    #[test]
    fn japanese_match_crosses_style_and_wrap_boundaries_and_anchor_survives_resize() {
        let document = TuiMarkdown.render(EXAMPLE, 6);
        let narrow = Layout::new(&document);
        let wide = Layout::at_width(&document, 40);
        assert_eq!(document.search_text().find("本語強調"), Some(3));
        assert_eq!(narrow.text().lines[0].to_string(), "日本語");
        assert_eq!(narrow.text().lines[1].to_string(), "強調あ");
        assert_eq!(wide.text().lines[0].to_string(), "日本語強調あと");
        let area = Rect::new(0, 0, 6, 2);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(narrow.text().clone()).render(area, &mut buffer);
        assert_eq!(buffer[(2, 0)].symbol(), "本");
        assert!(!buffer[(2, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(4, 0)].symbol(), "語");
        assert!(buffer[(4, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 1)].symbol(), "強");
        assert!(buffer[(0, 1)].modifier.contains(Modifier::BOLD));
        let range = 3..15;
        let columns = |layout: &Layout<'_>, row: usize| {
            layout.rows()[row]
                .cells_for(range.clone())
                .map(|cell| cell.columns.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(columns(&narrow, 0), [2..4, 4..6]);
        assert_eq!(columns(&narrow, 1), [0..2, 2..4]);
        assert_eq!(columns(&wide, 0), [2..4, 4..6, 6..8, 8..10]);
        let anchor = narrow.rows()[1].anchor;
        assert_eq!(anchor, 9);
        assert_eq!(wide.row_for_offset(anchor), Some(0));
        assert_eq!(narrow.row_for_offset(anchor), Some(1));
    }

    #[test]
    fn table_and_long_code_keep_matches_at_each_width() -> Result<(), &'static str> {
        let document = TuiMarkdown.render(EXAMPLE, 6);
        let rerendered = TuiMarkdown.render(EXAMPLE, 60);
        let narrow = Layout::new(&document);
        let wide = Layout::new(&rerendered);
        assert_eq!(document.search_text(), rerendered.search_text());
        assert_eq!(document.line_ranges(), rerendered.line_ranges());
        for phrase in ["表の長い項目", "long_identifier"] {
            let start = document
                .search_text()
                .find(phrase)
                .ok_or("search phrase missing")?;
            let end = start + phrase.len();
            let first = narrow.row_for_offset(start).ok_or("missing narrow row")?;
            let last = narrow
                .row_for_offset(end - 1)
                .ok_or("missing narrow continuation")?;
            let wide_row = wide.row_for_offset(start).ok_or("missing wide row")?;
            assert!(first < last);
            assert_eq!(wide.row_for_offset(end - 1), Some(wide_row));
            assert!(wide.text().lines[wide_row].to_string().contains(phrase));
            let visible = narrow.rows()[first..=last]
                .iter()
                .flat_map(|row| row.cells_for(start..end))
                .map(|cell| &document.search_text()[cell.bytes.clone()])
                .collect::<String>();
            assert_eq!(visible, phrase);
            assert_eq!(
                wide.row_for_offset(narrow.rows()[last].anchor),
                Some(wide_row)
            );
        }
        assert!(narrow.text().lines.iter().all(|line| line.width() <= 6));
        Ok(())
    }

    #[test]
    fn halfwidth_kana_uses_the_same_cell_width_as_the_paragraph() {
        let area = Rect::new(0, 0, 4, 1);
        let document = TuiMarkdown.render("ｶﾞA", area.width);
        let layout = Layout::new(&document);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(layout.text().clone()).render(area, &mut buffer);
        // 実端末の幅ではなく Ratatui のバッファ上の位置との一致を保証する。
        assert_eq!(layout.rows()[0].cells[0].columns, 0..3);
        assert_eq!(layout.rows()[0].cells[1].columns, 3..4);
        assert_eq!(buffer[(0, 0)].symbol(), "ｶﾞ");
        assert_eq!(buffer[(3, 0)].symbol(), "A");
    }

    #[test]
    fn tiny_width_and_line_ends_keep_defined_positions() {
        let document = TuiMarkdown.render("日\n\nA", 1);
        let layout = Layout::new(&document);
        assert_eq!(document.search_text(), "日\n\nA");
        assert_eq!(layout.text().to_string(), "�\n\nA");
        assert_eq!(layout.rows()[0].cells[0].bytes, 0..3);
        assert_eq!(layout.row_for_offset(3), Some(0));
        assert_eq!(layout.row_for_offset(4), Some(1));
        assert_eq!(layout.row_for_offset(6), Some(2));
        assert_eq!(layout.row_for_offset(7), None);
        assert_eq!(Layout::at_width(&document, 0).row_for_offset(0), None);
    }
}
