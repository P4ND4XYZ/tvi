use std::{borrow::Cow, path::Path};

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span, Text};

use super::{MarkdownLink, MarkdownRenderer, RenderedMarkdown};

#[derive(Debug, Default, Clone, Copy)]
pub struct TuiMarkdown;

impl MarkdownRenderer for TuiMarkdown {
    fn render(&self, markdown: &str, width: u16) -> RenderedMarkdown {
        // ANSIを経由するコードハイライトに、入力のESC等を渡さない。
        let source = if markdown.chars().any(is_unsafe_control) {
            Cow::Owned(markdown.replace(is_unsafe_control, "�"))
        } else {
            Cow::Borrowed(markdown)
        };
        // 自然幅の表を論理行として保持する。表セルをライブラリ側で再配置すると、
        // 幅に応じてテキストと順序が変わるため、他の論理行と同様にレイアウトで折り返す。
        let options = ::tui_markdown::Options::default()
            .image_fallback(::tui_markdown::ImageFallback::AltText);
        let text = ::tui_markdown::from_str_with_options(&source, &options);
        let lines = text.lines.into_iter().map(|line| {
            let spans = line.spans.into_iter().map(|span| {
                Span::styled(
                    span.content
                        .replace('\t', "    ")
                        .replace(char::is_control, "�"),
                    span.style,
                )
            });
            let mut owned = spans.collect::<Line<'static>>().style(line.style);
            owned.alignment = line.alignment;
            owned
        });
        let mut owned = lines.collect::<Text<'static>>().style(text.style);
        owned.alignment = text.alignment;
        RenderedMarkdown::new(owned, width)
    }

