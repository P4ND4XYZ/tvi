use crate::pdf::Page;

use super::super::{Motion, PdfSearch, ScrollEdge, ViewAnchor, ViewRows, Viewport};

pub(crate) struct PdfDocument {
    page: Page,
    search: Option<PdfSearch>,
    scale: f64,
    center_x: f64,
}

impl PdfDocument {
    pub(super) fn new(page: Page) -> Self {
        Self {
            page,
            search: None,
            scale: 1.0,
            center_x: 0.5,
        }
    }

    pub(crate) fn scale(&self) -> f64 {
        self.scale
    }

    pub(crate) fn center_x(&self) -> f64 {
        self.center_x
    }

    pub(super) fn set_view(&mut self, scale: f64, center_x: f64) {
        self.scale = scale;
        self.center_x = center_x;
    }

    pub(crate) fn page(&self) -> &Page {
        &self.page
    }

    pub(crate) fn search(&self) -> Option<&PdfSearch> {
        self.search.as_ref()
    }

    pub(super) fn replace_page(&mut self, next: Page) {
        self.page = next;
    }

    pub(super) fn publish_search(&mut self, result: PdfSearch) {
        self.search = Some(result);
    }

    pub(super) fn hide_highlights(&mut self) {
        if let Some(search) = &mut self.search {
            search.highlights = false;
        }
    }

    pub(crate) fn turn_target(&self, edge: ScrollEdge) -> Option<u32> {
        let page = &self.page;
        match edge {
            ScrollEdge::End => (page.index + 1 < page.source.count).then_some(page.index + 1),
            ScrollEdge::Start => page.index.checked_sub(1),
        }
    }

    pub(crate) fn arrival_anchor(previous: bool) -> ViewAnchor {
        if previous {
            ViewAnchor::text(usize::MAX)
        } else {
            ViewAnchor::default()
        }
    }

    pub(super) fn navigate(&self, viewport: &mut Viewport, motion: Motion, rows: &impl ViewRows) {
        let top = viewport.top;
        let anchor = viewport.anchor;
        viewport.navigate(motion, rows);
        if rows.len() == 0 || viewport.height == 0 {
            return;
        }
        let last = viewport.last_top(rows);
        if motion == Motion::End || (viewport.top == last && top < last) {
            viewport.anchor = Self::arrival_anchor(true);
        } else if motion != Motion::Start && viewport.top == top {
            // An inert scroll at a clamped edge must not discard the reflow anchor.
            viewport.anchor = anchor;
        }
    }
}
