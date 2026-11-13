use std::path::Path;

use karet_core::{LineCol, Range, TextEdit};
use karet_editor::{
    EditorState, Motion, editing,
    editing::LineComment,
    editing::autopair::{self, AutoClose, AutoPairs},
};
use karet_filetype::{CommentTokens, all_file_types, file_type_for_path};
use karet_text::TextBuffer;
use karet_treesitter::{LanguageId, injections_query, language_id_from_injection_name};

use crate::args::OutputDestination;
use crate::keymap::Action;
use crate::syntax::SyntaxState;

fn tokens_for_language(lang: LanguageId) -> CommentTokens {
    all_file_types()
        .iter()
        .find(|ft| ft.grammar().and_then(language_id_from_injection_name) == Some(lang))
        .map_or(
            CommentTokens::NONE,
            karet_filetype::FileType::comment_tokens,
        )
}

fn probe_comment_pos(buffer: &TextBuffer, range: Range) -> LineCol {
    let from = range.start.col as usize;
    let col = buffer.line(range.start.line as usize).and_then(|text| {
        let indices: Vec<usize> = text
            .chars()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .collect();
        indices
            .iter()
            .copied()
            .find(|&i| i >= from)
            .or_else(|| indices.iter().copied().take_while(|&i| i < from).last())
            .and_then(|i| u32::try_from(i).ok())
    });
    LineCol::new(range.start.line, col.unwrap_or(range.start.col))
}

fn comment_tokens_at(
    destination: &OutputDestination,
    buffer: &TextBuffer,
    pos: LineCol,
    syntax_state: Option<&SyntaxState>,
) -> CommentTokens {
    let own = match destination {
        OutputDestination::File(path) => file_type_for_path(path).comment_tokens(),
        OutputDestination::Stdout => CommentTokens::NONE,
    };
    let Some(syntax) = syntax_state else {
        return own;
    };
    let Some(tree) = syntax.tree() else {
        return own;
    };
    if injections_query(syntax.lang()).is_none() {
        return own;
    }
    let Ok(byte) = buffer.line_col_to_byte(pos) else {
        return own;
    };
    let layers = tree.layers_at(byte);
    let host = layers
        .iter()
        .rposition(|layer| {
            layer
                .named_ancestors_at(byte)
                .iter()
                .any(|node| node.kind.contains("comment"))
        })
        .unwrap_or(0);
    layers
        .iter()
        .skip(host)
        .map(|layer| layer.language())
        .map(|lang| {
            if lang == syntax.lang() {
                own
            } else {
                tokens_for_language(lang)
            }
        })
        .find(|tokens| !tokens.is_empty())
        .unwrap_or(own)
}

fn apply_change(
    buffer: &mut TextBuffer,
    syntax_state: &mut Option<SyntaxState>,
    change: &karet_core::Change,
    auto_closers: Option<&mut Vec<LineCol>>,
) -> anyhow::Result<()> {
    let applied = buffer.apply_simple(change)?;
    if let Some(syn) = syntax_state {
        syn.record_edits(&applied.edits);
    }
    if let Some(closers) = auto_closers {
        remap_auto_closers(closers, &change.edits);
    }
    Ok(())
}

fn apply_single_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    edit: &editing::Edit,
    auto_closers: Option<&mut Vec<LineCol>>,
) -> anyhow::Result<bool> {
    apply_change(buffer, syntax_state, &edit.change, auto_closers)?;
    state.goto(buffer, edit.caret);
    Ok(true)
}

fn apply_opt_single_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    edit: Option<editing::Edit>,
    auto_closers: Option<&mut Vec<LineCol>>,
) -> anyhow::Result<bool> {
    edit.map_or_else(
        || Ok(false),
        |e| apply_single_edit(buffer, state, syntax_state, &e, auto_closers),
    )
}

