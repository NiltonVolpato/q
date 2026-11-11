use std::fs::File;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Position, Rect},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardStage {
    RawMode,
    MouseCapture,
    FullyInitialized,
    Restored,
}

pub struct TerminalGuard {
    terminal: Option<Terminal<CrosstermBackend<File>>>,
    viewport_area: Option<Rect>,
    stage: GuardStage,
}

impl TerminalGuard {
    /// Initialize the terminal in raw mode with keyboard flags and inline viewport.
    ///
    /// # Errors
    /// Returns an error if opening `/dev/tty` or setting up terminal modes fails.
    pub fn init(lines: u16) -> anyhow::Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| anyhow::anyhow!("failed to open /dev/tty: {e}"))?;

        enable_raw_mode()?;
        let mut guard = Self {
            terminal: None,
            viewport_area: None,
            stage: GuardStage::RawMode,
        };
        let mut backend = CrosstermBackend::new(tty);
        execute!(
            backend,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        )?;
        guard.stage = GuardStage::MouseCapture;
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(lines),
            },
        )?;
        guard.terminal = Some(terminal);
        guard.stage = GuardStage::FullyInitialized;
        Ok(guard)
    }

    #[must_use]
    pub const fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<File>> {
        self.terminal.as_mut().expect("terminal initialized")
    }

    pub const fn set_viewport_area(&mut self, area: Rect) {
        self.viewport_area = Some(area);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.stage == GuardStage::Restored {
            return;
        }
        if self.stage == GuardStage::FullyInitialized
            && let Some(terminal) = self.terminal.as_mut()
        {
            let _ = terminal.clear();
            if let Some(area) = self.viewport_area {
                let _ = terminal.set_cursor_position(Position::new(area.x, area.y));
            }
            let _ = terminal.show_cursor();
        }
        if matches!(
            self.stage,
            GuardStage::MouseCapture | GuardStage::FullyInitialized
        ) {
            if let Some(terminal) = self.terminal.as_mut() {
                let _ = execute!(
                    terminal.backend_mut(),
                    PopKeyboardEnhancementFlags,
                    DisableMouseCapture
                );
            } else if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                let mut backend = CrosstermBackend::new(tty);
                let _ = execute!(backend, PopKeyboardEnhancementFlags, DisableMouseCapture);
            }
        }
        if self.stage != GuardStage::Restored {
            let _ = disable_raw_mode();
        }
        self.stage = GuardStage::Restored;
    }
}

#[must_use]
pub fn split_viewport(area: Rect) -> (Rect, Rect) {
    let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
        .flex(Flex::End)
        .areas(area);
    [top, bottom].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_viewport_allocates_editor_and_status_bar() {
        let area = Rect::new(0, 5, 80, 10);
        let (editor_area, status_bar_area) = split_viewport(area);

        assert_eq!(editor_area, Rect::new(0, 5, 80, 9));
        assert_eq!(status_bar_area, Rect::new(0, 14, 80, 1));
    }

    #[test]
    fn test_split_viewport_handles_minimal_height() {
        let area = Rect::new(0, 0, 80, 1);
        let (editor_area, status_bar_area) = split_viewport(area);

        assert_eq!(editor_area.height, 0);
        assert_eq!(status_bar_area.height, 1);
    }
}
