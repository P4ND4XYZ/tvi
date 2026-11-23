use super::{PDF_TILE_PIXELS, PdfGeometry};
use crate::ui::images::PDF_TILE_COLUMNS;

const MAX_DISPLAY_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_ROWS: u16 = 8192;

/// Bounded page raster and its cell-padded Kitty grid. The same calculation
/// validates a request before queuing and determines the worker's output size.
#[derive(Clone, Copy)]
pub(super) struct Raster {
    pub width: u32,
    pub height: u32,
    pub columns: u16,
    pub rows: u16,
    pub tile_columns: u16,
    pub tile_rows: u16,
    width_in_cells: f64,
    pub height_in_cells: f64,
}

impl PdfGeometry {
    pub(crate) fn initial_scale(self, aspect_height: f64, body_rows: u16) -> Option<f64> {
        let width = f64::from(u32::from(self.columns) * u32::from(self.cell_width));
        let height = f64::from(u32::from(body_rows) * u32::from(self.cell_height));
        if width == 0.0 || height == 0.0 || !aspect_height.is_finite() || aspect_height <= 0.0 {
            return None;
        }
        let fitted_width = (height / aspect_height).min(width).max(1.0);
        let scale = fitted_width / width;
        // Check the same width raster() will round: dividing and multiplying
        // by the body width can move the result across a half-pixel boundary.
        let scale = if ((width * scale).round() * aspect_height).round() > height {
            fitted_width.floor() / width
        } else {
            scale
        };
        // Keep the existing one-pixel minimum even if division rounds down.
        let scale = if width * scale < 1.0 {
            scale.next_up()
        } else {
            scale
        };
        let raster = self.raster(aspect_height, scale)?;
        (raster.columns <= self.columns && raster.rows <= body_rows).then_some(scale)
    }

    pub(crate) fn can_display(self, aspect_height: f64, scale: f64) -> bool {
        self.raster(aspect_height, scale).is_some()
    }

    pub(super) fn raster(self, aspect_height: f64, scale: f64) -> Option<Raster> {
        if !scale.is_finite() || scale <= 0.0 || !aspect_height.is_finite() || aspect_height <= 0.0
        {
            return None;
        }
        let cw = u32::from(self.cell_width);
        let ch = u32::from(self.cell_height);
        let cells_per_tile = PDF_TILE_PIXELS.checked_div(cw * ch)?;
        let width = f64::from(u32::from(self.columns) * cw) * scale;
        if !(1.0..=f64::from(u32::from(u16::MAX) * cw)).contains(&width) {
            return None;
        }
        let width = width.round();
        let height = (width * aspect_height).round().max(1.0);
        if height > f64::from(MAX_ROWS) * f64::from(ch) {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rounded pixel dimensions were bounded above"
        )]
        let (width, height) = (width as u32, height as u32);
        let columns = u16::try_from(width.div_ceil(cw)).ok()?;
        let rows = u16::try_from(height.div_ceil(ch)).ok()?;
        if u64::from(columns) * u64::from(cw) * u64::from(rows) * u64::from(ch) > MAX_DISPLAY_PIXELS
        {
            return None;
        }
        // Keep bands shallow to bound transfers outside the viewport. Each
        // dimension also stays below the Kitty placeholder coordinate limit.
        let tile_columns = u16::try_from(cells_per_tile.min(PDF_TILE_COLUMNS))
            .ok()?
            .min(columns);
        let tile_rows =
            u16::try_from(cells_per_tile.checked_div(u32::from(tile_columns))?.min(8)).ok()?;
        Some(Raster {
            width,
            height,
            columns,
            rows,
            tile_columns,
            tile_rows,
            width_in_cells: f64::from(width) / f64::from(cw),
            height_in_cells: f64::from(height) / f64::from(ch),
        })
    }
}

