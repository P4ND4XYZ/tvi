use std::{ops::Range, sync::Arc};

use crate::pdf::text::Hit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

#[derive(Clone, PartialEq)]
pub(crate) struct PdfSearch {
    pub query: Arc<str>,
    pub direction: Direction,
    pub hits: Arc<[Hit]>,
    pub selected: usize,
    pub highlights: bool,
}

impl PdfSearch {
    pub(crate) fn current(&self) -> Option<Hit> {
        self.hits.get(self.selected).copied()
    }

    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        self.current().map(|_| (self.selected + 1, self.hits.len()))
    }

    pub(crate) fn step(&mut self, reverse: bool) {
        if !self.hits.is_empty() {
            self.selected = step_index(self.selected, self.hits.len(), self.direction, reverse);
            self.highlights = true;
        }
    }
}

fn step_index(index: usize, len: usize, direction: Direction, reverse: bool) -> usize {
    if (direction == Direction::Backward) ^ reverse {
        index.checked_sub(1).unwrap_or(len - 1)
    } else {
        (index + 1) % len
    }
}

#[derive(Default)]
pub(super) struct Search {
    query: String,
    direction: Option<Direction>,
    matches: Vec<Range<usize>>,
    selected: Option<usize>,
    pub highlights: bool,
}

impl Search {
    pub fn submit(&mut self, input: String, direction: Direction, text: &str, offset: usize) {
        if !input.is_empty() {
            self.query = input;
        }
        if self.query.is_empty() {
            return;
        }
        self.direction = Some(direction);
        self.matches = text
            .match_indices(&self.query)
            .map(|(start, matched)| start..start + matched.len())
            .collect();
        let next = match direction {
            Direction::Forward => self.matches.partition_point(|hit| hit.start < offset),
            Direction::Backward => self
                .matches
                .partition_point(|hit| hit.start <= offset)
                .checked_sub(1)
                .unwrap_or_else(|| self.matches.len().saturating_sub(1)),
        };
        self.selected = (!self.matches.is_empty()).then(|| next % self.matches.len());
        self.highlights = self.selected.is_some();
    }

    pub(super) fn refresh(
        &mut self,
        text: &str,
        remap: impl FnOnce(usize) -> usize,
        anchor: usize,
    ) {
        if self.query.is_empty() {
            return;
        }
        let target = self.current().map_or(anchor, |hit| remap(hit.start));
        self.matches = text
            .match_indices(&self.query)
            .map(|(start, matched)| start..start + matched.len())
            .collect();
        let next = self.matches.partition_point(|hit| hit.start < target);
        self.selected = [next.checked_sub(1), Some(next)]
            .into_iter()
            .flatten()
            .filter(|index| *index < self.matches.len())
            .min_by_key(|&index| self.matches[index].start.abs_diff(target));
    }

    pub fn step(&mut self, reverse: bool) {
        if let (Some(index), Some(direction)) = (self.selected, self.direction) {
            self.selected = Some(step_index(index, self.matches.len(), direction, reverse));
            self.highlights = true;
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        Some((self.selected? + 1, self.matches.len()))
    }

    pub fn current(&self) -> Option<&Range<usize>> {
        self.matches.get(self.selected?)
    }

    pub fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_use_starting_offset_and_wrap_at_both_ends() {
        let mut search = Search::default();
        let text = "猫  猫  猫";
        search.submit("猫".into(), Direction::Forward, text, 4);
        assert_eq!(search.current(), Some(&(5..8)));
        search.submit(String::new(), Direction::Backward, text, 4);
        assert_eq!(search.current(), Some(&(0..3)));
        search.step(false);
        assert_eq!(search.current(), Some(&(10..13)));
        search.step(true);
        assert_eq!(search.current(), Some(&(0..3)));
        search.submit(String::new(), Direction::Forward, text, text.len());
        assert_eq!(search.selection(), Some((1, 3)));
    }
}
