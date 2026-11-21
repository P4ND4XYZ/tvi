use std::{collections::HashMap, ops::Range};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, LinkType, Tag, TagEnd};
use ratatui::{style::Modifier, text::Text};

use super::{
    table::BORDER_MARKER,
    tui_markdown::{CODE_CLOSE, CODE_OPEN, DIAGRAM_FENCE, QUOTE, parser},
};

// tui-markdown does not expose offsets. Match its intact event spans in order,
// and use the adapter's structural markers for buffered tables and code blocks.
// Coordinates are in the normalized logical text, before width-dependent layout.
type Position = (usize, usize);

pub(super) fn collect(source: &str, text: &Text<'_>) -> Vec<Option<Position>> {
    let starts: Vec<_> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(byte, _)| byte + 1))
        .filter(|&byte| byte < source.len())
        .collect();
    let mut positions = vec![None; starts.len()];
    let mut display = Display::new(text);
    let mut events = parser(source).into_offset_iter();
    let mut links = Vec::new();
    let mut images = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    let mut metadata = false;
    while let Some((event, range)) = events.next() {
        let first = source_line(&starts, range.start);
        let last = source_line(&starts, range.end.saturating_sub(1));
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                display.code_block(kind, range, &mut events, &starts, &mut positions);
            }
            Event::Start(Tag::Table(_)) => {
                display.table(&mut events, &starts, &mut positions);
            }
            Event::Text(value) | Event::Html(value) => {
                for (index, part) in value.lines().enumerate() {
                    if !part.is_empty() || source[range.clone()].starts_with('\n') {
                        anchor(
                            &mut positions,
                            (first + index).min(last),
                            display.find(part),
                        );
                    }
                }
            }
            Event::Code(value) => {
                if let Some((row, byte)) = display.find(&value) {
                    anchor(&mut positions, first, Some((row, byte)));
                    if first < last {
                        for (index, offset) in code_continuations(&source[range], &value) {
                            anchor(&mut positions, first + index, Some((row, byte + offset)));
                        }
                    }
                }
            }
            event @ (Event::InlineHtml(_) | Event::InlineMath(_)) => {
                let value = match event {
                    Event::InlineHtml(value) => value.into_string(),
                    Event::InlineMath(value) => format!("${value}$"),
                    _ => unreachable!(),
                };
                if let Some((row, byte)) = display.find(&value) {
                    anchor(&mut positions, first, Some((row, byte)));
                    for (index, (offset, _)) in value.match_indices('\n').enumerate() {
                        let offset = offset + 1;
                        if offset < value.len() && first + index < last {
                            anchor(
                                &mut positions,
                                first + index + 1,
                                Some((row, byte + normalized_len(&value[..offset]))),
                            );
                        }
                    }
                }
            }
            Event::DisplayMath(value) => {
                for (index, part) in format!("$${value}$$").lines().enumerate() {
                    anchor(
                        &mut positions,
                        (first + index).min(last),
                        display.find(part),
                    );
                }
            }
            Event::FootnoteReference(label) => {
                anchor(&mut positions, first, display.find(&format!("[{label}]")));
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                anchor(&mut positions, first, display.find(&format!("[{label}]: ")));
            }
            Event::Start(Tag::DefinitionListDefinition) => {
                anchor(&mut positions, first, display.find(": "));
            }
            Event::Start(Tag::Link {
                dest_url,
                link_type,
                ..
            }) => links.push((dest_url, link_type)),
            Event::End(TagEnd::Link) => {
                if let Some((destination, link_type)) = links.pop() {
                    let prefix = display.consume(" (");
                    let position = display.consume(&destination);
                    display.consume(")");
                    if first == last {
                        anchor(&mut positions, first, prefix);
                    } else if let Some(offset) =
                        destination_offset(source, range, &destination, link_type)
                    {
                        anchor(&mut positions, source_line(&starts, offset), position);
                    }
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                anchor(&mut positions, first, display.image_prefix());
                images.push((dest_url, display.cursor));
            }
            Event::End(TagEnd::Image) => {
                if let Some((destination, cursor)) = images.pop()
                    && cursor == display.cursor
                    && !destination.is_empty()
                {
                    display.find(&destination);
                }
            }
            Event::Start(Tag::BlockQuote(Some(_))) => {
                anchor(&mut positions, first, display.alert_header());
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let marker = "#".repeat(level as usize) + " ";
                anchor(&mut positions, first, display.find(&marker));
            }
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                let marker = if let Some(Some(number)) = lists.last_mut() {
                    let marker = format!("{number}. ");
                    *number += 1;
                    marker
                } else {
                    "- ".to_owned()
                };
                anchor(&mut positions, first, display.list_item(&marker));
            }
            Event::Start(Tag::MetadataBlock(_)) => {
                metadata = true;
                anchor(&mut positions, first, display.find("---"));
            }
            Event::End(TagEnd::MetadataBlock(_)) => {
                metadata = false;
                anchor(&mut positions, last, display.find("---"));
            }
            Event::Rule => anchor(&mut positions, first, display.find("---")),
            Event::SoftBreak if !metadata => {
                display.consume(" ");
            }
            Event::HardBreak if !images.is_empty() => {
                display.consume(" ");
            }
            _ => {}
        }
    }
    positions
}

