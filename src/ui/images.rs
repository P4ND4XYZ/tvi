use std::{cell::Cell, ops::Range};

use crate::terminal::ImageMode;
use image::DynamicImage;
use ratatui::{
    Frame,
    layout::{Rect, Size},
    style::Style,
};

use ratatui_image::{
    protocol::kitty::Kitty,
    sliced::{SlicedImage, SlicedProtocol},
};

// Kitty's protocol-defined column indices for a PDF tile; the first cell of
// each row carries its column explicitly, and the terminal infers the rest.
// https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
pub(super) const PDF_TILE_COLUMNS: u32 = 128;
const KITTY_COLUMNS: [char; PDF_TILE_COLUMNS as usize] = [
    '\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}',
    '\u{346}', '\u{34a}', '\u{34b}', '\u{34c}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35b}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36a}', '\u{36b}', '\u{36c}', '\u{36d}', '\u{36e}', '\u{36f}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59c}', '\u{59d}', '\u{59e}', '\u{59f}', '\u{5a0}', '\u{5a1}',
    '\u{5a8}', '\u{5a9}', '\u{5ab}', '\u{5ac}', '\u{5af}', '\u{5c4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
    '\u{659}', '\u{65a}', '\u{65b}', '\u{65d}', '\u{65e}', '\u{6d6}', '\u{6d7}', '\u{6d8}',
    '\u{6d9}', '\u{6da}', '\u{6db}', '\u{6dc}', '\u{6df}', '\u{6e0}', '\u{6e1}', '\u{6e2}',
    '\u{6e4}', '\u{6e7}', '\u{6e8}', '\u{6eb}', '\u{6ec}', '\u{730}', '\u{732}', '\u{733}',
    '\u{735}', '\u{736}', '\u{73a}', '\u{73d}', '\u{73f}', '\u{740}', '\u{741}', '\u{743}',
    '\u{745}', '\u{747}', '\u{749}', '\u{74a}', '\u{7eb}', '\u{7ec}', '\u{7ed}', '\u{7ee}',
    '\u{7ef}', '\u{7f0}', '\u{7f1}', '\u{7f3}', '\u{816}', '\u{817}', '\u{818}', '\u{819}',
    '\u{81b}', '\u{81c}', '\u{81d}', '\u{81e}', '\u{81f}', '\u{820}', '\u{821}', '\u{822}',
    '\u{823}', '\u{825}', '\u{826}', '\u{827}', '\u{829}', '\u{82a}', '\u{82b}', '\u{82c}',
];
const PLACEHOLDER: char = '\u{10eeee}';

pub struct PlacedImage {
    pub rows: Range<usize>,
    column: u16,
    protocol: SlicedProtocol,
    kitty_id: u32,
    drawn: Cell<bool>,
}

impl PlacedImage {
    pub fn new(image: &DynamicImage, columns: u16, start: usize, mode: ImageMode) -> Option<Self> {
        let ImageMode::Kitty {
            cell_width,
            cell_height,
        } = mode
        else {
            return None;
        };
        let kitty_id = crate::terminal::next_image_id();
        let protocol = Self::kitty(image, columns, cell_width, cell_height, kitty_id)?;
        Some(Self {
            rows: start..start + usize::from(protocol.size().height),
            column: 0,
            protocol,
            kitty_id,
            drawn: Cell::new(false),
        })
    }

    pub(super) fn pdf_tile(
        pixels: image::RgbaImage,
        size: Size,
        start: usize,
        column: u16,
    ) -> Option<Self> {
        let id = crate::terminal::next_image_id();
        let protocol = SlicedProtocol::Kitty(
            Kitty::new(DynamicImage::ImageRgba8(pixels), size, id, false, false).ok()?,
        );
        Some(Self {
            rows: start..start + usize::from(size.height),
            column,
            protocol,
            kitty_id: id,
            drawn: Cell::new(false),
        })
    }

    fn kitty(
        image: &DynamicImage,
        columns: u16,
        cw: u16,
        ch: u16,
        id: u32,
    ) -> Option<SlicedProtocol> {
        let (cw, ch) = (u32::from(cw), u32::from(ch));
        if cw == 0 || ch == 0 || columns == 0 {
            return None;
        }
        // ダイアクリティカルマークで参照するセル数と、画像の高さを制限する。
        let max_width = u32::from(columns.min(160)) * cw;
        let max_height = (160 * ch).min(3200);
        let resized;
        let image = if image.width() > max_width || image.height() > max_height {
            resized = image.resize(max_width, max_height, image::imageops::FilterType::Triangle);
            &resized
        } else {
            image
        };
        // 実セルの整数倍まで透明で埋め、端数セルで縦横比が変わらないようにする。
        let width = u16::try_from(image.width().div_ceil(cw)).ok()?;
        let height = u16::try_from(image.height().div_ceil(ch)).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        let mut pixels = image::RgbaImage::new(u32::from(width) * cw, u32::from(height) * ch);
        image::imageops::replace(&mut pixels, &image.to_rgba8(), 0, 0);
        let kitty = Kitty::new(
            DynamicImage::ImageRgba8(pixels),
            Size::new(width, height),
            id,
            false,
            false,
        )
        .ok()?;
        Some(SlicedProtocol::Kitty(kitty))
    }

