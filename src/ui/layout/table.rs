use ratatui::{
    layout::Alignment,
    style::Style,
    text::{Line, Span},
};

use crate::{
    app::ViewAnchor,
    markdown::{Table, TableLine},
};

use super::{Glyph, Layout, Row, glyphs, push_glyph};

pub(super) fn wrap<'a>(
    layout: &mut Layout<'a>,
    table: &Table,
    lines: &[Line<'_>],
    search: &'a str,
    ranges: &[std::ops::Range<usize>],
    width: usize,
) -> Option<()> {
    if lines.iter().all(|line| line.width() <= width) {
        return None;
    }
    let count = table.alignments.len();
    let mut natural = vec![1; count];
    let mut minimum = natural.clone();
    let mut prefix = 0;
    let mut prepared = Vec::new();
    for ((line, range), shape) in lines.iter().zip(ranges).zip(&table.lines) {
        let mut offsets = Vec::with_capacity(line.spans.len() + 1);
        let mut start = range.start;
        for span in &line.spans {
            offsets.push(start);
            start += span.content.len();
        }
        offsets.push(start);
        let segment = |from: usize, to: usize| {
            glyphs(
                &line.spans[from..to],
                &search[offsets[from]..offsets[to]],
                offsets[from],
            )
            .collect::<Vec<_>>()
        };
        let (left, body) = match shape {
            TableLine::Rule(span) => {
                let rule = segment(*span, span + 1);
                let divisions: Vec<_> = rule
                    .iter()
                    .enumerate()
                    .filter_map(|(i, g)| (g.symbol != "─").then_some(i))
                    .collect();
                if divisions.len() != count + 1 {
                    return None;
                }
                (*span, Body::Rule { rule, divisions })
            }
            TableLine::Cells(borders) => {
                if borders.len() != count + 1 {
                    return None;
                }
                let mut cells = Vec::with_capacity(count);
                for (column, pair) in borders.windows(2).enumerate() {
                    // 描画側は内容の前後に、それぞれ独立した余白 span を置く。
                    let from = pair[0] + 2;
                    let to = pair[1].checked_sub(1)?;
                    if from > to {
                        return None;
                    }
                    let content = segment(from, to);
                    natural[column] = natural[column].max(content.iter().map(|g| g.width).sum());
                    for g in &content {
                        minimum[column] = minimum[column].max(g.width);
                    }
                    cells.push(Slot {
                        content,
                        leading: segment(from - 1, from),
                        trailing: segment(to, to + 1),
                    });
                }
                let vertical = borders
                    .iter()
                    .map(|&i| segment(i, i + 1).into_iter().next())
                    .collect::<Option<Vec<_>>>()?;
                (*borders.first()?, Body::Cells { cells, vertical })
            }
        };
        let prefix_glyphs = segment(0, left);
        let prefix_width = prefix_glyphs.iter().map(|g| g.width).sum();
        prefix = prefix.max(prefix_width);
        prepared.push((prefix_glyphs, prefix_width, body));
    }
    let budget = width.checked_sub(prefix + 3 * count + 1)?;
    let mut widths = minimum;
    let extra = budget.checked_sub(widths.iter().sum())?;
    for _ in 0..extra {
        let Some(column) = (0..count)
            .filter(|&i| widths[i] < natural[i])
            .min_by_key(|&i| widths[i])
        else {
            break;
        };
        widths[column] += 1;
    }

    for (((prefix_glyphs, prefix_width, body), source), range) in
        prepared.into_iter().zip(lines).zip(ranges)
    {
        let first_row = layout.rows.len();
        let make_line = |first| {
            let mut line = Line::default().style(source.style);
            let mut row = Row {
                anchor: ViewAnchor::text(range.start),
                cells: Vec::new(),
            };
            let mut x = 0;
            for g in &prefix_glyphs {
                add(&mut line, &mut row, &mut x, g, first);
            }
            spaces(&mut line, &mut x, prefix - prefix_width, Style::default());
            (line, row, x)
        };
        match body {
            Body::Rule { rule, divisions } => {
                let (mut line, mut row, mut x) = make_line(true);
                add(&mut line, &mut row, &mut x, &rule[0], true);
                for (column, pair) in divisions.windows(2).enumerate() {
                    let start = pair[0] + 1;
                    let end = (start + widths[column] + 2).min(pair[1]);
                    for g in &rule[start..end] {
                        add(&mut line, &mut row, &mut x, g, true);
                    }
                    for _ in end - start..widths[column] + 2 {
                        push_glyph(&mut line, "─", rule[0].style);
                        x += 1;
                    }
                    add(&mut line, &mut row, &mut x, &rule[pair[1]], true);
                }
                layout.text.lines.push(line);
                layout.rows.push(row);
            }
            Body::Cells { cells, vertical } => {
                let mut remaining: Vec<_> =
                    cells.iter().map(|cell| cell.content.as_slice()).collect();
                let mut first = true;
                loop {
                    let (mut line, mut row, mut x) = make_line(first);
                    if !first
                        && let Some(byte) = remaining
                            .iter()
                            .find_map(|cell| cell.first().map(|g| g.start))
                    {
                        row.anchor = ViewAnchor::text(byte);
                    }
                    add(&mut line, &mut row, &mut x, &vertical[0], first);
                    for (i, rest) in remaining.iter_mut().enumerate() {
                        let width = widths[i];
                        let mut used = 0;
                        let end = rest
                            .iter()
                            .take_while(|g| {
                                used += g.width;
                                used <= width
                            })
                            .count();
                        let (chunk, tail) = rest.split_at(end);
                        *rest = tail;
                        let slack = width - chunk.iter().map(|g| g.width).sum::<usize>();
                        let left = match table.alignments[i] {
                            Alignment::Left => 0,
                            Alignment::Center => slack / 2,
                            Alignment::Right => slack,
                        };
                        padding(
                            &mut line,
                            &mut row,
                            &mut x,
                            left + 1,
                            &cells[i].leading,
                            first,
                        );
                        for g in chunk {
                            add(&mut line, &mut row, &mut x, g, true);
                        }
                        padding(
                            &mut line,
                            &mut row,
                            &mut x,
                            slack - left + 1,
                            &cells[i].trailing,
                            first,
                        );
                        add(&mut line, &mut row, &mut x, &vertical[i + 1], first);
                    }
                    layout.text.lines.push(line);
                    layout.rows.push(row);
                    if remaining.iter().all(|cell| cell.is_empty()) {
                        break;
                    }
                    first = false;
                }
            }
        }
        layout
            .reflowed
            .push((range.clone(), first_row..layout.rows.len()));
    }
    Some(())
}

