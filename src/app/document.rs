use std::{
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::markdown::MarkdownResources;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct Document {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) markdown: String,
    pub(super) links: Vec<DocumentLink>,
    pub(super) path: PathBuf,
    pub(super) resources: MarkdownResources,
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
            markdown,
            links: links
                .into_iter()
                .map(|(label, target)| DocumentLink {
                    id: next_id(),
                    label,
                    target,
                })
                .collect(),
            path,
            resources: MarkdownResources::default(),
        }
    }

    pub fn with_resources(mut self, resources: MarkdownResources) -> Self {
        self.resources = resources;
        self
    }

    pub(super) fn resolve_link(&self, id: u64) -> Result<PathBuf, String> {
        let link = self
            .links
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
