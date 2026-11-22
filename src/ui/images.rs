use std::{cell::Cell, ops::Range};

use crate::terminal::ImageMode;
use image::{DynamicImage, Pixel, RgbImage, Rgba};
use ratatui::{
    Frame,
    layout::{Rect, Size},
    style::Style,
};

use ratatui_image::{
    protocol::{halfblocks::Halfblocks, kitty::Kitty},
    sliced::{SlicedImage, SlicedProtocol},
};

pub struct PlacedImage {
    pub rows: Range<usize>,
    column: u16,
    protocol: SlicedProtocol,
    kitty_id: Option<u32>,
    drawn: Cell<bool>,
}

impl PlacedImage {
    pub fn new(image: &DynamicImage, columns: u16, start: usize, mode: ImageMode) -> Option<Self> {
        let (protocol, kitty_id) = match mode {
            ImageMode::Text => return None,
            ImageMode::Halfblocks => (Self::halfblocks(image, columns)?, None),
            ImageMode::Kitty {
                cell_width,
                cell_height,
            } => {
                let id = crate::terminal::next_image_id();
                let protocol = Self::kitty(image, columns, cell_width, cell_height, id)
                    .or_else(|| Self::halfblocks(image, columns))?;
                let id = matches!(protocol, SlicedProtocol::Kitty(_)).then_some(id);
                (protocol, id)
            }
        };
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
            kitty_id: Some(id),
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

    fn halfblocks(image: &DynamicImage, columns: u16) -> Option<SlicedProtocol> {
        let flattened;
        let image = if image.color().has_alpha() {
            // Halfblocks はアルファを捨てるので、縮小前に白背景へ合成する。
            let rgba = image.to_rgba8();
            flattened =
                DynamicImage::ImageRgb8(RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
                    let mut background = Rgba([255, 255, 255, 255]);
                    background.blend(rgba.get_pixel(x, y));
                    background.to_rgb()
                }));
            &flattened
        } else {
            image
        };
        // 1:2 の文字セルを想定。極端な寸法でもプロトコルのセル数を u16 に収める。
        let max_width = u32::from(columns.min(160)) * 10;
        let max_height = 160 * 20;
        let pixels = if image.width() > max_width || image.height() > max_height {
            image.resize(max_width, max_height, image::imageops::FilterType::Triangle)
        } else {
            // Halfblocks が使う色数に先に落とし、元画像のコピーを避ける。
            image.resize(
                image.width().div_ceil(10),
                image.height().div_ceil(20) * 2,
                image::imageops::FilterType::Triangle,
            )
        };
        let (width, height) = if image.width() > max_width || image.height() > max_height {
            (pixels.width().div_ceil(10), pixels.height().div_ceil(20))
        } else {
            (image.width().div_ceil(10), image.height().div_ceil(20))
        };
        let size = Size::new(u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        Some(SlicedProtocol::Halfblocks(
            Halfblocks::new(pixels, size).ok()?,
        ))
    }

    pub(super) fn visible_at_width(&self, width: u16) -> bool {
        self.column < width
    }

    pub fn transmitted_id(&self) -> Option<u32> {
        self.kitty_id.filter(|_| self.drawn.get())
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
    use std::io::Cursor;

    use image::{ImageFormat, RgbaImage};
    use ratatui::{Terminal, backend::TestBackend, style::Color};

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
    fn transparent_png_preserves_black_line_against_its_background()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = DynamicImage::ImageRgba8(RgbaImage::from_fn(70, 20, |x, _| {
            Rgba([0, 0, 0, if (20..50).contains(&x) { 255 } else { 0 }])
        }));
        let mut png = Cursor::new(Vec::new());
        source.write_to(&mut png, ImageFormat::Png)?;
        let decoded = image::load_from_memory_with_format(png.get_ref(), ImageFormat::Png)?;
        let placed = PlacedImage::new(&decoded, 7, 0, ImageMode::Halfblocks)
            .ok_or("image placement failed")?;
        let mut terminal = Terminal::new(TestBackend::new(7, 1))?;
        terminal.draw(|frame| placed.draw(frame, frame.area(), 0))?;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(255, 255, 255));
        assert_eq!(buffer[(3, 0)].bg, Color::Rgb(0, 0, 0));
        assert_eq!(buffer[(6, 0)].bg, Color::Rgb(255, 255, 255));
        Ok(())
    }
}
