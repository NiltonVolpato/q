use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Position,
};
use ratatui_code_editor::editor::Editor;
use ratatui_code_editor::theme::vesper;
use std::io::stdout;

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
    let mut editor_area = ratatui::layout::Rect::default();

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            editor_area = area;
            frame.render_widget(&editor, editor_area);

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

    execute!(stdout(), DisableMouseCapture)?;
    disable_raw_mode()?;
    println!();

    Ok(())
}
