use image::RgbaImage;

use crate::pdf::text::Bounds;

/// Apply approximate text-cell bounds to the fitted display raster only.
/// `current` takes precedence in overlaps, and a pixel is blended at most once.
pub(crate) fn paint(image: &mut RgbaImage, marks: impl Iterator<Item = (Bounds, bool)>) {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let mut coverage = vec![0_u8; width * height];
    let mut budget = coverage.len().saturating_mul(4);
    for (Bounds([left, top, right, bottom]), current) in marks {
        let [x0, y0, x1, y1] = [
            pixel(left, image.width(), false),
            pixel(top, image.height(), false),
            pixel(right, image.width(), true),
            pixel(bottom, image.height(), true),
        ];
        let area = (x1 - x0) * (y1 - y0);
        // A PDF may repeat or overlap many very large text cells. Reserve
        // work for the current match even if other marks fill the budget.
        if !current {
            let Some(remaining) = budget.checked_sub(area) else {
                continue;
            };
            budget = remaining;
        }
        for row in y0..y1 {
            let span = &mut coverage[row * width + x0..row * width + x1];
            for cell in span {
                *cell = (*cell).max(if current { 2 } else { 1 });
            }
        }
    }
    for (pixel, mark) in image.pixels_mut().zip(coverage) {
        let tint = match mark {
            1 => [0, 0, 255],
            2 => [255, 255, 0],
            _ => continue,
        };
        for (channel, tint) in pixel.0.iter_mut().zip(tint) {
            // About 30% opacity, leaving the source ink legible.
            *channel = u8::try_from((u16::from(*channel) * 7 + tint * 3) / 10).unwrap_or_default();
        }
    }
}

fn pixel(fraction: f64, size: u32, end: bool) -> usize {
    let at = fraction.clamp(0., 1.) * f64::from(size);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded by the raster's dimensions"
    )]
    let at = if end { at.ceil() } else { at.floor() } as usize;
    at
}