fn apply_multi_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    multi: editing::MultiEdit,
    auto_closers: Option<&mut Vec<LineCol>>,
) -> anyhow::Result<bool> {
    apply_change(buffer, syntax_state, &multi.change, auto_closers)?;
    state.set_cursor_state(buffer, multi.cursors);
    Ok(true)
}

fn apply_opt_multi_edit(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    syntax_state: &mut Option<SyntaxState>,
    multi: Option<editing::MultiEdit>,
    auto_closers: Option<&mut Vec<LineCol>>,
) -> anyhow::Result<bool> {
    multi.map_or_else(
        || Ok(false),
        |m| apply_multi_edit(buffer, state, syntax_state, m, auto_closers),
    )
}

pub fn toggle_comment(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
) -> anyhow::Result<bool> {
    let pos = probe_comment_pos(buffer, state.cursors().primary().range());
    let tokens = comment_tokens_at(destination, buffer, pos, syntax_state.as_ref());
    let token = match (tokens.line(), tokens.block()) {
        (Some(prefix), _) => LineComment::Prefix(prefix),
        (None, Some((open, close))) => LineComment::Wrap { open, close },
        (None, None) => return Ok(false),
    };
    let multi = editing::toggle_line_comment(buffer, state.cursors(), buffer.version(), token);
    apply_opt_multi_edit(buffer, state, syntax_state, multi, None)
}

#[must_use]
pub fn auto_pairs_for_destination(destination: &OutputDestination) -> &'static [(char, char)] {
    match destination {
        OutputDestination::File(path) => file_type_for_path(path).auto_pairs(),
        OutputDestination::Stdout => file_type_for_path(Path::new("prompt.txt")).auto_pairs(),
    }
}

#[must_use]
pub fn resolve_auto_pairs<'a>(
    destination: &OutputDestination,
    syntax_state: Option<&'a SyntaxState>,
) -> AutoPairs<'a> {
    AutoPairs {
        pairs: auto_pairs_for_destination(destination),
        brackets: AutoClose::LanguageDefined,
        quotes: AutoClose::LanguageDefined,
        highlights: syntax_state.and_then(|s| s.rendered_highlights.as_ref()),
    }
}

/// Update tracked auto-closer positions across `edits`, dropping any closer
/// whose character was deleted or replaced.
fn remap_auto_closers(auto_closers: &mut Vec<LineCol>, edits: &[TextEdit]) {
    auto_closers.retain(|pos| !edits.iter().any(|e| e.range.start <= *pos && *pos < e.range.end));
    for pos in auto_closers.iter_mut() {
        *pos = editing::map_position(*pos, edits);
    }
    auto_closers.sort_unstable();
    auto_closers.dedup();
}

pub fn handle_insert_char(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
    c: char,
) -> anyhow::Result<bool> {
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    if let Some(typed) = autopair::type_char(
        buffer,
        state.cursors(),
        buffer.version(),
        c,
        &rules,
        auto_closers,
    ) {
        *auto_closers = typed.closers;
        if let Some(edit) = typed.edit {
            apply_multi_edit(buffer, state, syntax_state, edit, None)
        } else {
            state.set_cursor_state(buffer, typed.cursors);
            Ok(true)
        }
    } else {
        let edit = editing::insert(
            state.cursor(),
            state.selection_range(),
            buffer.version(),
            &c.to_string(),
        );
        apply_single_edit(buffer, state, syntax_state, &edit, Some(auto_closers))
    }
}

pub fn handle_insert_newline(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    auto_closers.clear();
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    let indent_fn = |range: Range| {
        syntax_state
            .as_ref()
            .and_then(|syn| syn.compute_newline_indent_range(buffer, range))
    };
    if let Some(edit) = autopair::newline_indented(
        buffer,
        state.cursors(),
        buffer.version(),
        &rules,
        "    ",
        &indent_fn,
    ) {
        apply_multi_edit(buffer, state, syntax_state, edit, None)
    } else {
        let indent = syntax_state.as_ref().and_then(|syn| {
            syn.compute_newline_indent(buffer, state.cursor(), state.selection_range())
        });
        let edit = editing::newline_indented(
            state.cursor(),
            state.selection_range(),
            buffer,
            buffer.version(),
            indent.as_deref(),
        );
        apply_single_edit(buffer, state, syntax_state, &edit, None)
    }
}

