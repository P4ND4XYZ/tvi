use std::{
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

mod markdown;
mod pdf;

use crate::{markdown::MarkdownResources, pdf::Page};

pub(crate) use self::{markdown::Markdown, pdf::PdfDocument};
use super::{App, Direction, Motion, PdfSearch, ViewRows, Viewport};
use std::ops::Range;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct Document {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) path: PathBuf,
    content: Content,
}

enum Content {
    Markdown(Markdown),
    Pdf(PdfDocument),
}

pub struct DocumentLink {
    pub id: u64,
    pub label: String,
    pub target: String,
}

impl Document {
    pub fn new(
        name: String,
        markdown: String,
        path: PathBuf,
        links: Vec<(String, String)>,
    ) -> Self {
        Self {
            id: next_id(),
            name,
            path,
            content: Content::Markdown(Markdown::new(
                markdown,
                links
                    .into_iter()
                    .map(|(label, target)| DocumentLink {
                        id: next_id(),
                        label,
                        target,
                    })
                    .collect(),
            )),
        }
    }

    pub fn with_resources(mut self, resources: MarkdownResources) -> Self {
        if let Content::Markdown(markdown) = &mut self.content {
            markdown.set_resources(resources);
        }
        self
    }

    pub(crate) fn pdf(path: PathBuf, page: Page) -> Self {
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .replace(char::is_control, "�");
        Self {
            id: next_id(),
            name,
            path,
            content: Content::Pdf(PdfDocument::new(page)),
        }
    }

    pub(super) fn links(&self) -> &[DocumentLink] {
        match &self.content {
            Content::Markdown(markdown) => markdown.links(),
            Content::Pdf(_) => &[],
        }
    }

    pub(crate) fn markdown(&self) -> Option<&Markdown> {
        match &self.content {
            Content::Markdown(markdown) => Some(markdown),
            Content::Pdf(_) => None,
        }
    }

    fn markdown_mut(&mut self) -> Option<&mut Markdown> {
        match &mut self.content {
            Content::Markdown(markdown) => Some(markdown),
            Content::Pdf(_) => None,
        }
    }

    pub(crate) fn as_pdf(&self) -> Option<&PdfDocument> {
        match &self.content {
            Content::Pdf(pdf) => Some(pdf),
            Content::Markdown(_) => None,
        }
    }

    fn pdf_mut(&mut self) -> Option<&mut PdfDocument> {
        match &mut self.content {
            Content::Pdf(pdf) => Some(pdf),
            Content::Markdown(_) => None,
        }
    }

    pub(crate) fn markdown_display_input(
        app: &mut App,
    ) -> Option<(&Path, &str, &mut MarkdownResources)> {
        let Content::Markdown(markdown) = &mut app.document.content else {
            return None;
        };
        let (text, resources) = markdown.display_input();
        Some((&app.document.path, text, resources))
    }

    pub(crate) fn restore_display_positions(
        app: &mut App,
        text: &str,
        expansions: &[Range<usize>],
        uncertain: bool,
    ) {
        if let Some(markdown) = app.document.markdown_mut() {
            markdown.restore_display_positions(
                &mut app.viewport.anchor.byte,
                text,
                expansions,
                uncertain,
            );
        }
    }

    pub(crate) fn refresh_reloaded_positions(app: &mut App, text: &str) {
        if let Some(markdown) = app.document.markdown_mut() {
            markdown.refresh_reloaded_positions(&mut app.viewport.anchor.byte, text);
        }
    }

    pub(crate) fn set_pdf_view(app: &mut App, scale: f64, center_x: f64) {
        if let Some(pdf) = app.document.pdf_mut() {
            pdf.set_view(scale, center_x);
        }
    }

    pub(crate) fn replace_pdf_page(app: &mut App, next: Page) {
        if let Some(pdf) = app.document.pdf_mut() {
            pdf.replace_page(next);
        }
    }

    pub(crate) fn publish_pdf_search(app: &mut App, result: PdfSearch) -> bool {
        let Some(pdf) = app.document.pdf_mut() else {
            return false;
        };
        pdf.publish_search(result);
        true
    }

    pub(super) fn freeze_for_history(&mut self) {
        if let Some(markdown) = self.markdown_mut() {
            markdown.freeze_for_history();
        }
    }

    pub(super) fn reset_search(&mut self) {
        if let Some(markdown) = self.markdown_mut() {
            markdown.reset_search();
        }
    }

    pub(super) fn inherit_search(&mut self, old: &mut Self) {
        if let Some((new, old)) = self.markdown_mut().zip(old.markdown_mut()) {
            new.inherit_search(old);
        }
    }

    pub(super) fn navigate(&self, viewport: &mut Viewport, motion: Motion, rows: &impl ViewRows) {
        match &self.content {
            Content::Markdown(_) => viewport.navigate(motion, rows),
            Content::Pdf(pdf) => pdf.navigate(viewport, motion, rows),
        }
    }

    pub(super) fn search_query(&self) -> &str {
        match &self.content {
            Content::Markdown(markdown) => markdown.search().query(),
            Content::Pdf(pdf) => pdf.search().map_or("", |search| &search.query),
        }
    }

    pub(super) fn search_selection(&self) -> Option<(usize, usize)> {
        match &self.content {
            Content::Markdown(markdown) => markdown.search().selection(),
            Content::Pdf(pdf) => pdf.search().and_then(super::PdfSearch::selection),
        }
    }

    pub(super) fn search_highlights_enabled(&self) -> bool {
        match &self.content {
            Content::Markdown(markdown) => markdown.search().highlights,
            Content::Pdf(pdf) => pdf.search().is_some_and(|search| search.highlights),
        }
    }

    pub(super) fn text_highlights(&self) -> (&[Range<usize>], Option<&Range<usize>>) {
        self.markdown().map_or((&[], None), Markdown::highlights)
    }

    pub(super) fn hide_highlights(&mut self) {
        match &mut self.content {
            Content::Markdown(markdown) => markdown.hide_highlights(),
            Content::Pdf(pdf) => pdf.hide_highlights(),
        }
    }

    // PDF submit and repeat require the asynchronous job coordinator in Session.
    pub(super) fn submit_search(
        &mut self,
        viewport: &mut Viewport,
        rows: &impl ViewRows,
        text: &str,
        query: String,
        direction: Direction,
    ) -> Option<String> {
        self.markdown_mut()?
            .submit_search(viewport, rows, text, query, direction)
    }

    pub(super) fn repeat_search(
        &mut self,
        viewport: &mut Viewport,
        rows: &impl ViewRows,
        reverse: bool,
    ) -> Option<String> {
        self.markdown_mut()?.repeat_search(viewport, rows, reverse)
    }

    pub(super) fn resolve_link(&self, id: u64) -> Result<PathBuf, String> {
        let link = self
            .links()
            .iter()
            .find(|link| link.id == id)
            .ok_or_else(|| "Link does not belong to the current document".to_owned())?;
        let target = Path::new(&link.target);
        if target.has_root() || matches!(target.components().next(), Some(Component::Prefix(_))) {
            return Err("Link target is not a relative path".into());
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new(""));
        Ok(parent.join(target))
    }
}
