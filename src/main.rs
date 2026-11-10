use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use karet_core::Highlights;
use karet_editor::{Editor, EditorState, editing};
use karet_syntax::Highlighter;
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::{
    LanguageId, ParserPool, SyntaxTree, language_id_from_injection_name,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Position, Rect},
    style::Style,
    text::Line,
};
use std::io::stdout;

struct SyntaxHighlighter {
    pool: ParserPool,
    highlighter: Highlighter,
    lang: LanguageId,
}

impl SyntaxHighlighter {
    fn new(lang_label: &str) -> Option<Self> {
        let lang = language_id_from_injection_name(lang_label)?;
        let highlighter = Highlighter::new(lang).ok()?;
        Some(Self {
            pool: ParserPool::new(),
            highlighter,
            lang,
        })
    }

    fn highlight(&mut self, text: &str) -> Option<Highlights> {
        let tree = SyntaxTree::parse(&mut self.pool, self.lang, text).ok()?;
        self.highlighter.highlight(&tree, text).ok()
    }
}

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

    let mut buffer = TextBuffer::from_text(content);
    let mut state = EditorState::new();
    let theme = Theme::dark();
    let mut syntax_highlighter = SyntaxHighlighter::new("rust");
    let mut highlights = syntax_highlighter
        .as_mut()
        .and_then(|sh| sh.highlight(&buffer.text()));

    let status_bar = Line::from("Press Esc to exit").style(Style::new().dim());
    let mut viewport_area = Rect::default();
    let mut editor_area = Rect::default();

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            viewport_area = area;
            let (top, bottom) = split_viewport(area);
            editor_area = top;

            let mut editor_widget = Editor::new(&buffer).theme(&theme).focused(true);
            if let Some(ref h) = highlights {
                editor_widget = editor_widget.highlights(h);
            }

            frame.render_stateful_widget(editor_widget, editor_area, &mut state);
            frame.render_widget(&status_bar, bottom);

            if let Some((x, y)) = state.primary_caret_cell(editor_area, &buffer, &[]) {
                frame.set_cursor_position(Position::new(x, y));
            }
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let mut text_changed = false;

            match event::read()? {
                Event::Key(key) => {
                    if key.kind != crossterm::event::KeyEventKind::Press {
                        continue;
                    }
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Esc) => {
                            break;
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('z')) => {
                            if let Some(applied) = buffer.undo() {
                                if let Some(cursor) = applied.restored_cursor {
                                    state.set_cursor_state(&buffer, cursor);
                                }
                                text_changed = true;
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Left) => {
                            state.move_left(&buffer);
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Right) => {
                            state.move_right(&buffer);
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Up) => {
                            state.move_up(&buffer);
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Down) => {
                            state.move_down(&buffer);
                        }
                        (KeyModifiers::NONE, KeyCode::Home) => {
                            state.move_line_start(&buffer);
                        }
                        (KeyModifiers::NONE, KeyCode::End) => {
                            state.move_line_end(&buffer);
                        }
                        (KeyModifiers::NONE, KeyCode::PageUp) => {
                            state.page_up(&buffer);
                        }
                        (KeyModifiers::NONE, KeyCode::PageDown) => {
                            state.page_down(&buffer);
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            let edit = editing::insert(
                                state.cursor(),
                                state.selection_range(),
                                buffer.version(),
                                &c.to_string(),
                            );
                            buffer.apply_simple(&edit.change)?;
                            state.goto(&buffer, edit.caret);
                            text_changed = true;
                        }
                        (KeyModifiers::NONE, KeyCode::Tab) => {
                            let edit = editing::insert(
                                state.cursor(),
                                state.selection_range(),
                                buffer.version(),
                                "    ",
                            );
                            buffer.apply_simple(&edit.change)?;
                            state.goto(&buffer, edit.caret);
                            text_changed = true;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let edit = editing::newline(
                                state.cursor(),
                                state.selection_range(),
                                &buffer,
                                buffer.version(),
                            );
                            buffer.apply_simple(&edit.change)?;
                            state.goto(&buffer, edit.caret);
                            text_changed = true;
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            if let Some(edit) = editing::backspace(
                                state.cursor(),
                                state.selection_range(),
                                &buffer,
                                buffer.version(),
                            ) {
                                buffer.apply_simple(&edit.change)?;
                                state.goto(&buffer, edit.caret);
                                text_changed = true;
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Delete) => {
                            if let Some(edit) = editing::delete_forward(
                                state.cursor(),
                                state.selection_range(),
                                &buffer,
                                buffer.version(),
                            ) {
                                buffer.apply_simple(&edit.change)?;
                                state.goto(&buffer, edit.caret);
                                text_changed = true;
                            }
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let pos = state.pos_at(editor_area, &buffer, &[], mouse.column, mouse.row);
                        state.set_caret(&buffer, pos);
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll_to_line(state.scroll_line.saturating_add(1));
                    }
                    MouseEventKind::ScrollUp => {
                        state.scroll_to_line(state.scroll_line.saturating_sub(1));
                    }
                    _ => {}
                },
                _ => {}
            }

            if text_changed
                && let Some(ref mut sh) = syntax_highlighter
            {
                highlights = sh.highlight(&buffer.text());
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

    #[test]
    fn test_buffer_editing_operations() {
        let mut buffer = TextBuffer::from_text("hello");
        let edit = editing::insert(
            karet_core::LineCol::new(0, 5),
            None,
            buffer.version(),
            " world",
        );
        buffer.apply_simple(&edit.change).unwrap();
        assert_eq!(buffer.text(), "hello world");
        assert_eq!(edit.caret, karet_core::LineCol::new(0, 11));

        let backspace_edit =
            editing::backspace(edit.caret, None, &buffer, buffer.version()).unwrap();
        buffer.apply_simple(&backspace_edit.change).unwrap();
        assert_eq!(buffer.text(), "hello worl");
        assert_eq!(backspace_edit.caret, karet_core::LineCol::new(0, 10));
    }

    #[test]
    fn test_syntax_highlighter_initialization() {
        let mut highlighter = SyntaxHighlighter::new("rust");
        assert!(highlighter.is_some());
        let hl = highlighter.as_mut().unwrap();
        let spans = hl.highlight("fn main() {}");
        assert!(spans.is_some());
        assert!(!spans.unwrap().all().is_empty());
    }
}