impl Raster {
    pub(super) fn left(self, viewport: u16, center: f64) -> u16 {
        let left = (center * self.width_in_cells - f64::from(viewport) / 2.0)
            .round()
            .clamp(0.0, f64::from(self.columns.saturating_sub(viewport)));
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rounded and clamped to valid u16 columns"
        )]
        let left = left as u16;
        left
    }

    pub(super) fn center_x(self, viewport: u16, left: u16) -> f64 {
        if self.columns <= viewport {
            0.5
        } else {
            ((f64::from(left) + f64::from(viewport) / 2.0) / self.width_in_cells).min(1.0)
        }
    }

    pub(super) fn pan_x(self, viewport: u16, center: f64, columns: i64) -> f64 {
        let left = self.left(viewport, center);
        let last = self.columns.saturating_sub(viewport);
        let to = i64::from(left)
            .saturating_add(columns)
            .clamp(0, i64::from(last));
        let to = u16::try_from(to).unwrap_or(left);
        if to == left {
            center
        } else {
            self.center_x(viewport, to)
        }
    }

    pub(super) fn reveal_x(self, width: u16, center: f64, (first, last): (f64, f64)) -> f64 {
        let left = f64::from(self.left(width, center));
        if self.columns <= width
            || (first * self.width_in_cells >= left
                && last * self.width_in_cells <= left + f64::from(width))
        {
            center
        } else {
            first.midpoint(last)
        }
    }

    pub(super) fn center_y(self, height: usize, top: usize) -> f64 {
        let cell_count = |n| f64::from(u16::try_from(n).unwrap_or(u16::MAX));
        let (height, top) = (cell_count(height), cell_count(top));
        let visible = height.min(self.height_in_cells);
        ((top + visible / 2.0) / self.height_in_cells).min(1.0)
    }

    pub(super) fn top(self, height: usize, center: f64) -> usize {
        let viewport = f64::from(u16::try_from(height).unwrap_or(u16::MAX));
        let top = (center * self.height_in_cells - viewport / 2.0)
            .round()
            .max(0.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rounded nonnegative offset within the raster's bounded row count"
        )]
        let top = top as usize;
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_scale_caps_width_and_avoids_rounding_into_an_extra_row() {
        let geometry = PdfGeometry {
            columns: 12,
            cell_width: 2,
            cell_height: 4,
        };
        let width_fit = geometry.initial_scale(0.5, 4);
        assert_eq!(width_fit, Some(1.0));

        // The 16-pixel body allows 10.66... pixels of width at 3:2. Rounding
        // to 11 would produce 17 pixels of height in the existing raster path.
        let raster = geometry
            .initial_scale(1.5, 4)
            .and_then(|scale| geometry.raster(1.5, scale))
            .map(|raster| (raster.width, raster.height, raster.rows));
        assert_eq!(raster, Some((10, 15, 4)));
    }

    #[test]
    fn initial_scale_fits_after_half_pixel_rounding_through_zoom() {
        let geometry = PdfGeometry {
            columns: 80,
            cell_width: 8,
            cell_height: 16,
        };
        let aspect = 768.0 / 353.0;
        let dimensions = geometry
            .initial_scale(aspect, 24)
            .and_then(|scale| geometry.raster(aspect, scale))
            .map(|raster| (raster.width, raster.height, raster.columns, raster.rows));
        assert_eq!(dimensions, Some((176, 383, 22, 24)));
    }

    #[test]
    fn initial_scale_requires_a_displayable_raster_in_the_body() {
        let geometry = PdfGeometry {
            columns: 12,
            cell_width: 2,
            cell_height: 4,
        };
        assert_eq!(geometry.initial_scale(16.49, 4), Some(1.0 / 24.0));
        assert!(geometry.initial_scale(16.5, 4).is_none());
        assert!(geometry.initial_scale(1.0, 0).is_none());
        assert!(
            PdfGeometry {
                columns: 4096,
                cell_width: 8,
                cell_height: 16,
            }
            .initial_scale(0.125, 256)
            .is_none()
        );
    }

    #[test]
    fn zoom_range_accounts_for_pixel_budget_row_limit_and_one_pixel_minimum() {
        let geometry = PdfGeometry {
            columns: 128,
            cell_width: 8,
            cell_height: 16,
        };
        assert!(geometry.can_display(1.0, 1.0 / 1024.0));
        assert!(!geometry.can_display(1.0, 1.0 / 2048.0));
        assert!(geometry.can_display(1.0, 5.0));
        assert!(!geometry.can_display(1.0, 6.0));
        assert!(geometry.can_display(512.0, 0.25));
        assert!(!geometry.can_display(513.0, 0.25));
    }
}
