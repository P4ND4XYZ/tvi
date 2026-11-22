use std::{ops::Range, sync::Arc};

use crate::{
    app::PdfSearch,
    pdf::{Page, text::Hit},
    ui::layout::pdf_marks,
};

/// Only the slice belonging to the target page is baked into its display tiles.
#[derive(Clone)]
pub(super) struct Marks {
    hits: Arc<[Hit]>,
    range: Range<usize>,
    current: Option<usize>,
}

impl PartialEq for Marks {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.hits, &other.hits)
            && self.range == other.range
            && self.current == other.current
    }
}

impl Marks {
    pub(super) fn for_page(search: Option<&PdfSearch>, page: u32) -> Option<Self> {
        let search = search.filter(|search| search.highlights)?;
        let start = search.hits.partition_point(|hit| hit.page < page);
        let end = search.hits.partition_point(|hit| hit.page <= page);
        (start < end).then(|| Self {
            hits: Arc::clone(&search.hits),
            range: start..end,
            current: (start..end)
                .contains(&search.selected)
                .then_some(search.selected),
        })
    }

    pub(super) fn paint(&self, page: &Page, image: &mut image::RgbaImage) {
        let mut marks = self
            .range
            .clone()
            .filter_map(|i| {
                let hit = self.hits[i];
                if !page.supports_hit(hit) {
                    return None;
                }
                hit.bounds.map(|bounds| (bounds, self.current == Some(i)))
            })
            .peekable();
        if marks.peek().is_some() {
            pdf_marks::paint(image, marks);
        }
    }
}
