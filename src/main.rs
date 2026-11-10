mod keymap;
mod status_bar;

use std::collections::BTreeSet;
use std::io::{Stdout, stdout};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use karet_core::{FoldRegions, Highlights, LineCol, Range};
use karet_editor::{Editor, EditorState, Fold, editing, resolve_folds};
use karet_syntax::Highlighter;
use karet_text::TextBuffer;
use karet_theme::Theme;
use karet_treesitter::{
    LanguageId, ParserPool, SyntaxTree, language_id_from_injection_name,
};
use keymap::{Action, map_key};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Position, Rect},
};
use status_bar::{StatusBar, StatusBarConfig};

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

    fn analyze(&mut self, text: &str) -> (Option<Highlights>, FoldRegions) {
        if let Ok(tree) = SyntaxTree::parse(&mut self.pool, self.lang, text) {
            let highlights = self.highlighter.highlight(&tree, text).ok();
            let folds = karet_syntax::fold(&tree);
            (highlights, folds)
        } else {
            (None, FoldRegions::default())
        }
    }
}

fn split_viewport(area: Rect) -> (Rect, Rect) {
    let [top, bottom] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
        .flex(Flex::End)
        .areas(area);
    [top, bottom].into()
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    execute!(stdout(), EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(10),
        },
    )?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    viewport_area: Rect,
) -> anyhow::Result<()> {
    terminal.clear()?;
    terminal.set_cursor_position(Position::new(viewport_area.x, viewport_area.y))?;
    terminal.show_cursor()?;
    execute!(stdout(), DisableMouseCapture)?;
    disable_raw_mode()?;
    Ok(())
}

fn toggle_comment_on_current_line(
    buffer: &mut TextBuffer,
    state: &EditorState,
) -> anyhow::Result<()> {
    let line_idx = state.cursor().line;
    let line_str = buffer.line(line_idx as usize).unwrap_or_default();
    let trimmed = line_str.trim_start();
    let indent_len = u32::try_from(line_str.len() - trimmed.len()).unwrap_or(u32::MAX);

    if trimmed.starts_with("// ") {
        let range = Range {
            start: LineCol::new(line_idx, indent_len),
            end: LineCol::new(line_idx, indent_len.saturating_add(3)),
        };
        let edit = editing::insert(range.start, Some(range), buffer.version(), "");
        buffer.apply_simple(&edit.change)?;
    } else {
        let range = Range {
            start: LineCol::new(line_idx, indent_len),
            end: LineCol::new(line_idx, indent_len),
        };
        let edit = editing::insert(range.start, Some(range), buffer.version(), "// ");
        buffer.apply_simple(&edit.change)?;
    }
    Ok(())
}

fn toggle_fold_at_cursor(
    folds: &FoldRegions,
    folded: &mut BTreeSet<u32>,
    state: &mut EditorState,
    buffer: &TextBuffer,
) {
    let cursor_line = state.cursor().line;
    if let Some(r) = folds
        .regions()
        .iter()
        .rfind(|r| r.start <= cursor_line && cursor_line <= r.end)
    {
        let start = r.start;
        if !folded.remove(&start) {
            folded.insert(start);
            if state.cursor().line > start {
                state.goto(buffer, LineCol::new(start, 0));
            }
        }
    }
}

