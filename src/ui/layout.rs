pub(crate) mod pdf_marks;
mod table;

use std::ops::Range;

use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::{Rect, Size},
    style::Style,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;

use super::images::PlacedImage;
use crate::{
    app::{ViewAnchor, ViewRows},
    markdown::{RenderedMarkdown, SourcePosition},
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
    source_positions: &'a [Option<SourcePosition>],
    logical_rows: Vec<usize>,
    images: Vec<PlacedImage>,
    pdf_raster_rows: Option<f64>,
}

// Four new tiles per frame: at most 128 KiB raw RGBA (under 180 KiB of
// base64 plus Kitty transfer framing), independently of viewport size.
const PDF_TILE_PIXELS: u32 = 8 * 1024;
const PDF_TILES_PER_FRAME: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PdfGeometry {
    pub columns: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Layout<'static> {
    pub fn empty_pdf() -> Self {
        Self {
            text: Text::default(),
            rows: Vec::new(),
            #[cfg(test)]
            search_len: 0,
            reflowed: Vec::new(),
            source_positions: &[],
            logical_rows: Vec::new(),
            images: Vec::new(),
            pdf_raster_rows: Some(0.0),
        }
    }

    pub fn pdf_page(
        image: &image::DynamicImage,
        aspect_height: f64,
        geometry: PdfGeometry,
        decorate: impl FnOnce(&mut image::RgbaImage),
    ) -> Option<Self> {
        // Kitty placeholders have a finite coordinate table. Tile the width-fitted
        // raster rather than reducing its height to fit a single placement.
        const MAX_DISPLAY_PIXELS: u64 = 32 * 1024 * 1024;
        const MAX_ROWS: u16 = 8192;
        const ANCHOR_SCALE: usize = 1 << 18;

        let cw = u32::from(geometry.cell_width);
        let ch = u32::from(geometry.cell_height);
        let cells_per_tile = PDF_TILE_PIXELS.checked_div(cw * ch)?;
        // Keep bands shallow to limit bytes outside the viewport. Each dimension
        // also stays below the Kitty placeholder coordinate limit.
        let tile_columns = u16::try_from(cells_per_tile.min(128))
            .ok()?
            .min(geometry.columns);
        let tile_rows =
            u16::try_from(cells_per_tile.checked_div(u32::from(tile_columns))?.min(8)).ok()?;
        let width = u32::from(geometry.columns) * cw;
        let height = (f64::from(width) * aspect_height).round().max(1.0);
        if width == 0
            || ch == 0
            || !height.is_finite()
            || height > f64::from(MAX_ROWS) * f64::from(ch)
        {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "checked against MAX_ROWS times a u16 cell size"
        )]
        let height = height as u32;
        let rows = u16::try_from(height.div_ceil(ch)).ok()?;
        if u64::from(width) * u64::from(rows) * u64::from(ch) > MAX_DISPLAY_PIXELS {
            return None;
        }
        let mut pixels = image
            .resize_exact(width, height, image::imageops::FilterType::Triangle)
            .into_rgba8();
        decorate(&mut pixels);
        let mut layout = Self::empty_pdf();
        layout.pdf_raster_rows = Some(f64::from(height) / f64::from(ch));
        for row in 0..rows {
            let byte = (usize::from(row) * ANCHOR_SCALE).div_ceil(usize::from(rows));
            layout.rows.push(Row {
                anchor: ViewAnchor::text(byte),
                cells: Vec::new(),
            });
        }
        for start in (0..rows).step_by(usize::from(tile_rows)) {
            let tile_rows = tile_rows.min(rows - start);
            let y = u32::from(start) * ch;
            let tile_height = u32::from(tile_rows) * ch;
            for column in (0..geometry.columns).step_by(usize::from(tile_columns)) {
                let tile_columns = tile_columns.min(geometry.columns - column);
                let x = u32::from(column) * cw;
                let tile_width = u32::from(tile_columns) * cw;
                let mut tile = image::RgbaImage::new(tile_width, tile_height);
                let visible = image::imageops::crop_imm(
                    &pixels,
                    x,
                    y,
                    tile_width,
                    tile_height.min(height - y),
                );
                image::imageops::replace(&mut tile, &visible.to_image(), 0, 0);
                layout.images.push(PlacedImage::pdf_tile(
                    tile,
                    Size::new(tile_columns, tile_rows),
                    usize::from(start),
                    column,
                )?);
            }
        }
        Some(layout)
    }
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
            source_positions: &document.source_positions,
            logical_rows: Vec::with_capacity(original.lines.len() + 1),
            images: Vec::new(),
            pdf_raster_rows: None,
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
                        let reflowed = layout.reflowed.len() - table.lines.len();
                        layout.logical_rows.extend(
                            layout.reflowed[reflowed..]
                                .iter()
                                .map(|(_, rows)| rows.start),
                        );
                        table_end = end;
                        continue;
                    }
                }
                layout.logical_rows.push(layout.rows.len());
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
        layout.logical_rows.push(layout.rows.len());
        layout
    }

    pub(crate) fn row_for_source_line(&self, line: usize) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let last = self.source_positions.len().checked_sub(1)?;
        let line = line.min(last);
        if let Some(position) = self.source_positions[line..].iter().flatten().next() {
            let start = self.logical_rows[position.line];
            let end = self.logical_rows[position.line + 1];
            if self.rows[start].anchor.byte == position.byte {
                return Some(start);
            }
            return Some(
                self.rows[start..end]
                    .iter()
                    .position(|row| {
                        row.cells_for(position.byte..position.byte + 1)
                            .next()
                            .is_some()
                    })
                    .map_or(start, |row| start + row),
            );
        }
        self.source_positions[..line]
            .iter()
            .rev()
            .flatten()
            .next()
            .map(|position| self.logical_rows[position.line + 1] - 1)
    }

    pub fn transmitted_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.images.iter().filter_map(PlacedImage::transmitted_id)
    }

    fn visible_images(&self, body: Rect, top: usize) -> impl Iterator<Item = &PlacedImage> {
        let first = self.images.partition_point(|image| image.rows.end <= top);
        self.images[first..]
            .iter()
            .take_while(move |image| {
                !body.is_empty() && image.rows.start < top + usize::from(body.height)
            })
            .filter(move |image| image.visible_at_width(body.width))
    }

    pub(crate) fn pdf_row(&self, from_top: f64) -> usize {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "hit fractions lie within the raster's finite row count"
        )]
        let row = (from_top * self.pdf_raster_rows.unwrap_or_default()).floor() as usize;
        row.min(self.rows.len().saturating_sub(1))
    }

    pub(crate) fn pdf_row_boundary(&self, row: usize) -> Option<f64> {
        let raster = self.pdf_raster_rows.filter(|&rows| rows > 0.0)?;
        Some(f64::from(u16::try_from(row).ok()?) / raster)
    }

    pub fn pdf_transfer_pending(&self, body: Rect, top: usize) -> bool {
        self.pdf_raster_rows.is_some()
            && self
                .visible_images(body, top)
                .any(|image| image.transmitted_id().is_none())
    }

    pub fn draw_images(&self, frame: &mut Frame<'_>, body: Rect, top: usize) {
        let mut new_tiles = 0;
        for image in self.visible_images(body, top) {
            if self.pdf_raster_rows.is_some() && image.transmitted_id().is_none() {
                if new_tiles == PDF_TILES_PER_FRAME {
                    continue;
                }
                new_tiles += 1;
            }
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
    fn pdf_kitty_placement_spans_body_width_and_keeps_the_last_tile_rows()
    -> Result<(), Box<dyn std::error::Error>> {
        use ratatui::{Terminal, backend::TestBackend};

        let pixels = image::DynamicImage::new_rgb8(13, 26);
        let geometry = PdfGeometry {
            columns: 162,
            cell_width: 2,
            cell_height: 4,
        };
        let layout =
            Layout::pdf_page(&pixels, 2.0, geometry, |_| {}).ok_or("page was not placed")?;
        assert_eq!(layout.len(), 162);
        let mut terminal = Terminal::new(TestBackend::new(164, 4))?;
        terminal.draw(|frame| layout.draw_images(frame, Rect::new(1, 0, 162, 3), 159))?;
        let buffer = terminal.backend().buffer();
        for y in 0..3 {
            for x in [1, 128, 129, 162] {
                assert!(
                    buffer[(x, y)].symbol().contains('\u{10eeee}'),
                    "no page pixel at {x},{y}"
                );
            }
            assert_eq!(buffer[(0, y)].symbol(), " ");
            assert_eq!(buffer[(163, y)].symbol(), " ");
        }
        assert_eq!(buffer[(1, 3)].symbol(), " ");
        assert_eq!(layout.transmitted_ids().count(), 4);
        Ok(())
    }

    #[test]
    fn desktop_pdf_transfers_only_a_bounded_batch_of_visible_tiles_each_frame()
    -> Result<(), Box<dyn std::error::Error>> {
        use ratatui::{Terminal, backend::TestBackend};

        let geometry = PdfGeometry {
            columns: 128,
            cell_width: 10,
            cell_height: 20,
        };
        let image = image::DynamicImage::new_rgb8(40, 160);
        let layout =
            Layout::pdf_page(&image, 4.0, geometry, |_| {}).ok_or("page was not placed")?;
        let body = Rect::new(0, 0, 128, 47);
        let mut screen = Terminal::new(TestBackend::new(128, 48))?;
        for top in [0, 48] {
            let mut frames = 0;
            while layout.pdf_transfer_pending(body, top) {
                screen.draw(|frame| layout.draw_images(frame, body, top))?;
                let bytes: usize = screen
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .filter(|symbol| symbol.contains("\x1b_G"))
                    .map(str::len)
                    .sum();
                assert!(
                    (1..180 * 1024).contains(&bytes),
                    "one frame wrote {bytes} Kitty bytes"
                );
                frames += 1;
                assert!(frames <= 60, "visible tile transfer never finished");
            }
            assert!(
                frames > 1,
                "a large viewport should transfer in multiple batches"
            );
            assert!(
                screen.backend().buffer()[(127, 46)]
                    .symbol()
                    .contains('\u{10eeee}')
            );
            screen.draw(|frame| layout.draw_images(frame, body, top))?;
            assert!(
                screen
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| !cell.symbol().contains("\x1b_G"))
            );
        }
        Ok(())
    }

    #[test]
    fn source_lines_follow_content_through_wraps_and_hidden_lines() {
        let source = "abcdefgh ijkl\nTARGET overlong\n\n[hidden]: /x\n\n# End\n\n[tail]: /y\n";
        let document = TuiMarkdown.render(source, 8);
        let narrow = Layout::new(&document);
        let wide = Layout::at_width(&document, 16);
        assert_eq!(narrow.row_for_source_line(0), Some(0));
        assert_eq!(narrow.row_for_source_line(1), Some(1));
        assert_eq!(wide.row_for_source_line(1), Some(0));
        assert_eq!(narrow.text().lines[1].to_string(), " ijkl TA");
        assert_eq!(narrow.row_for_source_line(3), Some(5));
        assert_eq!(narrow.text().lines[5].to_string(), "# End");
        assert_eq!(narrow.row_for_source_line(usize::MAX), Some(5));
    }

    #[test]
    fn repeating_inline_code_is_located_on_its_own_physical_line() {
        let document = TuiMarkdown.render("`again again\r\nagain`\n", 6);
        let layout = Layout::new(&document);
        assert_eq!(layout.text().lines[2].to_string().trim_start(), "again");
        assert_eq!(layout.row_for_source_line(1), Some(2));
    }

    #[test]
    fn source_line_can_target_a_decoded_multiline_link_destination() {
        let document = TuiMarkdown.render("0123456789 [label](\nfoo&amp;bar\n)\n", 6);
        let layout = Layout::new(&document);
        assert_eq!(layout.text().lines[3].to_string(), "foo&ba");
        assert_eq!(layout.row_for_source_line(1), Some(3));
    }

    #[test]
    fn source_lines_do_not_count_a_final_newline_as_a_new_line() {
        for source in ["alphabet soup", "alphabet soup\n", "alphabet soup\r\n"] {
            let document = TuiMarkdown.render(source, 5);
            let layout = Layout::new(&document);
            assert_eq!(layout.text().lines.len(), 3);
            assert_eq!(layout.row_for_source_line(usize::MAX), Some(0));
        }
        for source in ["", "\r\n", "[hidden]: /url\n"] {
            let document = TuiMarkdown.render(source, 5);
            assert_eq!(Layout::new(&document).row_for_source_line(0), None);
        }
    }

    #[test]
    fn source_lines_distinguish_mermaid_image_from_code() {
        let source = "Before\n\n```mermaid\r\nflowchart TD\r\n A --> B\r\n```\n\nAfter\n";
        let resources = TuiMarkdown.resources(source);
        let diagram = TuiMarkdown.render_with_diagrams(source, 40, &resources, &[(0, Some(0))]);
        let images = [Some(image::DynamicImage::new_rgb8(10, 60))];
        let layout = Layout::with_images(&diagram, 40, &images, ImageMode::Halfblocks);
        assert_eq!(layout.images.len(), 1);
        assert_eq!(layout.images[0].rows, 2..5);
        for line in 2..=5 {
            assert_eq!(layout.row_for_source_line(line), Some(2));
        }
        let code = TuiMarkdown.render_with_diagrams(source, 40, &resources, &[(0, None)]);
        let layout = Layout::new(&code);
        for line in 2..=5 {
            assert_eq!(layout.row_for_source_line(line), Some(line));
        }
        assert_eq!(layout.text().lines[4].to_string(), " A --> B");
    }

    #[test]
    fn source_lines_target_table_rows_and_keep_caption_search_offsets() {
        let source =
            "| Header | B |\n| --- | --- |\n| Long first cell | C |\n\n![caption](photo.png)\n";
        let document = TuiMarkdown.render_document(source, 12, &TuiMarkdown.resources(source));
        let images = [Some(image::DynamicImage::new_rgb8(10, 60))];
        let layout = Layout::with_images(&document, 12, &images, ImageMode::Halfblocks);
        let header = layout.row_for_source_line(0).unwrap();
        let body = layout.row_for_source_line(2).unwrap();
        assert_eq!(header, 1);
        assert!(layout.text().lines[body].to_string().contains("Long"));
        assert_eq!(layout.row_for_source_line(1), Some(body));
        let image = layout.row_for_source_line(4).unwrap();
        assert_eq!(image, layout.images[0].rows.start);
        let caption = document.search_text().find("caption").unwrap();
        assert_eq!(layout.row_for_offset(caption), Some(image + 3));
    }

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