    pub(super) fn visible_at_columns(&self, left: u16, width: u16) -> bool {
        let end = u32::from(self.column) + u32::from(self.protocol.size().width);
        end > u32::from(left) && u32::from(self.column) < u32::from(left) + u32::from(width)
    }

    pub fn transmitted_id(&self) -> Option<u32> {
        self.drawn.get().then_some(self.kitty_id)
    }

    pub fn height(&self) -> u16 {
        self.protocol.size().height
    }

    pub fn draw(&self, frame: &mut Frame<'_>, body: Rect, top: usize, left: u16) {
        let bottom = top + usize::from(body.height);
        let start = self.rows.start.max(top);
        let end = self.rows.end.min(bottom);
        let Some(height) = end.checked_sub(start).filter(|height| *height > 0) else {
            return;
        };
        let (Ok(y), Ok(height), Ok(skip)) = (
            u16::try_from(start - top),
            u16::try_from(height),
            i16::try_from(start - self.rows.start),
        ) else {
            return;
        };
        let x = self.column.saturating_sub(left);
        let skip_x = left.saturating_sub(self.column);
        let width = self
            .protocol
            .size()
            .width
            .saturating_sub(skip_x)
            .min(body.width.saturating_sub(x));
        if width == 0 {
            return;
        }
        let clip = Rect::new(body.x + x, body.y + y, width, height);
        frame.buffer_mut().set_style(clip, Style::reset());
        self.drawn.set(true);
        frame.render_widget(SlicedImage::new(&self.protocol, (0, -skip).into()), clip);
        if skip_x != 0
            && let Some(&column) = KITTY_COLUMNS.get(usize::from(skip_x))
        {
            // SlicedImage supports row clipping but always starts at tile column zero.
            // Keep its transfer, row and image ID; replace just that first column mark.
            let buffer = frame.buffer_mut();
            for y in clip.top()..clip.bottom() {
                let cell = &mut buffer[(clip.x, y)];
                if let Some((prefix, marks)) = cell.symbol().rsplit_once(PLACEHOLDER) {
                    let mut marks = marks.chars();
                    if let (Some(row), Some(_)) = (marks.next(), marks.next()) {
                        let symbol =
                            format!("{prefix}{PLACEHOLDER}{row}{column}{}", marks.as_str());
                        cell.set_symbol(&symbol);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn kitty_uses_real_cell_size_and_clips_both_edges() -> Result<(), Box<dyn std::error::Error>> {
        let image = DynamicImage::new_rgb8(40, 64);
        let mode = ImageMode::Kitty {
            cell_width: 8,
            cell_height: 16,
        };
        let placed = PlacedImage::new(&image, 5, 2, mode).ok_or("image placement failed")?;
        let mut terminal = Terminal::new(TestBackend::new(7, 4))?;
        assert_eq!(placed.rows, 2..6);
        assert!(placed.transmitted_id().is_none());
        terminal.draw(|frame| placed.draw(frame, Rect::new(1, 1, 5, 2), 3, 0))?;
        let buffer = terminal.backend().buffer();
        assert!(buffer[(1, 1)].symbol().contains("a=T,U=1"));
        assert!(
            buffer[(1, 1)]
                .symbol()
                .contains("\u{10eeee}\u{030d}\u{0305}")
        );
        assert!(placed.transmitted_id().is_some());
        assert!(
            buffer[(1, 2)]
                .symbol()
                .starts_with("\u{10eeee}\u{030e}\u{0305}")
        );
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(1, 3)].symbol(), " ");
        assert_eq!(buffer[(6, 2)].symbol(), " ");
        terminal.draw(|frame| placed.draw(frame, Rect::new(1, 1, 5, 2), 1, 0))?;
        assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), " ");
        assert!(
            terminal.backend().buffer()[(1, 2)]
                .symbol()
                .starts_with("\u{10eeee}\u{0305}\u{0305}")
        );
        let narrow = PlacedImage::new(&image, 2, 2, mode).ok_or("narrow placement failed")?;
        assert_eq!(narrow.rows, 2..4);
        assert_ne!(narrow.kitty_id, placed.kitty_id);
        assert!(PlacedImage::new(&image, 5, 2, ImageMode::Text).is_none());
        Ok(())
    }

    #[test]
    fn kitty_preserves_images_wider_than_1600_pixels_when_they_fit()
    -> Result<(), Box<dyn std::error::Error>> {
        let image = DynamicImage::new_rgb8(1917, 64);
        let mode = ImageMode::Kitty {
            cell_width: 16,
            cell_height: 16,
        };
        let placed = PlacedImage::new(&image, 128, 0, mode).ok_or("image placement failed")?;
        assert!(matches!(placed.protocol, SlicedProtocol::Kitty(_)));
        assert_eq!(placed.protocol.size(), Size::new(120, 4));
        Ok(())
    }

    #[test]
    fn invalid_kitty_cell_size_does_not_place_an_image() {
        let mode = ImageMode::Kitty {
            cell_width: 0,
            cell_height: 16,
        };
        assert!(PlacedImage::new(&DynamicImage::new_rgb8(8, 16), 10, 0, mode).is_none());
    }
}