fn handle_motion_action(action: Action, buffer: &TextBuffer, state: &mut EditorState) -> bool {
    match action {
        Action::MoveLeft { extend } => {
            state.move_left(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveRight { extend } => {
            state.move_right(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveUp { extend } => {
            state.move_up(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDown { extend } => {
            state.move_down(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveWordLeft { extend } => {
            state.move_word_left(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveWordRight { extend } => {
            state.move_word_right(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveLineStart { extend } => {
            state.move_line_start(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveLineEnd { extend } => {
            state.move_line_end(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDocStart { extend } => {
            state.move_doc_start(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::MoveDocEnd { extend } => {
            state.move_doc_end(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::PageUp { extend } => {
            state.page_up(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::PageDown { extend } => {
            state.page_down(buffer);
            if !extend {
                state.clear_selection();
            }
            true
        }
        Action::SelectAll => {
            state.select_all(buffer);
            true
        }
        _ => false,
    }
}

fn handle_edit_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
) -> anyhow::Result<bool> {
    match action {
        Action::InsertChar(c) => {
            let edit = editing::insert(
                state.cursor(),
                state.selection_range(),
                buffer.version(),
                &c.to_string(),
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::InsertTab => {
            let edit = editing::insert(
                state.cursor(),
                state.selection_range(),
                buffer.version(),
                "    ",
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::InsertNewline => {
            let edit = editing::newline(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            );
            buffer.apply_simple(&edit.change)?;
            state.goto(buffer, edit.caret);
            Ok(true)
        }
        Action::DeleteBackward => {
            if let Some(edit) = editing::backspace(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteForward => {
            if let Some(edit) = editing::delete_forward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteWordBackward => {
            if let Some(edit) = editing::delete_word_backward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::DeleteWordForward => {
            if let Some(edit) = editing::delete_word_forward(
                state.cursor(),
                state.selection_range(),
                buffer,
                buffer.version(),
            ) {
                buffer.apply_simple(&edit.change)?;
                state.goto(buffer, edit.caret);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        _ => Ok(false),
    }
}

fn handle_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    folds: &FoldRegions,
    folded: &mut BTreeSet<u32>,
) -> anyhow::Result<(bool, bool)> {
    if handle_motion_action(action, buffer, state) {
        return Ok((false, false));
    }
    if handle_edit_action(action, buffer, state)? {
        return Ok((true, false));
    }

    match action {
        Action::Quit => Ok((false, true)),
        Action::Undo => {
            if let Some(applied) = buffer.undo() {
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok((true, false))
            } else {
                Ok((false, false))
            }
        }
        Action::ToggleFold => {
            toggle_fold_at_cursor(folds, folded, state, buffer);
            Ok((false, false))
        }
        Action::ToggleComment => {
            toggle_comment_on_current_line(buffer, state)?;
            Ok((true, false))
        }
        _ => Ok((false, false)),
    }
}

fn handle_mouse_event(
    mouse: MouseEvent,
    buffer: &TextBuffer,
    state: &mut EditorState,
    editor_area: Rect,
    resolved_folds: &[Fold],
    folded: &mut BTreeSet<u32>,
) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(line) = state.gutter_marker_line_at(
                editor_area,
                buffer,
                resolved_folds,
                mouse.column,
                mouse.row,
            ) {
                if !folded.remove(&line) {
                    folded.insert(line);
                }
            } else {
                let pos = state.pos_at(
                    editor_area,
                    buffer,
                    resolved_folds,
                    mouse.column,
                    mouse.row,
                );
                state.set_caret(buffer, pos);
            }
        }
        MouseEventKind::ScrollDown => {
            state.scroll_to_line(state.scroll_line.saturating_add(1));
        }
        MouseEventKind::ScrollUp => {
            state.scroll_to_line(state.scroll_line.saturating_sub(1));
        }
        _ => {}
    }
}

fn run_editor_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<Rect> {
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
    let (mut highlights, mut folds) = syntax_highlighter
        .as_mut()
        .map_or_else(|| (None, FoldRegions::default()), |sh| sh.analyze(&buffer.text()));

    let mut folded: BTreeSet<u32> = BTreeSet::new();
    let mut viewport_area = Rect::default();
    let mut editor_area = Rect::default();

    loop {
        let resolved_folds = resolve_folds(&folds, &folded);

        terminal.draw(|frame| {
            let area = frame.area();
            viewport_area = area;
            let (top, bottom) = split_viewport(area);
            editor_area = top;

            let mut editor_widget = Editor::new(&buffer)
                .theme(&theme)
                .folds(&resolved_folds)
                .focused(true);
            if let Some(ref h) = highlights {
                editor_widget = editor_widget.highlights(h);
            }

            frame.render_stateful_widget(editor_widget, editor_area, &mut state);

            let status_config = StatusBarConfig {
                cursor: state.cursor(),
                selection: state.selection_range(),
                language: "Rust",
                encoding: "UTF-8",
                eol: "LF",
            };
            StatusBar::new(&theme, &status_config).draw(frame, bottom);

            if let Some((x, y)) = state.primary_caret_cell(editor_area, &buffer, &resolved_folds) {
                frame.set_cursor_position(Position::new(x, y));
            }
        })?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let mut text_changed = false;

            match event::read()? {
                Event::Key(key) if key.kind == crossterm::event::KeyEventKind::Press => {
                    let action = map_key(key);
                    let (changed, should_exit) = handle_action(
                        action,
                        &mut buffer,
                        &mut state,
                        &folds,
                        &mut folded,
                    )?;
                    if should_exit {
                        break;
                    }
                    text_changed = changed;
                }
                Event::Mouse(mouse) => {
                    handle_mouse_event(
                        mouse,
                        &buffer,
                        &mut state,
                        editor_area,
                        &resolved_folds,
                        &mut folded,
                    );
                }
                _ => {}
            }

            if text_changed
                && let Some(ref mut sh) = syntax_highlighter
            {
                let (new_highlights, new_folds) = sh.analyze(&buffer.text());
                highlights = new_highlights;
                folds = new_folds;
            }
        }
    }

    Ok(viewport_area)
}

fn main() -> anyhow::Result<()> {
    let mut terminal = init_terminal()?;
    let viewport_area = run_editor_loop(&mut terminal)?;
    restore_terminal(&mut terminal, viewport_area)?;
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
    fn test_cursor_move_without_extend_clears_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();
        let folds = FoldRegions::default();
        let mut folded = BTreeSet::new();

        // Move right without extend
        let (changed, exit) = handle_action(
            Action::MoveRight { extend: false },
            &mut buffer,
            &mut state,
            &folds,
            &mut folded,
        )
        .unwrap();

        assert!(!changed);
        assert!(!exit);
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range();
        assert!(sel.is_none() || sel.unwrap().start == sel.unwrap().end);
    }

    #[test]
    fn test_cursor_move_with_extend_creates_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();
        let folds = FoldRegions::default();
        let mut folded = BTreeSet::new();

        // Move right with extend = true
        handle_action(
            Action::MoveRight { extend: true },
            &mut buffer,
            &mut state,
            &folds,
            &mut folded,
        )
        .unwrap();

        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range().unwrap();
        assert_eq!(sel.start, LineCol::new(0, 0));
        assert_eq!(sel.end, LineCol::new(0, 1));
    }

    #[test]
    fn test_toggle_comment() {
        let mut buffer = TextBuffer::from_text("fn test() {}");
        let mut state = EditorState::new();
        let folds = FoldRegions::default();
        let mut folded = BTreeSet::new();

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &folds,
            &mut folded,
        )
        .unwrap();
        assert_eq!(buffer.text(), "// fn test() {}");

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &folds,
            &mut folded,
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn test() {}");
    }
}
