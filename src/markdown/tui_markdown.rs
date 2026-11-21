use std::{
    borrow::Cow,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span, Text},
};

use super::{
    ImageLine, ImageReference, MarkdownLink, MarkdownRenderer, MarkdownResources, RenderedMarkdown,
    table::{BORDER_MARKER, take_tables},
};

#[derive(Debug, Default, Clone, Copy)]
pub struct TuiMarkdown;

impl MarkdownRenderer for TuiMarkdown {
    fn resources(&self, markdown: &str) -> MarkdownResources {
        extract_resources(markdown)
    }

    fn render_document(
        &self,
        markdown: &str,
        width: u16,
        resources: &MarkdownResources,
    ) -> RenderedMarkdown {
        self.render_with_diagrams(markdown, width, resources, &[])
    }
}

impl TuiMarkdown {
    // Diagram entries identify a block and either its image slot or a restored code block.
    pub(crate) fn render_with_diagrams(
        &self,
        markdown: &str,
        width: u16,
        resources: &MarkdownResources,
        diagrams: &[(usize, Option<usize>)],
    ) -> RenderedMarkdown {
        // ANSIを経由するコードハイライトに、入力のESC等を渡さない。
        let source = if markdown.chars().any(is_unsafe_control) {
            Cow::Owned(markdown.replace(is_unsafe_control, "�"))
        } else {
            Cow::Borrowed(markdown)
        };
        // 表の自然幅を検索用に保ち、折り返しに必要な罫線の位置だけを渡す。
        let mut image_lines = Vec::new();
        let mut marker = String::from("\u{e000}tvi-diagram");
        while source.contains(&marker) {
            marker.push('\u{e000}');
        }
        let sheet = PositionMarkers {
            next: std::sync::Arc::new(AtomicUsize::new(0)),
            ordinals: resources.images.iter().map(|image| image.ordinal).collect(),
            fences: std::sync::Arc::new(AtomicUsize::new(0)),
            diagrams: diagrams.into(),
            marker: marker.as_str().into(),
        };
        let options = ::tui_markdown::Options::new(sheet.clone());
        let mut text = ::tui_markdown::from_str_with_options(&source, &options);
        let Some((diagram_lines, expanded_lines)) = place_diagrams(&mut text, &marker, diagrams)
        else {
            // ライブラリ側のフェンス位置を確認できなければ、元のコードを描画する。
            return self.render_document(markdown, width, resources);
        };
        if !resources.images.is_empty() {
            for (index, line) in text.lines.iter_mut().enumerate() {
                if let Some(prefix) = line.spans.first().filter(|span| {
                    span.style.add_modifier.contains(IMAGE_POSITION)
                        && matches!(span.content.as_ref(), "[img]" | "[img] ")
                }) {
                    image_lines.push((index, prefix.content.len()));
                }
                for span in &mut line.spans {
                    span.style.add_modifier.remove(IMAGE_POSITION);
                }
            }
            if image_lines.len() != resources.images.len()
                || sheet.next.load(Ordering::Relaxed) != resources.image_count
            {
                // adapter と描画側のイベント対応が崩れた場合は本文を従来どおり表示する。
                image_lines.clear();
            }
        }
        let tables = take_tables(&mut text, || {
            use pulldown_cmark::Alignment::{Center, Right};
            use ratatui::layout::Alignment;

            parser(&source)
                .filter_map(|event| match event {
                    Event::Start(Tag::Table(alignments)) => Some(
                        alignments
                            .into_iter()
                            .map(|alignment| match alignment {
                                Center => Alignment::Center,
                                Right => Alignment::Right,
                                _ => Alignment::Left,
                            })
                            .collect(),
                    ),
                    _ => None,
                })
                .collect()
        });
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
        let mut document = RenderedMarkdown::new(owned, width);
        document.tables = tables;
        document.expanded_diagrams = expanded_lines
            .into_iter()
            .map(|lines| {
                document.line_ranges()[*lines.start()].start
                    ..document.line_ranges()[*lines.end()].end
            })
            .collect();
        document.images = image_lines
            .into_iter()
            .zip(&resources.images)
            .enumerate()
            .map(|(image, ((line, prefix_bytes), reference))| ImageLine {
                line,
                image,
                prefix_bytes,
                has_alt: reference.has_alt,
            })
            .collect();
        document.images.extend(diagram_lines);
        document.images.sort_unstable_by_key(|image| image.line);
        document
    }
}

