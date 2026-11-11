use std::fmt::Write as _;

use karet_core::{LineCol, Range, ThemeRole};
use karet_theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::keymap::EditorMode;

const SEPARATOR: &str = " \u{b7} ";
const CTRL: &str = "\u{2303}";
const SHIFT: &str = "\u{21e7}";
const ENTER: &str = "\u{23ce}";
const ESC: &str = "\u{238b}";

pub struct StatusBarConfig<'a> {
    pub cursor: LineCol,
    pub selection: Option<Range>,
    pub language: &'a str,
    pub file_name: &'a str,
    pub is_dirty: bool,
    pub mode: EditorMode,
    pub in_confirm_discard: bool,
    pub pct_below: u8,
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
            let warning_color = theme.role(ThemeRole::DiagnosticWarning).to_ratatui();
            Line::from(vec![Span::styled(
                "Discard unsaved changes? (y/n)",
                Style::default().fg(warning_color).add_modifier(Modifier::BOLD),
            )])
        } else {
            let left_text = match config.mode {
                EditorMode::Prompt => {
                    format!(
                        "{ENTER}: Accept{SEPARATOR}{SHIFT}{ENTER}: Newline{SEPARATOR}{ESC}: Abort{SEPARATOR}{CTRL}Z: Undo"
                    )
                }
                EditorMode::Normal => {
                    format!(
                        "{CTRL}S: Save{SEPARATOR}{CTRL}Q: Quit{SEPARATOR}{ESC}: Abort{SEPARATOR}{CTRL}Z: Undo"
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

        // Cursor, selection, and percentage below editor
        let mut cursor_label = format!("{}:{}", config.cursor.line + 1, config.cursor.col + 1);
        if let Some(range) = config.selection
            && range.start != range.end
        {
            if range.start.line == range.end.line {
                let count = range.end.col.abs_diff(range.start.col);
                let _ = write!(cursor_label, " ({count} sel)");
            } else {
                let lines = range.end.line.abs_diff(range.start.line) + 1;
                let _ = write!(cursor_label, " ({lines} lines)");
            }
        }
        let _ = write!(cursor_label, " {}%", config.pct_below);
        right_segments.push(cursor_label);

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
            file_name: "test.rs",
            is_dirty: false,
            mode: EditorMode::Normal,
            in_confirm_discard: false,
            pct_below: 80,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.right.to_string().contains("test.rs"));
        assert!(bar.right.to_string().contains("1:6 (5 sel) 80%"));
        assert!(bar.right.to_string().contains("Rust"));
        assert!(bar.left.to_string().contains("\u{2303}S: Save"));
        assert!(!bar.right.to_string().contains("UTF-8"));
    }

    #[test]
    fn test_status_bar_dirty_file_and_confirm_discard() {
        let theme = Theme::dark();
        let config = StatusBarConfig {
            cursor: LineCol::new(0, 0),
            selection: None,
            language: "Markdown",
            file_name: "note.md",
            is_dirty: true,
            mode: EditorMode::Prompt,
            in_confirm_discard: true,
            pct_below: 0,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.right.to_string().contains("note.md*"));
        assert!(bar.right.to_string().contains("1:1 0%"));
        assert_eq!(bar.left.to_string(), "Discard unsaved changes? (y/n)");
    }

    #[test]
    fn test_status_bar_prompt_mode_hints() {
        let theme = Theme::dark();
        let config = StatusBarConfig {
            cursor: LineCol::new(0, 0),
            selection: None,
            language: "Plain Text",
            file_name: "commit.txt",
            is_dirty: false,
            mode: EditorMode::Prompt,
            in_confirm_discard: false,
            pct_below: 50,
        };
        let bar = StatusBar::new(&theme, &config);
        assert!(bar.left.to_string().contains("\u{23ce}: Accept"));
        assert!(bar.left.to_string().contains("\u{21e7}\u{23ce}: Newline"));
        assert!(bar.left.to_string().contains("\u{238b}: Abort"));
        assert!(bar.left.to_string().contains("\u{2303}Z: Undo"));
        assert!(bar.right.to_string().contains("1:1 50%"));
    }
}
