use karet_core::{BytePos, Decoration, DecorationKind, Range, Span, ThemeRole};
use karet_text::TextBuffer;

#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub matches: Vec<Range>,
    pub current: Option<usize>,
}

impl SearchState {
    pub const fn open(&mut self) {
        self.active = true;
    }

    pub const fn close(&mut self) {
        self.active = false;
    }

    pub fn update_matches(&mut self, buffer: &TextBuffer, anchor_line: u32) {
        self.matches.clear();
        self.current = None;

        if self.query.is_empty() {
            return;
        }

        let end = BytePos(buffer.len_bytes());
        let mut pos = BytePos(0);
        while pos < end {
            let Some(found) = buffer.find(&self.query, Span::new(pos, end)) else {
                break;
            };
            let match_end = BytePos(found.0 + self.query.len());
            let start_lc = buffer.byte_to_line_col(found);
            let end_lc = buffer.byte_to_line_col(match_end);
            self.matches.push(Range::new(start_lc, end_lc));
            pos = match_end;
        }

        if !self.matches.is_empty() {
            self.current = self
                .matches
                .iter()
                .position(|r| r.start.line >= anchor_line)
                .or(Some(0));
        }
    }

    #[must_use]
    pub fn current_range(&self) -> Option<Range> {
        self.current.and_then(|idx| self.matches.get(idx).copied())
    }

    pub fn next(&mut self) -> Option<Range> {
        if self.matches.is_empty() {
            return None;
        }
        let next_idx = match self.current {
            Some(idx) => (idx + 1) % self.matches.len(),
            None => 0,
        };
        self.current = Some(next_idx);
        self.current_range()
    }

    pub fn prev(&mut self) -> Option<Range> {
        if self.matches.is_empty() {
            return None;
        }
        let prev_idx = match self.current {
            Some(0) | None => self.matches.len() - 1,
            Some(idx) => idx - 1,
        };
        self.current = Some(prev_idx);
        self.current_range()
    }

    #[must_use]
    pub fn decorations(&self) -> Vec<Decoration> {
        self.matches
            .iter()
            .enumerate()
            .map(|(idx, &range)| {
                let role = if Some(idx) == self.current {
                    ThemeRole::SearchMatchCurrent
                } else {
                    ThemeRole::SearchMatch
                };
                Decoration::new(range, DecorationKind::TextBackground, Some(role))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_find_and_cycle() {
        let buffer = TextBuffer::from_text("alpha beta\ngamma alpha\nalpha");
        let mut search = SearchState::default();
        search.open();
        search.query = "alpha".to_string();
        search.update_matches(&buffer, 1);

        assert_eq!(search.matches.len(), 3);
        assert_eq!(search.current, Some(1));
        assert_eq!(search.current_range().unwrap().start.line, 1);

        assert_eq!(search.next().unwrap().start.line, 2);
        assert_eq!(search.next().unwrap().start.line, 0);
        assert_eq!(search.prev().unwrap().start.line, 2);

        let decs = search.decorations();
        assert_eq!(decs.len(), 3);
        assert_eq!(decs[2].role, Some(ThemeRole::SearchMatchCurrent));
        assert_eq!(decs[0].role, Some(ThemeRole::SearchMatch));
    }

    #[test]
    fn test_search_empty_query_clears() {
        let buffer = TextBuffer::from_text("hello world");
        let mut search = SearchState {
            active: true,
            query: "hello".to_string(),
            ..Default::default()
        };
        search.update_matches(&buffer, 0);
        assert_eq!(search.matches.len(), 1);

        search.query.clear();
        search.update_matches(&buffer, 0);
        assert_eq!(search.matches, [] as [karet_core::Range; 0]);
        assert_eq!(search.current, None);
    }
}
