use pulldown_cmark::{CodeBlockKind, Event, Tag, TagEnd};

use super::tui_markdown::parser;

pub(crate) struct Diagram {
    pub ordinal: usize,
    pub line: usize,
    pub source: String,
}

pub(crate) fn extract(markdown: &str) -> Vec<Diagram> {
    let mut diagrams = Vec::new();
    let mut active = None::<Diagram>;
    let mut depth = 0;
    let mut ordinal = 0;
    let mut counted = 0;
    let mut line = 1;
    for (event, range) in parser(markdown).into_offset_iter() {
        match &event {
            Event::Start(Tag::CodeBlock(kind)) => {
                if depth == 0
                    && matches!(kind, CodeBlockKind::Fenced(info) if info.as_ref() == "mermaid")
                {
                    line += markdown.as_bytes()[counted..range.start]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count();
                    counted = range.start;
                    active = Some(Diagram {
                        ordinal,
                        line,
                        source: String::new(),
                    });
                }
                ordinal += 1;
            }
            Event::Text(text) => {
                if let Some(diagram) = &mut active {
                    diagram.source.push_str(text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(diagram) = active.take() {
                    diagrams.push(diagram);
                }
            }
            _ => {}
        }
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            _ => {}
        }
    }
    diagrams
}