pub fn handle_backspace(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    let rules = resolve_auto_pairs(destination, syntax_state.as_ref());
    if let Some(edit) = autopair::backspace(buffer, state.cursors(), buffer.version(), &rules) {
        apply_multi_edit(buffer, state, syntax_state, edit, Some(auto_closers))
    } else {
        let edit = editing::backspace(
            state.cursor(),
            state.selection_range(),
            buffer,
            buffer.version(),
        );
        apply_opt_single_edit(buffer, state, syntax_state, edit, Some(auto_closers))
    }
}

pub fn handle_motion_action(
    action: Action,
    buffer: &TextBuffer,
    state: &mut EditorState,
    auto_closers: &mut Vec<LineCol>,
) -> bool {
    let (motion, extend) = match action {
        Action::MoveLeft { extend } => (Motion::Left, extend),
        Action::MoveRight { extend } => (Motion::Right, extend),
        Action::MoveUp { extend } => (Motion::Up, extend),
        Action::MoveDown { extend } => (Motion::Down, extend),
        Action::MoveWordLeft { extend } => (Motion::WordLeft, extend),
        Action::MoveWordRight { extend } => (Motion::WordRight, extend),
        Action::MoveLineStart { extend } => (Motion::LineStart, extend),
        Action::MoveLineEnd { extend } => (Motion::LineEnd, extend),
        Action::MoveDocStart { extend } => (Motion::DocStart, extend),
        Action::MoveDocEnd { extend } => (Motion::DocEnd, extend),
        Action::PageUp { extend } => (Motion::PageUp, extend),
        Action::PageDown { extend } => (Motion::PageDown, extend),
        Action::SelectAll => {
            state.select_all(buffer);
            auto_closers.clear();
            return true;
        }
        _ => return false,
    };
    state.move_carets(buffer, motion, extend);
    auto_closers.clear();
    true
}

pub fn handle_edit_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    match action {
        Action::InsertChar(c) => {
            handle_insert_char(buffer, state, destination, syntax_state, auto_closers, c)
        }
        Action::Indent => {
            auto_closers.clear();
            apply_opt_multi_edit(
                buffer,
                state,
                syntax_state,
                editing::indent(buffer, state.cursors(), buffer.version(), "    ", 4),
                None,
            )
        }
        Action::Dedent => {
            auto_closers.clear();
            apply_opt_multi_edit(
                buffer,
                state,
                syntax_state,
                editing::dedent(buffer, state.cursors(), buffer.version(), "    ", 4),
                None,
            )
        }
        Action::InsertNewline => {
            handle_insert_newline(buffer, state, destination, syntax_state, auto_closers)
        }
        Action::DeleteBackward => {
            handle_backspace(buffer, state, destination, syntax_state, auto_closers)
        }
        Action::DeleteForward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_forward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
                None,
            )
        }
        Action::DeleteWordBackward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_word_backward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
                None,
            )
        }
        Action::DeleteWordForward => {
            auto_closers.clear();
            apply_opt_single_edit(
                buffer,
                state,
                syntax_state,
                editing::delete_word_forward(
                    state.cursor(),
                    state.selection_range(),
                    buffer,
                    buffer.version(),
                ),
                None,
            )
        }
        _ => Ok(false),
    }
}