// Replace only complete, marked logical blocks before calculating search offsets.
fn place_diagrams(
    text: &mut Text<'_>,
    marker: &str,
    diagrams: &[(usize, Option<usize>)],
) -> Option<(Vec<ImageLine>, Vec<std::ops::RangeInclusive<usize>>)> {
    if diagrams.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    let mut start = None;
    let mut blocks = Vec::new();
    for (index, line) in text.lines.iter().enumerate() {
        if let [span] = line.spans.as_slice()
            && let Some(suffix) = span.content.strip_prefix(marker)
        {
            match suffix {
                "mermaid" if start.is_none() => start = Some(index),
                "" => blocks.push(start.take()?..=index),
                _ => return None,
            }
        }
    }
    if start.is_some() || blocks.len() != diagrams.len() {
        return None;
    }
    let mut source = std::mem::take(&mut text.lines).into_iter();
    let mut consumed = 0;
    let mut placements = Vec::new();
    let mut expansions = Vec::new();
    for (block, &(_, image)) in blocks.into_iter().zip(diagrams) {
        text.lines
            .extend(source.by_ref().take(block.start() - consumed));
        let start = text.lines.len();
        let count = block.end() - block.start() + 1;
        if let Some(image) = image {
            source.by_ref().take(count).for_each(drop);
            text.lines.push(Line::default());
            placements.push(ImageLine {
                line: start,
                image,
                prefix_bytes: 0,
                has_alt: false,
            });
        } else {
            text.lines.extend(source.by_ref().take(count));
            text.lines[start].spans[0].content = Cow::Borrowed("```mermaid");
            text.lines[start + count - 1].spans[0].content = Cow::Borrowed("```");
            expansions.push(start..=start + count - 1);
        }
        consumed = block.end() + 1;
    }
    text.lines.extend(source);
    Some((placements, expansions))
}

// image_alt は各画像開始時に呼ばれる。本文や代替文字列を照合せず、
// pulldown-cmark と描画側で同じ出現順を使って、対象画像の論理行だけに印を付ける。
const IMAGE_POSITION: Modifier = Modifier::HIDDEN;

#[derive(Clone)]
struct PositionMarkers {
    next: std::sync::Arc<AtomicUsize>,
    ordinals: std::sync::Arc<[usize]>,
    fences: std::sync::Arc<AtomicUsize>,
    diagrams: std::sync::Arc<[(usize, Option<usize>)]>,
    marker: std::sync::Arc<str>,
}

impl ::tui_markdown::StyleSheet for PositionMarkers {
    fn code_block_fence(&self) -> &str {
        // tui-markdown calls this for the opening and closing line of each block.
        let ordinal = self.fences.fetch_add(1, Ordering::Relaxed) / 2;
        if self
            .diagrams
            .binary_search_by_key(&ordinal, |&(block, _)| block)
            .is_ok()
        {
            &self.marker
        } else {
            "```"
        }
    }

    fn table_border(&self) -> Style {
        ::tui_markdown::StyleSheet::table_border(&::tui_markdown::DefaultStyleSheet)
            .add_modifier(BORDER_MARKER)
    }

    fn image_alt(&self) -> Style {
        let ordinal = self.next.fetch_add(1, Ordering::Relaxed);
        let style = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);
        if self.ordinals.binary_search(&ordinal).is_ok() {
            style.add_modifier(IMAGE_POSITION)
        } else {
            style
        }
    }
}

pub(super) fn parser(markdown: &str) -> Parser<'_> {
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
    Parser::new_ext(markdown, options)
}