// Prefer the parser's slice. Escapes and entities make inline destinations
// owned; use the start of the destination syntax rather than a title's line.
fn destination_offset(
    source: &str,
    range: Range<usize>,
    destination: &CowStr<'_>,
    link: LinkType,
) -> Option<usize> {
    if let CowStr::Borrowed(value) = destination {
        return (value.as_ptr() as usize)
            .checked_sub(source.as_ptr() as usize)
            .filter(|offset| range.contains(offset));
    }
    if link != LinkType::Inline {
        return None;
    }
    let (label, rest) = source[range.clone()].split_once("](")?;
    Some(range.start + label.len() + 2 + rest.len() - rest.trim_start().len())
}

fn anchor(positions: &mut [Option<Position>], line: usize, position: Option<Position>) {
    if let (Some(slot), Some(position)) = (positions.get_mut(line), position) {
        *slot = Some(slot.map_or(position, |old| old.min(position)));
    }
}

// A code span joins physical lines with a space. Match continuation content
// from the end so repeated words on earlier lines cannot steal its position.
fn code_continuations(raw: &str, rendered: &str) -> Vec<(usize, usize)> {
    let fence = raw.bytes().take_while(|&byte| byte == b'`').count();
    let body = raw[fence..].strip_suffix(&raw[..fence]).unwrap_or(raw);
    let mut end = rendered.len();
    let mut offsets = Vec::new();
    let line_count = body.lines().count();
    for (count, line) in body
        .lines()
        .rev()
        .take(line_count.saturating_sub(1))
        .enumerate()
    {
        let mut line = line.trim_matches(' ');
        let offset = loop {
            if line.is_empty() {
                break None;
            }
            if let Some(offset) = rendered[..end].rfind(line) {
                break Some(offset);
            }
            // Continuation lines in a quote can still contain the source prefix.
            let Some(tail) = line.strip_prefix('>') else {
                break None;
            };
            line = tail.trim_start_matches(' ');
        };
        if let Some(offset) = offset {
            offsets.push((line_count - count - 1, normalized_len(&rendered[..offset])));
            end = offset;
        }
    }
    offsets
}

fn source_line(starts: &[usize], byte: usize) -> usize {
    starts
        .partition_point(|&start| start <= byte)
        .saturating_sub(1)
}

fn normalized_len(value: &str) -> usize {
    value
        .chars()
        .map(|ch| match ch {
            '\t' => 4,
            ch if ch.is_control() => 3,
            ch => ch.len_utf8(),
        })
        .sum()
}

struct Display<'a> {
    text: &'a Text<'a>,
    spans: Vec<(usize, usize, &'a str)>,
    by_text: HashMap<&'a str, Vec<usize>>,
    line_starts: Vec<usize>,
    cursor: usize,
}

impl<'a> Display<'a> {
    fn new(text: &'a Text<'a>) -> Self {
        let mut display = Self {
            text,
            spans: Vec::new(),
            by_text: HashMap::new(),
            line_starts: Vec::new(),
            cursor: 0,
        };
        for (row, line) in text.lines.iter().enumerate() {
            display.line_starts.push(display.spans.len());
            let mut byte = 0;
            for span in &line.spans {
                let value = span.content.as_ref();
                display
                    .by_text
                    .entry(value)
                    .or_default()
                    .push(display.spans.len());
                display.spans.push((row, byte, value));
                byte += normalized_len(value);
            }
        }
        display.line_starts.push(display.spans.len());
        display
    }

    fn next(&self, value: &str) -> Option<usize> {
        let indices = self.by_text.get(value)?;
        indices
            .get(indices.partition_point(|&index| index < self.cursor))
            .copied()
    }

    fn take(&mut self, index: usize) -> (usize, usize, &'a str) {
        self.cursor = index + 1;
        self.spans[index]
    }

    fn find(&mut self, value: &str) -> Option<(usize, usize)> {
        let (row, byte, _) = self.take(self.next(value)?);
        Some((row, byte))
    }

    fn consume(&mut self, expected: &str) -> Option<Position> {
        let &(row, byte, value) = self.spans.get(self.cursor)?;
        if value == expected {
            self.cursor += 1;
            Some((row, byte))
        } else {
            None
        }
    }

