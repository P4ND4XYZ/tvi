use std::borrow::Cow;

use ratatui::text::{Line, Span, Text};

use super::{MarkdownRenderer, RenderedMarkdown};

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
}

fn is_unsafe_control(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\t' | '\r' | '\n')
}

#[cfg(test)]
mod tests {
    use super::*;

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
