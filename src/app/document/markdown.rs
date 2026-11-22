use std::ops::Range;

use crate::markdown::MarkdownResources;

use super::super::{Direction, ViewRows, Viewport, search::Search};
use super::DocumentLink;

pub(crate) struct Markdown {
    text: String,
    links: Vec<DocumentLink>,
    resources: MarkdownResources,
    search: Search,
}

impl Markdown {
    pub(super) fn new(text: String, links: Vec<DocumentLink>) -> Self {
        Self {
            text,
            links,
            resources: MarkdownResources::default(),
            search: Search::default(),
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn links(&self) -> &[DocumentLink] {
        &self.links
    }

    pub(super) fn set_resources(&mut self, resources: MarkdownResources) {
        self.resources = resources;
    }

    #[cfg(test)]
    pub(crate) fn resources(&self) -> &MarkdownResources {
        &self.resources
    }

    pub(super) fn display_input(&mut self) -> (&str, &mut MarkdownResources) {
        (&self.text, &mut self.resources)
    }

    pub(super) fn freeze_for_history(&mut self) {
        // A page left before its first draw (e.g. by Lua) must not convert on return.
        self.resources.successful_diagrams.get_or_insert_default();
    }

    pub(super) fn inherit_search(&mut self, old: &mut Self) {
        self.search = std::mem::take(&mut old.search);
    }

    pub(super) fn reset_search(&mut self) {
        self.search = Search::default();
    }

    pub(super) fn search(&self) -> &Search {
        &self.search
    }

    pub(super) fn highlights(&self) -> (&[Range<usize>], Option<&Range<usize>>) {
        if self.search.highlights {
            (self.search.matches(), self.search.current())
        } else {
            (&[], None)
        }
    }

    pub(super) fn hide_highlights(&mut self) {
        self.search.highlights = false;
    }

    pub(super) fn submit_search(
        &mut self,
        viewport: &mut Viewport,
        rows: &impl ViewRows,
        text: &str,
        query: String,
        direction: Direction,
    ) -> Option<String> {
        let offset = rows.anchor_at(viewport.top).unwrap_or(viewport.anchor).byte;
        self.search.submit(query, direction, text, offset);
        self.reveal_search(viewport, rows)
    }

    pub(super) fn repeat_search(
        &mut self,
        viewport: &mut Viewport,
        rows: &impl ViewRows,
        reverse: bool,
    ) -> Option<String> {
        self.search.step(reverse);
        self.reveal_search(viewport, rows)
    }

    fn reveal_search(&self, viewport: &mut Viewport, rows: &impl ViewRows) -> Option<String> {
        if let Some(hit) = self.search.current() {
            viewport.search_target(hit.start, rows);
            None
        } else if self.search.query().is_empty() {
            Some("No previous search".into())
        } else {
            Some("No matches".into())
        }
    }

    pub(super) fn restore_display_positions(
        &mut self,
        anchor: &mut usize,
        text: &str,
        expansions: &[Range<usize>],
        uncertain: bool,
    ) {
        if expansions.is_empty() && !uncertain {
            return;
        }
        // Each restored block replaces one empty line. Its new byte range is the
        // number of bytes added, excluding the newline that already followed it.
        let remap = |offset: usize| {
            let mut added = 0;
            for range in expansions {
                if offset <= range.start - added {
                    break;
                }
                added += range.len();
            }
            offset + added
        };
        *anchor = remap(*anchor);
        self.search.refresh(text, remap, *anchor);
    }

    pub(super) fn refresh_reloaded_positions(&mut self, anchor: &mut usize, text: &str) {
        *anchor = (*anchor).min(text.len());
        self.search.refresh(text, |offset| offset, *anchor);
    }
}
