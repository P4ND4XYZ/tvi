mod table;

use std::ops::Range;

use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;

use super::images::PlacedImage;
use crate::{
    app::{ViewAnchor, ViewRows},
    markdown::RenderedMarkdown,
    terminal::ImageMode,
};

/// ひとつの書記素が占めるセルと、対応する検索用UTF-8バイト範囲。
#[derive(Debug, PartialEq, Eq)]
pub struct Cell {
    pub bytes: Range<usize>,
    pub columns: Range<usize>,
}

/// 画面上の一行。空行も閲覧アンカーを持つ。
#[derive(Debug)]
pub struct Row {
    pub anchor: ViewAnchor,
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
    #[cfg(test)]
    search_len: usize,
    reflowed: Vec<(Range<usize>, Range<usize>)>,
    images: Vec<PlacedImage>,
}

impl<'a> Layout<'a> {
    #[cfg(test)]
    #[must_use]
    pub fn new(document: &'a RenderedMarkdown) -> Self {
        Self::at_width(document, document.width())
    }

    #[cfg(test)]
    #[must_use]
    pub fn at_width(document: &'a RenderedMarkdown, width: u16) -> Self {
        Self::with_images(document, width, &[], ImageMode::Text)
    }

    pub fn with_images(
        document: &'a RenderedMarkdown,
        width: u16,
        images: &[Option<image::DynamicImage>],
        mode: ImageMode,
    ) -> Self {
        let original = document.text();
        let mut layout = Self {
            text: Text::default().style(original.style),
            rows: Vec::new(),
            #[cfg(test)]
            search_len: document.search_text().len(),
            reflowed: Vec::new(),
            images: Vec::new(),
        };
        if width != 0 {
            let mut placements = document.images.iter().peekable();
            let mut tables = document.tables.iter().peekable();
            let mut table_end = 0;
            for (index, (line, bytes)) in original
                .lines
                .iter()
                .zip(document.line_ranges())
                .enumerate()
            {
                if index < table_end {
                    continue;
                }
                if let Some(table) = tables.next_if(|table| table.start == index) {
                    let end = index + table.lines.len();
                    if table::wrap(
                        &mut layout,
                        table,
                        &original.lines[index..end],
                        document.search_text(),
                        &document.line_ranges()[index..end],
                        usize::from(width),
                    )
                    .is_some()
                    {
                        table_end = end;
                        continue;
                    }
                }
                let placement = placements.next_if(|placement| placement.line == index);
                let mut skip = 0;
                if let Some(placement) = placement
                    && let Some(Some(image)) = images.get(placement.image)
                    && let Some(image) = PlacedImage::new(image, width, layout.rows.len(), mode)
                {
                    for row in 0..image.height() {
                        layout.text.lines.push(Line::default());
                        layout.rows.push(Row {
                            anchor: ViewAnchor {
                                byte: bytes.start,
                                image_row: row,
                            },
                            cells: Vec::new(),
                        });
                    }
                    layout.images.push(image);
                    if !placement.has_alt {
                        continue;
                    }
                    skip = placement.prefix_bytes;
                }
                layout.wrap_line(
                    line,
                    document.search_text(),
                    bytes,
                    usize::from(width),
                    skip,
                );
            }
        }
        layout
    }

    pub fn transmitted_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.images.iter().filter_map(PlacedImage::transmitted_id)
    }

    pub fn draw_images(&self, frame: &mut Frame<'_>, body: Rect, top: usize) {
        let first = self.images.partition_point(|image| image.rows.end <= top);
        for image in self.images[first..]
            .iter()
            .take_while(|image| image.rows.start < top + usize::from(body.height))
        {
            image.draw(frame, body, top);
        }
    }

    #[must_use]
    pub fn text(&self) -> &Text<'a> {
        &self.text
    }

    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[cfg(test)]
    pub fn row_for_offset(&self, offset: usize) -> Option<usize> {
        (offset <= self.search_len && !self.rows.is_empty())
            .then(|| self.row_at(ViewAnchor::text(offset)))
    }

    fn table_row_at(&self, offset: usize) -> Option<usize> {
        let table_line = self
            .reflowed
            .partition_point(|(bytes, _)| bytes.end < offset);
        if let Some((bytes, rows)) = self.reflowed.get(table_line)
            && bytes.start <= offset
        {
            if offset == bytes.end {
                return Some(rows.end - 1);
            }
            return self.rows[rows.clone()]
                .iter()
                .position(|row| row.cells_for(offset..offset + 1).next().is_some())
                .map(|row| rows.start + row)
                .or(Some(rows.start));
        }
        None
    }

    fn wrap_line(
        &mut self,
        source: &Line<'_>,
        search: &'a str,
        bytes: &Range<usize>,
        width: usize,
        skip: usize,
    ) {
        let fresh_line = || Line::default().style(source.style);
        let mut line = fresh_line();
        let mut row = Row {
            anchor: ViewAnchor::text(bytes.start + skip),
            cells: Vec::new(),
        };
        let mut column = 0;
        for Glyph {
            start,
            end,
            symbol,
            width: measured,
            style,
        } in glyphs(&source.spans, &search[bytes.clone()], bytes.start)
            .filter(|g| g.start >= bytes.start + skip)
        {
            let (glyph, cells) = if measured > width {
                ("�", 1)
            } else {
                (symbol, measured)
            };
            if column + cells > width {
                self.text.lines.push(line);
                self.rows.push(row);
                line = fresh_line();
                row = Row {
                    anchor: ViewAnchor::text(start),
                    cells: Vec::new(),
                };
                column = 0;
            }
            push_glyph(&mut line, glyph, style);
            row.cells.push(Cell {
                bytes: start..end,
                columns: column..column + cells,
            });
            column += cells;
        }
        self.text.lines.push(line);
        self.rows.push(row);
    }
}

