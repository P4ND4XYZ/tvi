use std::path::PathBuf;

use crate::markdown::MarkdownResources;

use super::{App, Content, Document};

impl App {
    pub(crate) fn from_test_markdown(name: String, markdown: String) -> Self {
        let path = PathBuf::from(&name);
        Self::from_document(Document::new(name, markdown, path, Vec::new()))
    }

    pub(crate) fn markdown_resources(&self) -> &MarkdownResources {
        match &self.document.content {
            Content::Markdown(markdown) => &markdown.resources,
            Content::Pdf { .. } => panic!("expected Markdown resources"),
        }
    }
}
