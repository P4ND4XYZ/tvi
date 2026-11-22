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

    pub(super) fn visible_at_width(&self, width: u16) -> bool {
        self.column < width
    }

    pub fn transmitted_id(&self) -> Option<u32> {
        self.drawn.get().then_some(self.kitty_id)
    }

    pub fn height(&self) -> u16 {
        self.protocol.size().height
    }

    pub fn draw(&self, frame: &mut Frame<'_>, body: Rect, top: usize) {
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
        if self.column >= body.width {
            return;
        }
        let clip = Rect::new(
            body.x + self.column,
            body.y + y,
            body.width - self.column,
            height,
        );
        let bounds = Rect {
            width: self.protocol.size().width.min(clip.width),
            ..clip
        };
        frame.buffer_mut().set_style(bounds, Style::reset());
        self.drawn.set(true);
        frame.render_widget(SlicedImage::new(&self.protocol, (0, -skip).into()), clip);
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
        terminal.draw(|frame| placed.draw(frame, Rect::new(1, 1, 5, 2), 3))?;
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
        terminal.draw(|frame| placed.draw(frame, Rect::new(1, 1, 5, 2), 1))?;
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
