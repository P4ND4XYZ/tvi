use ratatui::{layout::Alignment, style::Modifier, text::Text};

pub(crate) struct Table {
    pub start: usize,
    pub lines: Vec<TableLine>,
    pub alignments: Vec<Alignment>,
}

pub(crate) enum TableLine {
    Rule(usize),
    Cells(Vec<usize>),
}

// 描画済みの罫線 span だけを認識し、同じ文字がセル本文にあっても区切らない。
pub(super) const BORDER_MARKER: Modifier = Modifier::SLOW_BLINK;

pub(super) fn take_tables(
    text: &mut Text<'_>,
    alignments: impl FnOnce() -> Vec<Vec<Alignment>>,
) -> Vec<Table> {
    let mut tables = Vec::new();
    let mut active = None::<Table>;
    for (index, line) in text.lines.iter_mut().enumerate() {
        let marked: Vec<_> = line
            .spans
            .iter_mut()
            .enumerate()
            .filter_map(|(span_index, span)| {
                if !span.style.add_modifier.contains(BORDER_MARKER) {
                    return None;
                }
                span.style.add_modifier.remove(BORDER_MARKER);
                Some(span_index)
            })
            .collect();
        match marked.as_slice() {
            [span] if line.spans[*span].content.starts_with('┌') => {
                active = Some(Table {
                    start: index,
                    lines: vec![TableLine::Rule(*span)],
                    alignments: Vec::new(),
                });
            }
            [span] if line.spans[*span].content.starts_with('└') => {
                if let Some(mut table) = active.take() {
                    table.lines.push(TableLine::Rule(*span));
                    tables.push(table);
                }
            }
            [span] => {
                if let Some(table) = &mut active {
                    table.lines.push(TableLine::Rule(*span));
                }
            }
            [_, _, ..] => {
                if let Some(table) = &mut active {
                    table.lines.push(TableLine::Cells(marked));
                }
            }
            [] => active = None,
        }
    }
    if tables.is_empty() {
        return tables;
    }
    let alignments = alignments();
    if tables.len() != alignments.len() {
        return Vec::new();
    }
    for (table, alignment) in tables.iter_mut().zip(alignments) {
        table.alignments = alignment;
    }
    tables
}
