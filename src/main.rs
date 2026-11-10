use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Position, Rect},
    style::Style,
    text::Line,
};
use ratatui_code_editor::editor::Editor;
use ratatui_code_editor::theme::vesper;
use std::io::stdout;

fn split_viewport(area: Rect) -> (Rect, Rect) {
    let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
        .flex(Flex::End)
        .areas(area);
    (top, bottom)
}

fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(10),
        },
    )?;

    let content = r#"// 10-line inline editor inside shell
// Press Esc to exit
fn main() {
    let message = "Terminal scrollback preserved!";
    println!("{}", message);
    for index in 0..10 {
        println!("Line {}", index);
    }
}"#;

    let mut editor = Editor::new("rust", content, vesper())?;
    editor.set_left_code_padding(1);
    let status_bar = Line::from("Press Esc to exit").style(Style::new().dim());
    let mut viewport_area = ratatui::layout::Rect::default();
    let mut editor_area = ratatui::layout::Rect::default();

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            viewport_area = area;
            let (top, bottom) = split_viewport(area);
            editor_area = top;

            frame.render_widget(&editor, editor_area);
            frame.render_widget(&status_bar, bottom);

            if let Some((x, y)) = editor.get_visible_cursor(&editor_area) {
                frame.set_cursor_position(Position::new(x, y));
            }
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.code == KeyCode::Esc {
                        break;
                    }
                    editor.input(key, &editor_area)?;
                }
                Event::Mouse(mouse) => {
                    editor.mouse(mouse, &editor_area)?;
                }
                _ => {}
            }
        }
    }

    terminal.clear()?;
    terminal.set_cursor_position(Position::new(viewport_area.x, viewport_area.y))?;
    terminal.show_cursor()?;
    execute!(stdout(), DisableMouseCapture)?;
    disable_raw_mode()?;

    Ok(())
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
