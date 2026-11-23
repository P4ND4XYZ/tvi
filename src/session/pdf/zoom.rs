use crate::{app::Document, pdf::text::Hit};

use super::{App, Failure, Geometry, Intent, Jobs, Layout, Request, Work, active_page, page_key};

pub(super) const ZOOM_NOTICE: &str = "Could not display the requested PDF zoom";

#[derive(Clone, Copy)]
pub(super) struct ZoomRequest {
    pub document: u64,
    pub epoch: u64,
    scale: f64,
}

impl Jobs {
    pub(in crate::session) fn set_zoom(
        &mut self,
        app: &App,
        geometry: Result<Geometry, Failure>,
        scale: f64,
    ) -> Result<(), String> {
        let pdf = app
            .document()
            .as_pdf()
            .ok_or("no PDF document is displayed")?;
        let geometry = geometry.map_err(|failure| failure.message)?;
        if !geometry.can_display(pdf.page().aspect_height(), scale) {
            return Err("zoom scale is outside the display limits for this page and window".into());
        }
        if self.requested_zoom.is_some_and(|requested| {
            requested.epoch == app.display_epoch() && requested.scale.to_bits() == scale.to_bits()
        }) {
            return Ok(());
        }
        self.cancel_if(|intent| matches!(intent, Intent::Zoom { .. }));
        self.requested_zoom = (pdf.scale().to_bits() != scale.to_bits()).then(|| ZoomRequest {
            document: app.document_id(),
            epoch: app.display_epoch(),
            scale,
        });
        Ok(())
    }

    pub(in crate::session) fn pan_x(
        &self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
        columns: i64,
    ) -> Result<(), String> {
        let pdf = app
            .document()
            .as_pdf()
            .ok_or("no PDF document is displayed")?;
        let width = geometry.map_err(|failure| failure.message)?.columns;
        let layout = self.placed(app).ok_or("PDF page is not displayed yet")?;
        let center = layout.pdf_pan_x(width, pdf.center_x(), columns);
        Document::set_pdf_view(app, pdf.scale(), center);
        Ok(())
    }

    // Share the bounded render queue with navigation, but do not cancel a turn,
    // search or open merely because the currently displayed PDF requested a zoom.
    pub(super) fn queue_zoom(&mut self, app: &App) {
        if self
            .requested_zoom
            .is_some_and(|zoom| zoom.epoch != app.display_epoch())
        {
            self.requested_zoom = None;
            self.cancel_if(|intent| matches!(intent, Intent::Zoom { .. }));
        }
        if self.pending().is_none()
            && let Some(zoom) = self.requested_zoom
            && let Some((from, page)) = page_key(app).zip(active_page(app))
        {
            self.queued = Some(Request {
                intent: Intent::Zoom {
                    from,
                    epoch: zoom.epoch,
                    scale: zoom.scale,
                },
                work: Work::Fit(page.clone()),
            });
        }
    }

    pub(super) fn commit_zoom(
        &mut self,
        app: &mut App,
        width: u16,
        scale: f64,
        next: &Layout<'static>,
    ) {
        if let Some(pdf) = app.document().as_pdf() {
            let center = self.placed(app).map_or(pdf.center_x(), |old| {
                old.pdf_center_x(width, pdf.center_x())
            });
            self.preserve_zoom_y(app, next);
            Document::set_pdf_view(app, scale, center);
        }
        self.requested_zoom = None;
        app.clear_notice_if(ZOOM_NOTICE);
    }

    pub(super) fn preserve_zoom_y(&self, app: &mut App, next: &Layout<'static>) {
        let height = app.viewport_height();
        if let Some(old) = self.placed(app)
            && let Some(center) = old.pdf_center_y(height, app.top_row())
        {
            app.show_top_row(next, next.pdf_top(height, center));
        }
    }
}

pub(super) fn reveal_hit(app: &mut App, layout: &Layout<'static>, width: u16, hit: Hit) {
    app.center_row(layout, layout.pdf_row(hit.from_top));
    if let Some(bounds) = hit.bounds
        && let Some(pdf) = app.document().as_pdf()
    {
        let [left, _, right, _] = bounds.0;
        let center = layout.pdf_reveal_x(width, pdf.center_x(), (left, right));
        Document::set_pdf_view(app, pdf.scale(), center);
    }
}