    fn alert_header(&mut self) -> Option<Position> {
        let index =
            self.spans[self.cursor..]
                .iter()
                .enumerate()
                .find_map(|(i, &(row, _, _))| {
                    let index = self.cursor + i;
                    let span = &self.text.lines[row].spans[index - self.line_starts[row]];
                    span.style
                        .add_modifier
                        .contains(QUOTE | Modifier::BOLD)
                        .then_some(index)
                })?;
        let (row, byte, _) = self.take(index);
        Some((row, byte))
    }

    fn list_item(&mut self, marker: &str) -> Option<(usize, usize)> {
        let index = self.spans[self.cursor..]
            .iter()
            .position(|&(_, _, value)| {
                let value = value.trim_start();
                value == marker || (marker == "- " && matches!(value, "- [x] " | "- [ ] "))
            })?;
        let (row, byte, _) = self.take(self.cursor + index);
        Some((row, byte))
    }

    fn image_prefix(&mut self) -> Option<(usize, usize)> {
        let index = [self.next("[img]"), self.next("[img] ")]
            .into_iter()
            .flatten()
            .min()?;
        let (row, byte, _) = self.take(index);
        Some((row, byte))
    }

    fn code_block<'src>(
        &mut self,
        kind: CodeBlockKind<'src>,
        range: Range<usize>,
        events: &mut impl Iterator<Item = (Event<'src>, Range<usize>)>,
        starts: &[usize],
        positions: &mut [Option<Position>],
    ) {
        let first = source_line(starts, range.start);
        let last = source_line(starts, range.end.saturating_sub(1));
        let fence = self.code_fence();
        let diagram = fence.is_some_and(|(_, _, marker)| marker == DIAGRAM_FENCE);
        let mut body = Vec::new();
        for (event, range) in events.by_ref() {
            match event {
                Event::Text(value) if !diagram => {
                    let start = source_line(starts, range.start);
                    for (index, part) in value.split_inclusive('\n').enumerate() {
                        if body.last().is_none_or(|&(line, _)| line != start + index) {
                            body.push((start + index, String::new()));
                        }
                        body.last_mut().unwrap().1.push_str(part);
                    }
                }
                Event::End(TagEnd::CodeBlock) => break,
                _ => {}
            }
        }
        let Some((open, byte, _)) = fence else { return };
        if diagram {
            positions[first..=last].fill(Some((open, byte)));
            return;
        }
        let fenced = matches!(kind, CodeBlockKind::Fenced(_));
        if fenced {
            anchor(positions, first, Some((open, byte)));
        }
        if let Some(close) = self.close_fence() {
            // Highlighting may omit a line on failure. Only advance on a
            // matching body row rather than shifting the remaining source lines.
            let mut row = open + 1;
            for (line, value) in &body {
                let value = value.trim_end_matches(['\r', '\n']);
                if row < close && self.ends_with(row, value) {
                    anchor(positions, *line, Some((row, 0)));
                    row += 1;
                }
            }
            if fenced && last > body.last().map_or(first, |&(line, _)| line) {
                anchor(positions, last, Some((close, 0)));
            }
        }
    }

    fn table<'src>(
        &mut self,
        events: &mut impl Iterator<Item = (Event<'src>, Range<usize>)>,
        starts: &[usize],
        positions: &mut [Option<Position>],
    ) {
        let rows = self.table_rows();
        let mut source_rows = Vec::new();
        for (event, range) in events.by_ref() {
            match event {
                Event::Start(Tag::TableHead | Tag::TableRow) => {
                    source_rows.push(source_line(starts, range.start));
                }
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
        if rows.len() == source_rows.len() {
            for (line, row) in source_rows.into_iter().zip(rows) {
                anchor(positions, line, Some((row, 0)));
            }
        }
    }

    fn code_fence(&mut self) -> Option<(usize, usize, &'a str)> {
        let index = self.spans[self.cursor..]
            .iter()
            .position(|(_, _, value)| value.starts_with(CODE_OPEN) || *value == DIAGRAM_FENCE)?;
        Some(self.take(self.cursor + index))
    }

    fn close_fence(&mut self) -> Option<usize> {
        self.find(CODE_CLOSE).map(|(row, _)| row)
    }

    fn ends_with(&self, row: usize, content: &str) -> bool {
        self.text.lines[row].to_string().ends_with(content)
    }

    fn table_rows(&mut self) -> Vec<usize> {
        let start = self
            .spans
            .get(self.cursor)
            .map_or(self.text.lines.len(), |&(row, _, _)| row);
        let mut rows = Vec::new();
        let mut inside = false;
        for (row, line) in self.text.lines.iter().enumerate().skip(start) {
            let mut borders = line
                .spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(BORDER_MARKER));
            if let Some(border) = borders.next() {
                if border.content.starts_with('┌') {
                    inside = true;
                } else if inside && border.content.starts_with('└') {
                    self.cursor = self.line_starts[row + 1];
                    break;
                } else if inside && borders.next().is_some() {
                    rows.push(row);
                }
            } else if inside {
                break;
            }
        }
        rows
    }
}
