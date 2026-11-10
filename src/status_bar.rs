use std::fmt::Write as _;

use karet_core::{LineCol, Range, ThemeRole};
use karet_theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::keymap::EditorMode;

const SEPARATOR: &str = " \u{b7} ";

pub struct StatusBarConfig<'a> {
    pub cursor: LineCol,
    pub selection: Option<Range>,
    pub language: &'a str,
    pub encoding: &'a str,
    pub eol: &'a str,
    pub file_name: &'a str,
    pub is_dirty: bool,
    pub mode: EditorMode,
    pub in_confirm_discard: bool,
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

        // Left hints or discard confirmation prompt
        let left = if config.in_confirm_discard {
            Line::from(vec![Span::styled(
                "Discard unsaved changes? (y/n)",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )])
        } else {
            let left_text = match config.mode {
                EditorMode::Prompt => {
                    format!(
                        "Enter: Accept{SEPARATOR}Shift+Enter: Newline{SEPARATOR}Esc: Abort{SEPARATOR}^Z: Undo{SEPARATOR}^F: Fold"
                    )
                }
                EditorMode::Normal => {
                    format!(
                        "^S: Save{SEPARATOR}^Q: Quit{SEPARATOR}Esc: Abort{SEPARATOR}^Z: Undo{SEPARATOR}^F: Fold{SEPARATOR}^/: Comment"
                    )
                }
            };
            Line::from(vec![Span::raw(left_text)])
        };

        // Right details
        let mut right_segments: Vec<String> = Vec::new();

        // File name & dirty marker
        let file_label = if config.is_dirty {
            format!("{}*", config.file_name)
        } else {
            config.file_name.to_owned()
        };
        right_segments.push(file_label);

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
        let right_width = self.right_width().min(area.width);
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
            file_name: "test.rs",
            is_dirty: false,
            mode: EditorMode::Normal,
            in_confirm_discard: false,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.right.to_string().contains("test.rs"));
        assert!(bar.right.to_string().contains("Ln 1, Col 6 (5 selected)"));
        assert!(bar.right.to_string().contains("UTF-8"));
        assert!(bar.right.to_string().contains("Rust"));
        assert!(bar.left.to_string().contains("^S: Save"));
    }

    #[test]
    fn test_status_bar_dirty_file_and_confirm_discard() {
        let theme = Theme::dark();
        let config = StatusBarConfig {
            cursor: LineCol::new(0, 0),
            selection: None,
            language: "Markdown",
            encoding: "UTF-8",
            eol: "LF",
            file_name: "note.md",
            is_dirty: true,
            mode: EditorMode::Prompt,
            in_confirm_discard: true,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.right.to_string().contains("note.md*"));
        assert_eq!(bar.left.to_string(), "Discard unsaved changes? (y/n)");
    }

    #[test]
    fn test_status_bar_prompt_mode_hints() {
        let theme = Theme::dark();
        let config = StatusBarConfig {
            cursor: LineCol::new(0, 0),
            selection: None,
            language: "Plain Text",
            encoding: "UTF-8",
            eol: "LF",
            file_name: "commit.txt",
            is_dirty: false,
            mode: EditorMode::Prompt,
            in_confirm_discard: false,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.left.to_string().contains("Enter: Accept"));
        assert!(bar.left.to_string().contains("Shift+Enter: Newline"));
        assert!(bar.left.to_string().contains("Esc: Abort"));
    }
}
