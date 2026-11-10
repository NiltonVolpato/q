use std::fmt::Write as _;

use karet_core::{LineCol, Range, ThemeRole};
use karet_theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

const SEPARATOR: &str = " \u{b7} ";

pub struct StatusBarConfig<'a> {
    pub cursor: LineCol,
    pub selection: Option<Range>,
    pub language: &'a str,
    pub encoding: &'a str,
    pub eol: &'a str,
}

pub struct StatusBar<'a> {
    bar_style: Style,
    left: Line<'a>,
    right: Line<'a>,
}

impl<'a> StatusBar<'a> {
    #[must_use]
    pub fn new(theme: &Theme, config: &StatusBarConfig<'a>) -> Self {
        let bar_style = Style::default()
            .bg(theme.role(ThemeRole::StatusBarBackground).to_ratatui())
            .fg(theme.role(ThemeRole::StatusBarForeground).to_ratatui());

        // Left hints
        let left_text = format!(
            "Esc: Quit{SEPARATOR}^Z: Undo{SEPARATOR}^F: Fold{SEPARATOR}^/: Comment"
        );
        let left = Line::from(vec![Span::raw(left_text)]);

        // Right details
        let mut right_segments: Vec<String> = Vec::new();

        // Cursor & selection
        let mut cursor_label = format!("Ln {}, Col {}", config.cursor.line + 1, config.cursor.col + 1);
        if let Some(range) = config.selection
            && range.start != range.end
        {
            if range.start.line == range.end.line {
                let count = range.end.col.abs_diff(range.start.col);
                let _ = write!(cursor_label, " ({count} selected)");
            } else {
                let lines = range.end.line.abs_diff(range.start.line) + 1;
                let _ = write!(cursor_label, " ({lines} lines selected)");
            }
        }
        right_segments.push(cursor_label);

        // Encoding & EOL
        right_segments.push(config.encoding.to_owned());
        right_segments.push(config.eol.to_owned());

        // Language
        right_segments.push(config.language.to_owned());

        let right = Line::from(vec![Span::raw(right_segments.join(SEPARATOR))]);

        Self {
            bar_style,
            left,
            right,
        }
    }

    #[must_use]
    pub fn right_width(&self) -> u16 {
        u16::try_from(self.right.width()).unwrap_or(u16::MAX)
    }

    pub fn draw(self, frame: &mut Frame, area: Rect) {
        let right_width = self.right_width();
        let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)])
            .split(area);

        frame.render_widget(Paragraph::new(self.left).style(self.bar_style), cols[0]);
        frame.render_widget(
            Paragraph::new(self.right)
                .style(self.bar_style)
                .alignment(Alignment::Right),
            cols[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_bar_right_label_single_line_selection() {
        let theme = Theme::dark();
        let config = StatusBarConfig {
            cursor: LineCol::new(0, 5),
            selection: Some(Range {
                start: LineCol::new(0, 0),
                end: LineCol::new(0, 5),
            }),
            language: "Rust",
            encoding: "UTF-8",
            eol: "LF",
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.right.to_string().contains("Ln 1, Col 6 (5 selected)"));
        assert!(bar.right.to_string().contains("UTF-8"));
        assert!(bar.right.to_string().contains("Rust"));
    }
}