impl ViewRows for Layout<'_> {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn anchor_at(&self, row: usize) -> Option<ViewAnchor> {
        self.rows.get(row).map(|row| row.anchor)
    }

    fn row_at(&self, position: ViewAnchor) -> usize {
        // 表は、先行列の続きより後続列の先頭が上に来ることがある。
        self.table_row_at(position.byte)
            .unwrap_or_else(|| self.row_at_ordered(position))
    }
}

struct Glyph<'a> {
    symbol: &'a str,
    width: usize,
    start: usize,
    end: usize,
    style: Style,
}

fn glyphs<'a, 's, 'span>(
    spans: &'s [Span<'span>],
    text: &'a str,
    start: usize,
) -> impl Iterator<Item = Glyph<'a>> + use<'a, 's, 'span> {
    let mut spans = spans.iter();
    let mut span_end = 0;
    let mut style = Style::default();
    text.grapheme_indices(true)
        .map(move |(relative, grapheme)| {
            // 装飾の境界が書記素の途中なら、先頭のスタイルで一体として描く。
            while relative >= span_end {
                let Some(span) = spans.next() else { break };
                span_end += span.content.len();
                style = span.style;
            }
            let measured = usize::from(grapheme.cell_width());
            Glyph {
                symbol: if measured == 0 { "�" } else { grapheme },
                width: measured.max(1),
                start: start + relative,
                end: start + relative + grapheme.len(),
                style,
            }
        })
}

fn push_glyph<'a>(line: &mut Line<'a>, symbol: &'a str, style: Style) {
    if let Some(previous) = line.spans.last_mut().filter(|span| span.style == style) {
        previous.content.to_mut().push_str(symbol);
    } else {
        line.spans.push(Span::styled(symbol, style));
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
        let anchor = narrow.rows()[1].anchor.byte;
        assert_eq!(anchor, 9);
        assert_eq!(wide.row_for_offset(anchor), Some(0));
        assert_eq!(narrow.row_for_offset(anchor), Some(1));
    }

    #[test]
    fn commands_table_keeps_its_borders_aligned_at_48_cells() -> Result<(), &'static str> {
        let document = TuiMarkdown.render(include_str!("../../docs/features/commands.md"), 48);
        let wide = Layout::at_width(&document, 49);
        let narrow = Layout::new(&document);
        let table_start = |layout: &Layout<'_>| {
            layout
                .text()
                .lines
                .iter()
                .position(|line| line.to_string().starts_with('┌'))
        };
        let wide_start = table_start(&wide).ok_or("wide table missing")?;
        let narrow_start = table_start(&narrow).ok_or("narrow table missing")?;
        let natural_start = document
            .text()
            .lines
            .iter()
            .position(|line| line.to_string().starts_with('┌'))
            .ok_or("natural table missing")?;
        let strings = |lines: &[Line<'_>]| lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(
            strings(&wide.text().lines[wide_start..wide_start + 7]),
            strings(&document.text().lines[natural_start..natural_start + 7])
        );
        assert_eq!(wide.text().lines[wide_start].width(), 49);

        let area = Rect::new(0, 0, 48, 8);
        let mut buffer = Buffer::empty(area);
        let table: Text<'_> = narrow.text().lines[narrow_start..narrow_start + 8]
            .iter()
            .cloned()
            .collect();
        assert!(table.lines.iter().all(|line| line.width() == 48));
        Paragraph::new(table).render(area, &mut buffer);
        for (y, borders) in ["┌┬┐", "│││", "├┼┤", "│││", "│││", "│││", "│││", "└┴┘"]
            .into_iter()
            .enumerate()
        {
            for (x, border) in [0, 21, 47].into_iter().zip(borders.chars()) {
                assert_eq!(buffer[(x, y as u16)].symbol(), border.to_string());
            }
        }
        assert_eq!(buffer[(2, 5)].symbol(), ":");
        assert_eq!(buffer[(23, 5)].symbol(), "検");
        assert_eq!(buffer[(43, 5)].symbol(), "効");
        assert_eq!(buffer[(23, 6)].symbol(), "化");
        assert_eq!(buffer[(2, 6)].symbol(), " ");
        let continuation = document
            .search_text()
            .find("化 │")
            .ok_or("missing continuation")?;
        assert_eq!(narrow.row_for_offset(continuation), Some(narrow_start + 6));
        assert_eq!(
            wide.row_for_offset(narrow.rows()[narrow_start + 6].anchor.byte),
            Some(wide_start + 5)
        );
        Ok(())
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
                wide.row_for_offset(narrow.rows()[last].anchor.byte),
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