    fn links(&self, markdown: &str) -> Vec<MarkdownLink> {
        // 描画側のパーサーと同じ拡張を有効にする。
        let options = Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_HEADING_ATTRIBUTES
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
            | Options::ENABLE_SUPERSCRIPT
            | Options::ENABLE_SUBSCRIPT
            | Options::ENABLE_MATH
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_DEFINITION_LIST
            | Options::ENABLE_GFM
            | Options::ENABLE_TABLES;
        let mut links = Vec::new();
        let mut active: Option<MarkdownLink> = None;
        let mut image_depth = 0;
        for event in Parser::new_ext(markdown, options) {
            match event {
                Event::Start(Tag::Image { .. }) => image_depth += 1,
                Event::End(TagEnd::Image) => image_depth -= 1,
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }) if image_depth == 0
                    && !matches!(link_type, LinkType::Autolink | LinkType::Email) =>
                {
                    active = local_markdown_target(&dest_url).map(|target| MarkdownLink {
                        label: String::new(),
                        target,
                    });
                }
                Event::End(TagEnd::Link) if image_depth == 0 => {
                    if let Some(mut link) = active.take() {
                        let label = link.label.trim();
                        link.label = if label.is_empty() {
                            link.target.clone()
                        } else {
                            label.to_owned()
                        };
                        links.push(link);
                    }
                }
                event => {
                    if let Some(MarkdownLink { label, .. }) = &mut active {
                        match event {
                            Event::Text(text) | Event::Code(text) | Event::InlineHtml(text) => {
                                label.push_str(&text);
                            }
                            Event::SoftBreak | Event::HardBreak => label.push(' '),
                            Event::InlineMath(math) => {
                                label.push('$');
                                label.push_str(&math);
                                label.push('$');
                            }
                            Event::FootnoteReference(reference) => {
                                label.push('[');
                                label.push_str(&reference);
                                label.push(']');
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        links
    }
}

fn is_unsafe_control(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\t' | '\r' | '\n')
}

fn local_markdown_target(target: &str) -> Option<String> {
    // エンコードされた区切り文字はファイル名の一部として扱う。
    if target.contains(['#', '?']) {
        return None;
    }
    let target = decode_percent_encoding(target)?;
    if target.starts_with(['/', '\\']) || target.chars().any(char::is_control) {
        return None;
    }
    // URIのスキームとWindowsのドライブ指定はどちらも相対リンクの対象外。
    if target.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
    }) {
        return None;
    }
    let file_name = target.rsplit(['/', '\\']).next().unwrap_or_default();
    Path::new(file_name)
        .extension()
        .is_some_and(|extension| extension == "md")
        .then(|| target.into_owned())
}

fn decode_percent_encoding(target: &str) -> Option<Cow<'_, str>> {
    if !target.contains('%') {
        return Some(Cow::Borrowed(target));
    }
    let mut remaining = target.as_bytes();
    let mut decoded = Vec::with_capacity(remaining.len());
    while let Some((&byte, tail)) = remaining.split_first() {
        if byte == b'%'
            && let [high, low, rest @ ..] = tail
            && let (Some(high), Some(low)) = (hex_digit(*high), hex_digit(*low))
        {
            decoded.push(high * 16 + low);
            remaining = rest;
        } else {
            decoded.push(byte);
            remaining = tail;
        }
    }
    String::from_utf8(decoded).ok().map(Cow::Owned)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_follow_their_appearance_and_include_resolved_references() {
        let source = r#"
[**はじめに** and `setup`](<guide/start here.md> "Title")

[Line
break][guide]

[Guide][] and [guide]

[][guide]

[guide]: ../guide.md
"#;
        let links = TuiMarkdown.links(source);
        let entries: Vec<_> = links
            .iter()
            .map(|link| (link.label.as_str(), link.target.as_str()))
            .collect();
        assert_eq!(
            entries,
            [
                ("はじめに and setup", "guide/start here.md"),
                ("Line break", "../guide.md"),
                ("Guide", "../guide.md"),
                ("guide", "../guide.md"),
                ("../guide.md", "../guide.md"),
            ]
        );
    }

    #[test]
    fn links_exclude_non_relative_destinations_code_and_image_contents() {
        let source = r#"---
metadata: "[YAML](metadata.md)"
---

![Alt [nested](nested.md)](image.md)
[![Image **label**](image.png)](image-guide.md)
[Website](https://example.test/index.md) [Scheme](custom:doc.md)
<user@example.md> [Network](//example.test/index.md)
[Anchor](#heading.md) [Fragment](file.md#heading) [Query](file.md?name=other.md)
[Absolute](/docs/index.md) [Drive](C:/docs/index.md) [Drive-relative](C:index.md)
[Windows root](\docs\index.md) [UNC](<\\server\docs\index.md>)
[Text](file.txt) [Directory](notes.md/) [Unknown][missing]
[Remote reference][remote]

[remote]: https://example.test/ref.md

`[Code](inline.md)`

Math $[Equation](math.md)$

```md
[Fenced](fenced.md)
```

    [Indented](indented.md)
"#;
        assert_eq!(
            TuiMarkdown.links(source),
            [MarkdownLink {
                label: "Image label".to_owned(),
                target: "image-guide.md".to_owned(),
            }]
        );
    }

    #[test]
    fn encoded_local_paths_are_decoded_once_before_validation() {
        let source = r"
[Space](My%20Guide.md) [Ref][guide]
[Hash](%23notes.md) [Question](question%3F.md) [Once](A%2520B.md)
[Literal percent](100%.md) [Plus](A+B.md)
[Raw fragment](file.md#%68eading) [Raw query](file.md?name=%23notes.md)
[Root](%2Fdocs/index.md) [Windows root](%5Cdocs%5Cindex.md)
[Drive](%43%3A/docs/index.md) [Scheme](https%3A//example.test/index.md)
[NUL](zero%00.md) [Control](ctrl%C2%85.md) [Invalid UTF-8](bad%FF.md)

[guide]: ../%E6%97%A5%E6%9C%AC%2emd
";
        let targets: Vec<_> = TuiMarkdown
            .links(source)
            .into_iter()
            .map(|link| link.target)
            .collect();
        assert_eq!(
            targets,
            [
                "My Guide.md",
                "../日本.md",
                "#notes.md",
                "question?.md",
                "A%20B.md",
                "100%.md",
                "A+B.md",
            ]
        );
    }

    #[test]
    fn control_characters_do_not_become_terminal_sequences_in_highlighted_code() {
        let document = TuiMarkdown.render("```rust\nlet x = \"\x1b[31m\";\t// x\n```", 40);
        assert!(
            document
                .search_text()
                .contains("let x = \"�[31m\";    // x")
        );
        assert!(
            document
                .search_text()
                .chars()
                .all(|ch| ch == '\n' || !ch.is_control())
        );
    }
}