struct Slot<'a> {
    content: Vec<Glyph<'a>>,
    leading: Vec<Glyph<'a>>,
    trailing: Vec<Glyph<'a>>,
}

enum Body<'a> {
    Rule {
        rule: Vec<Glyph<'a>>,
        divisions: Vec<usize>,
    },
    Cells {
        cells: Vec<Slot<'a>>,
        vertical: Vec<Glyph<'a>>,
    },
}

fn add<'a>(line: &mut Line<'a>, row: &mut Row, x: &mut usize, g: &Glyph<'a>, mapped: bool) {
    if mapped {
        row.cells.push(super::Cell {
            bytes: g.start..g.end,
            columns: *x..*x + g.width,
        });
    }
    push_glyph(line, g.symbol, g.style);
    *x += g.width;
}

fn padding(
    line: &mut Line<'_>,
    row: &mut Row,
    x: &mut usize,
    size: usize,
    source: &[Glyph<'_>],
    first: bool,
) {
    let style = source.first().map_or(Style::default(), |g| g.style);
    let n = size.min(source.len());
    // 元の行に存在しない埋め草や複製した罫線には、検索バイトを割り当てない。
    if first {
        for (i, g) in source[..n].iter().enumerate() {
            row.cells.push(super::Cell {
                bytes: g.start..g.end,
                columns: *x + i..*x + i + 1,
            });
        }
    }
    spaces(line, x, size, style);
}

fn spaces(line: &mut Line<'_>, x: &mut usize, n: usize, style: Style) {
    if n != 0 {
        line.spans.push(Span::styled(" ".repeat(n), style));
        *x += n;
    }
}
