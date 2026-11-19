use std::ops::Range;

use ratatui::text::Text;

pub mod tui_markdown;

pub trait MarkdownRenderer {
    #[cfg(test)]
    fn render(&self, markdown: &str, width: u16) -> RenderedMarkdown {
        self.render_document(markdown, width, &MarkdownResources::default())
    }
    fn render_document(
        &self,
        markdown: &str,
        width: u16,
        resources: &MarkdownResources,
    ) -> RenderedMarkdown;
    fn resources(&self, markdown: &str) -> MarkdownResources;
}

#[derive(Debug, PartialEq, Eq)]
pub struct MarkdownLink {
    pub label: String,
    pub target: String,
}

#[derive(Default)]
pub struct MarkdownResources {
    pub links: Vec<MarkdownLink>,
    pub images: Vec<ImageReference>,
    pub image_count: usize,
}

/// すべての画像開始イベントを数えた文書内通番。
#[derive(Debug)]
pub struct ImageReference {
    pub ordinal: usize,
    pub target: String,
    pub has_alt: bool,
}

#[derive(Debug)]
pub struct ImageLine {
    pub line: usize,
    pub image: usize,
    pub prefix_bytes: usize,
    pub has_alt: bool,
}

/// 幅に依存しない論理行と、検索テキスト上のUTF-8バイト範囲。
/// 論理行の区切りはLF。各行の範囲に区切り自体は含まれない。
pub struct RenderedMarkdown {
    text: Text<'static>,
    search_text: String,
    line_ranges: Vec<Range<usize>>,
    pub images: Vec<ImageLine>,
    width: u16,
}

impl RenderedMarkdown {
    /// 各 span は行内テキストとし、タブ・改行・制御文字は adapter で正規化する。
    #[must_use]
    pub fn new(text: Text<'static>, width: u16) -> Self {
        let mut search_text = String::new();
        let mut line_ranges = Vec::with_capacity(text.lines.len());
        for (index, line) in text.lines.iter().enumerate() {
            if index != 0 {
                search_text.push('\n');
            }
            let start = search_text.len();
            for span in &line.spans {
                search_text.push_str(&span.content);
            }
            line_ranges.push(start..search_text.len());
        }
        Self {
            text,
            search_text,
            line_ranges,
            images: Vec::new(),
            width,
        }
    }

    #[must_use]
    pub fn text(&self) -> &Text<'static> {
        &self.text
    }

    #[must_use]
    pub fn search_text(&self) -> &str {
        &self.search_text
    }

    #[must_use]
    pub fn line_ranges(&self) -> &[Range<usize>] {
        &self.line_ranges
    }

    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }
}
