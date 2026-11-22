use std::{
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{markdown::MarkdownResources, pdf::Page};

use super::PdfSearch;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct Document {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) content: Content,
}

pub(super) enum Content {
    Markdown(Markdown),
    Pdf {
        page: Page,
        search: Option<PdfSearch>,
    },
}

pub(super) struct Markdown {
    pub text: String,
    pub links: Vec<DocumentLink>,
    pub resources: MarkdownResources,
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
            content: Content::Markdown(Markdown {
                text: markdown,
                links: links
                    .into_iter()
                    .map(|(label, target)| DocumentLink {
                        id: next_id(),
                        label,
                        target,
                    })
                    .collect(),
                resources: MarkdownResources::default(),
            }),
        }
    }

    pub fn with_resources(mut self, resources: MarkdownResources) -> Self {
        if let Content::Markdown(markdown) = &mut self.content {
            markdown.resources = resources;
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
            content: Content::Pdf { page, search: None },
        }
    }

    pub(super) fn links(&self) -> &[DocumentLink] {
        match &self.content {
            Content::Markdown(markdown) => &markdown.links,
            Content::Pdf { .. } => &[],
        }
    }

    pub(super) fn display_input(&mut self) -> Option<(&Path, &str, &mut MarkdownResources)> {
        match &mut self.content {
            Content::Markdown(markdown) => {
                Some((&self.path, &markdown.text, &mut markdown.resources))
            }
            Content::Pdf { .. } => None,
        }
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