pub fn handle_action(
    action: Action,
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    destination: &OutputDestination,
    syntax_state: &mut Option<SyntaxState>,
    auto_closers: &mut Vec<LineCol>,
) -> anyhow::Result<bool> {
    if handle_motion_action(action, buffer, state, auto_closers) {
        return Ok(false);
    }
    if handle_edit_action(
        action,
        buffer,
        state,
        destination,
        syntax_state,
        auto_closers,
    )? {
        return Ok(true);
    }

    match action {
        Action::Undo => {
            auto_closers.clear();
            if let Some(applied) = buffer.undo() {
                if let Some(syn) = syntax_state {
                    syn.record_edits(&applied.edits);
                }
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::Redo => {
            auto_closers.clear();
            if let Some(applied) = buffer.redo() {
                if let Some(syn) = syntax_state {
                    syn.record_edits(&applied.edits);
                }
                if let Some(cursor) = applied.restored_cursor {
                    state.set_cursor_state(buffer, cursor);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Action::ToggleComment => {
            auto_closers.clear();
            toggle_comment(buffer, state, destination, syntax_state)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use karet_core::LineCol;
    use karet_treesitter::languages;

    use super::*;

    #[test]
    fn test_cursor_move_with_extend_creates_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();

        let changed = handle_action(
            Action::MoveRight { extend: true },
            &mut buffer,
            &mut state,
            &OutputDestination::Stdout,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(!changed);
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range().unwrap();
        assert_eq!(sel.start, LineCol::new(0, 0));
        assert_eq!(sel.end, LineCol::new(0, 1));
    }

    #[test]
    fn test_cursor_move_without_extend_clears_selection() {
        let mut buffer = TextBuffer::from_text("line 1\nline 2");
        let mut state = EditorState::new();

        let changed = handle_action(
            Action::MoveRight { extend: false },
            &mut buffer,
            &mut state,
            &OutputDestination::Stdout,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(!changed);
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        let sel = state.selection_range();
        assert!(sel.is_none() || sel.unwrap().start == sel.unwrap().end);
    }

    #[test]
    fn test_indent_and_dedent() {
        let mut buffer = TextBuffer::from_text("hello\nworld");
        let mut state = EditorState::new();
        let dest = OutputDestination::Stdout;

        handle_action(
            Action::Indent,
            &mut buffer,
            &mut state,
            &dest,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "    hello\nworld");

        handle_action(
            Action::Dedent,
            &mut buffer,
            &mut state,
            &dest,
            &mut None,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "hello\nworld");
    }

    #[test]
    fn test_toggle_comment() {
        let mut buffer = TextBuffer::from_text("fn main() {\n    let x = 1;\n}\n");
        let mut state = EditorState::new();
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), &buffer.text()));

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "// fn main() {\n    let x = 1;\n}\n");

        handle_action(
            Action::ToggleComment,
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(buffer.text(), "fn main() {\n    let x = 1;\n}\n");
    }

    #[test]
    fn test_multi_language_comment_toggling() {
        let mut py_buffer = TextBuffer::from_text("def hello(): pass");
        let mut py_state = EditorState::new();
        let py_dest = OutputDestination::File(PathBuf::from("test.py"));
        let mut py_syntax = Some(SyntaxState::new(
            languages::PYTHON.unwrap(),
            &py_buffer.text(),
        ));

        handle_action(
            Action::ToggleComment,
            &mut py_buffer,
            &mut py_state,
            &py_dest,
            &mut py_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(py_buffer.text(), "# def hello(): pass");

        handle_action(
            Action::ToggleComment,
            &mut py_buffer,
            &mut py_state,
            &py_dest,
            &mut py_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(py_buffer.text(), "def hello(): pass");

        let mut html_buffer = TextBuffer::from_text("<div>hello</div>");
        let mut html_state = EditorState::new();
        let html_dest = OutputDestination::File(PathBuf::from("index.html"));
        let mut html_syntax = Some(SyntaxState::new(
            languages::HTML.unwrap(),
            &html_buffer.text(),
        ));

        handle_action(
            Action::ToggleComment,
            &mut html_buffer,
            &mut html_state,
            &html_dest,
            &mut html_syntax,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(html_buffer.text(), "<!-- <div>hello</div> -->");
    }

    #[test]
    fn test_closing_bracket_auto_dedent() {
        let text = "fn test() {\n        \n";
        let mut buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        state.set_caret(&buffer, LineCol::new(1, 8));
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), text));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('}'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();

        assert_eq!(buffer.text(), "fn test() {\n}\n");
        assert_eq!(state.cursor(), LineCol::new(1, 1));
    }

    #[test]
    fn test_closing_bracket_dedent_without_preexisting_pair() {
        let text = "impl Foo {\n    bar()\n        ";
        let mut buffer = TextBuffer::from_text(text);
        let mut state = EditorState::new();
        state.set_caret(&buffer, LineCol::new(2, 8));
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), text));
        let mut closers = Vec::new();

        handle_action(
            Action::InsertChar('}'),
            &mut buffer,
            &mut state,
            &dest,
            &mut syntax,
            &mut closers,
        )
        .unwrap();

        assert_eq!(buffer.text(), "impl Foo {\n    bar()\n}");
        assert_eq!(state.cursor(), LineCol::new(2, 1));
    }

    fn run_actions(
        actions: &[Action],
        dest: &OutputDestination,
        syntax: &mut Option<SyntaxState>,
    ) -> (TextBuffer, EditorState, Vec<LineCol>) {
        let mut buffer = TextBuffer::from_text("");
        let mut state = EditorState::new();
        let mut closers = Vec::new();
        for &action in actions {
            handle_action(action, &mut buffer, &mut state, dest, syntax, &mut closers).unwrap();
        }
        (buffer, state, closers)
    }

    #[test]
    fn test_autopair_insert_and_step_over() {
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), ""));

        let (buffer, state, closers) =
            run_actions(&[Action::InsertChar('{')], &dest, &mut syntax);
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 1));
        assert_eq!(closers, vec![LineCol::new(0, 1)]);

        let (buffer, state, closers) = run_actions(
            &[Action::InsertChar('{'), Action::InsertChar('}')],
            &dest,
            &mut syntax,
        );
        assert_eq!(buffer.text(), "{}");
        assert_eq!(state.cursor(), LineCol::new(0, 2));
        assert_eq!(closers, []);
    }

    #[test]
    fn test_autopair_empty_backspace() {
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), ""));
        let (buffer, state, _) = run_actions(
            &[Action::InsertChar('{'), Action::DeleteBackward],
            &dest,
            &mut syntax,
        );
        assert_eq!(buffer.text(), "");
        assert_eq!(state.cursor(), LineCol::new(0, 0));
    }

    #[test]
    fn test_autopair_newline_bracket_expansion() {
        let dest = OutputDestination::File(PathBuf::from("test.rs"));
        let mut syntax = Some(SyntaxState::new(languages::RUST.unwrap(), ""));
        let (buffer, state, _) = run_actions(
            &[Action::InsertChar('{'), Action::InsertNewline],
            &dest,
            &mut syntax,
        );
        assert_eq!(buffer.text(), "{\n    \n}");
        assert_eq!(state.cursor(), LineCol::new(1, 4));
    }

    #[test]
    fn test_autopair_step_over_after_typing_inside() {
        let mut syntax = None;
        let (buffer, state, closers) = run_actions(
            &[
                Action::InsertChar('['),
                Action::InsertChar('0'),
                Action::InsertChar(']'),
            ],
            &OutputDestination::Stdout,
            &mut syntax,
        );
        assert_eq!(buffer.text(), "[0]");
        assert_eq!(state.cursor(), LineCol::new(0, 3));
        assert_eq!(closers, []);
    }

    #[test]
    fn test_autopair_step_over_after_backspace_inside() {
        let mut syntax = None;
        let (buffer, state, closers) = run_actions(
            &[
                Action::InsertChar('['),
                Action::InsertChar('a'),
                Action::DeleteBackward,
                Action::InsertChar(']'),
            ],
            &OutputDestination::Stdout,
            &mut syntax,
        );
        assert_eq!(buffer.text(), "[]");
        assert_eq!(state.cursor(), LineCol::new(0, 2));
        assert_eq!(closers, []);
    }
}
