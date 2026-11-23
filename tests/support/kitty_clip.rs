//! Decode only the Kitty subset emitted by tvi PDF tiles: uncompressed RGBA
//! transfers and Unicode placeholders. This is a pixel oracle, not an emulator.

use std::{collections::HashMap, error::Error};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::RgbaImage;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

const APC: &str = "\x1b_G";
const PLACEHOLDER: char = '\u{10eeee}';
// First eight entries of Kitty's rowcolumn-diacritics table (PDF tiles are at
// most eight cell rows high). The first cell carries explicit row and column.
const OFFSETS: [char; 8] = [
    '\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}',
];

#[derive(Default)]
pub(super) struct KittyClip(HashMap<u32, RgbaImage>);

impl KittyClip {
    pub(super) fn retire(&mut self, ids: Vec<u32>) {
        for id in ids {
            let _removed = self.0.remove(&(id & 0xff_ffff));
        }
    }

    pub(super) fn receive(&mut self, buffer: &Buffer) -> Result<(), Box<dyn Error>> {
        for cell in &buffer.content {
            if let Some((_, packets)) = cell.symbol().split_once(APC) {
                let mut chunks = packets.split(APC).peekable();
                let mut data = String::new();
                let mut tile = None;
                while let Some(chunk) = chunks.next() {
                    let (header, payload) = chunk.split_once(';').ok_or("missing Kitty header")?;
                    let (payload, _) = payload
                        .split_once("\x1b\\")
                        .ok_or("unterminated Kitty packet")?;
                    let field = |key: &str| header.split(',').find_map(|f| f.strip_prefix(key));
                    let more = if chunks.peek().is_some() { "1" } else { "0" };
                    assert_eq!(field("m="), Some(more));
                    if tile.is_none() {
                        assert_eq!(field("a="), Some("T"));
                        assert_eq!(field("U="), Some("1"));
                        assert_eq!(field("f="), Some("32"));
                        assert_eq!(field("o="), None);
                        let integer = |key| -> Result<u32, Box<dyn Error>> {
                            Ok(field(key).ok_or("missing Kitty tile metadata")?.parse()?)
                        };
                        tile = Some((integer("i=")?, integer("s=")?, integer("v=")?));
                    }
                    data.push_str(payload);
                }
                let (id, w, h) = tile.ok_or("missing Kitty tile")?;
                let pixels = RgbaImage::from_raw(w, h, STANDARD.decode(data)?)
                    .ok_or("wrong Kitty tile size")?;
                // For the few concurrently live tiles the RGB portion of the
                // image ID suffices. Fail instead of aliasing two transfers.
                assert!(self.0.insert(id & 0xff_ffff, pixels).is_none());
            }
        }
        Ok(())
    }

    pub(super) fn clip(
        &self,
        buffer: &Buffer,
        body: Rect,
        cw: u32,
        ch: u32,
    ) -> Result<RgbaImage, Box<dyn Error>> {
        let mut clip = RgbaImage::new(u32::from(body.width) * cw, u32::from(body.height) * ch);
        for y in body.top()..body.bottom() {
            let mut previous = None;
            for x in body.left()..body.right() {
                let cell = &buffer[(x, y)];
                let (_, marks) = cell
                    .symbol()
                    .rsplit_once(PLACEHOLDER)
                    .ok_or("missing Kitty cell")?;
                let Color::Rgb(r, g, b) = cell.fg else {
                    return Err("missing Kitty image ID".into());
                };
                let id = u32::from_be_bytes([0, r, g, b]);
                let (row, col) = if marks.is_empty() {
                    let (prev_id, row, col) = previous.ok_or("missing first column offsets")?;
                    assert_eq!(id, prev_id);
                    (row, col + 1)
                } else {
                    let mut marks = marks.chars();
                    let row = marks
                        .next()
                        .and_then(|mark| OFFSETS.iter().position(|&m| m == mark))
                        .ok_or("unknown Kitty tile row")?;
                    let col = match marks.next() {
                        Some('\u{305}') => 0,
                        Some('\u{30d}') => 1,
                        Some('\u{35b}') => 16,
                        Some('\u{369}') => 23, // centered on the lower search hit
                        Some('\u{485}') => 32, // right edge at 2x in a 32-column viewport
                        _ => return Err("unexpected Kitty test column offset".into()),
                    };
                    (u32::try_from(row)?, col)
                };
                let tile = self.0.get(&id).ok_or("undelivered Kitty tile")?;
                assert!((col + 1) * cw <= tile.width() && (row + 1) * ch <= tile.height());
                for dy in 0..ch {
                    for dx in 0..cw {
                        let px = tile.get_pixel(col * cw + dx, row * ch + dy);
                        clip.put_pixel(
                            u32::from(x - body.x) * cw + dx,
                            u32::from(y - body.y) * ch + dy,
                            *px,
                        );
                    }
                }
                previous = Some((id, row, col));
            }
        }
        Ok(clip)
    }
}