fn extract_resources(markdown: &str) -> MarkdownResources {
    let mut resources = MarkdownResources::default();
    let mut active: Option<MarkdownLink> = None;
    let mut image_depth = 0;
    let mut depth = 0;
    let mut paragraph = None;
    let mut candidate = None::<ImageReference>;
    for (event, range) in parser(markdown).into_offset_iter() {
        if matches!(&event, Event::End(_)) {
            depth -= 1;
        }
        match &event {
            Event::Start(Tag::Paragraph) if depth == 0 => paragraph = Some(range.clone()),
            Event::End(TagEnd::Paragraph) if depth == 0 => {
                if let Some(image) = candidate.take() {
                    resources.images.push(image);
                }
                paragraph = None;
            }
            Event::Start(Tag::Image { dest_url, .. }) if depth == 1 => {
                if let Some(block) = &paragraph
                    && markdown[block.start..range.start].trim_ascii().is_empty()
                    && markdown[range.end..block.end].trim_ascii().is_empty()
                    && let Some(target) = local_relative_target(dest_url)
                    && !target.is_empty()
                    && !target.contains(':')
                {
                    candidate = Some(ImageReference {
                        ordinal: resources.image_count,
                        target: target.replace('\\', "/"),
                        has_alt: false,
                    });
                }
            }
            Event::Text(_)
            | Event::Code(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::End(TagEnd::Image | TagEnd::Link)
                if depth >= 2 =>
            {
                if let Some(image) = &mut candidate {
                    image.has_alt = true;
                }
            }
            _ => {}
        }
        if matches!(&event, Event::Start(_)) {
            depth += 1;
        }
        match event {
            Event::Start(Tag::Image { .. }) => {
                image_depth += 1;
                resources.image_count += 1;
            }
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
                    resources.links.push(link);
                }
            }
            event => {
                if let Some(MarkdownLink { label, .. }) = &mut active {
                    append_link_label(label, &event);
                }
            }
        }
    }
    resources
}

fn append_link_label(label: &mut String, event: &Event<'_>) {
    match event {
        Event::Text(text) | Event::Code(text) | Event::InlineHtml(text) => label.push_str(text),
        Event::SoftBreak | Event::HardBreak => label.push(' '),
        Event::InlineMath(math) => {
            label.push('$');
            label.push_str(math);
            label.push('$');
        }
        Event::FootnoteReference(reference) => {
            label.push('[');
            label.push_str(reference);
            label.push(']');
        }
        _ => {}
    }
}

fn is_unsafe_control(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\t' | '\r' | '\n')
}

fn local_markdown_target(target: &str) -> Option<String> {
    let target = local_relative_target(target)?;
    let file_name = target.rsplit(['/', '\\']).next().unwrap_or_default();
    Path::new(file_name)
        .extension()
        .is_some_and(|extension| extension == "md")
        .then(|| target.into_owned())
}

fn local_relative_target(target: &str) -> Option<Cow<'_, str>> {
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
    Some(target)
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
        let links = TuiMarkdown.resources(source).links;
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
            TuiMarkdown.resources(source).links,
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
            .resources(source)
            .links
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
    fn only_single_top_level_images_with_local_relative_targets_are_placed() {
        let source = r"
before ![same](inline.png)

| Heading |
| --- |
| ![same](table.png) |

> ![same](quote.png)

![same](one.png) ![same](two.png)

![same](https://example.test/web.png)

![same](/absolute.png)

![same](C:drive.png)

![same](%5C%5Cserver/share/unc.png)

![same][ref]

![](empty-alt.png)

[ref]: ../first%20image.png
";
        let resources = TuiMarkdown.resources(source);
        let references: Vec<_> = resources
            .images
            .iter()
            .map(|image| (image.target.as_str(), image.has_alt))
            .collect();
        assert_eq!(
            references,
            [("../first image.png", true), ("empty-alt.png", false)]
        );
        assert!(resources.links.is_empty());
        let document = TuiMarkdown.render_document(source, 30, &resources);
        let lines: Vec<_> = document
            .images
            .iter()
            .map(|placement| document.text().lines[placement.line].to_string())
            .collect();
        assert_eq!(lines, ["[img] same", "[img] empty-alt.png"]);
        assert_eq!(
            document.search_text(),
            TuiMarkdown.render(source, 30).search_text()
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
