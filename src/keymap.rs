use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorMode {
    Normal,
    Prompt,
    View,
    Query,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Abort,
    Save,
    SaveAndAccept,
    MoveLeft { extend: bool },
    MoveRight { extend: bool },
    MoveUp { extend: bool },
    MoveDown { extend: bool },
    MoveWordLeft { extend: bool },
    MoveWordRight { extend: bool },
    MoveLineStart { extend: bool },
    MoveLineEnd { extend: bool },
    MoveDocStart { extend: bool },
    MoveDocEnd { extend: bool },
    PageUp { extend: bool },
    PageDown { extend: bool },
    SelectAll,
    InsertChar(char),
    Indent,
    Dedent,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    ToggleComment,
    Undo,
    Redo,
    ScrollUp(u32),
    ScrollDown(u32),
    StartSearch,
    SearchNext,
    SearchPrev,
    None,
}

#[must_use]
pub const fn map_key(key: KeyEvent, mode: EditorMode) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if matches!(mode, EditorMode::View | EditorMode::Query) {
        return map_view_key(key.code, ctrl, alt, shift);
    }

    match (mode, ctrl, alt, shift, key.code) {
        // Esc: Abort in both modes
        (_, false, false, _, KeyCode::Esc) => Action::Abort,

        // Search
        (_, true, false, false, KeyCode::Char('f')) => Action::StartSearch,

        // Prompt Mode: Enter accepts, Shift+Enter / Alt+Enter / Ctrl+J inserts newline
        (EditorMode::Prompt, false, false, false, KeyCode::Enter)
        | (EditorMode::Prompt, true, false, false, KeyCode::Char('s')) => Action::SaveAndAccept,
        (EditorMode::Prompt, false, false, true, KeyCode::Enter)
        | (EditorMode::Prompt, false, true, false, KeyCode::Enter)
        | (EditorMode::Prompt, true, false, false, KeyCode::Char('j'))
        | (EditorMode::Normal, false, false, _, KeyCode::Enter) => Action::InsertNewline,

        // Normal Mode: Ctrl+S saves, Ctrl+Q quits
        (EditorMode::Normal, true, false, false, KeyCode::Char('s')) => Action::Save,
        (EditorMode::Normal, true, false, false, KeyCode::Char('q')) => Action::Quit,

        // Undo & Redo (VS Code & karet parity)
        (_, true, false, false, KeyCode::Char('z')) => Action::Undo,
        (_, true, false, false, KeyCode::Char('y'))
        | (_, true, false, true, KeyCode::Char('z' | 'Z')) => Action::Redo,

        // Select all
        (_, true, false, false, KeyCode::Char('a')) => Action::SelectAll,

        // Comments
        (_, true, false, false, KeyCode::Char('/' | '7')) => Action::ToggleComment,

        // Horizontal navigation
        (_, false, false, _, KeyCode::Left) => Action::MoveLeft { extend: shift },
        (_, false, false, _, KeyCode::Right) => Action::MoveRight { extend: shift },
        (_, true, false, _, KeyCode::Left) | (_, false, true, _, KeyCode::Left) => {
            Action::MoveWordLeft { extend: shift }
        }
        (_, true, false, _, KeyCode::Right) | (_, false, true, _, KeyCode::Right) => {
            Action::MoveWordRight { extend: shift }
        }

        // Vertical navigation
        (_, false, false, _, KeyCode::Up) => Action::MoveUp { extend: shift },
        (_, false, false, _, KeyCode::Down) => Action::MoveDown { extend: shift },

        // Line bounds
        (_, false, false, _, KeyCode::Home) => Action::MoveLineStart { extend: shift },
        (_, false, false, _, KeyCode::End) => Action::MoveLineEnd { extend: shift },

        // Document bounds
        (_, true, false, _, KeyCode::Home) => Action::MoveDocStart { extend: shift },
        (_, true, false, _, KeyCode::End) => Action::MoveDocEnd { extend: shift },

        // Paging
        (_, false, false, _, KeyCode::PageUp) => Action::PageUp { extend: shift },
        (_, false, false, _, KeyCode::PageDown) => Action::PageDown { extend: shift },

        // Tab & BackTab / Shift+Tab (Indent & Dedent)
        (_, false, false, false, KeyCode::Tab) => Action::Indent,
        (_, false, false, true, KeyCode::Tab) | (_, false, false, _, KeyCode::BackTab) => {
            Action::Dedent
        }
        (_, false, false, _, KeyCode::Char(c)) => Action::InsertChar(c),

        // Deleting
        (_, false, false, false, KeyCode::Backspace) => Action::DeleteBackward,
        (_, false, false, false, KeyCode::Delete) => Action::DeleteForward,
        (_, true, false, _, KeyCode::Backspace) | (_, false, true, _, KeyCode::Backspace) => {
            Action::DeleteWordBackward
        }
        (_, true, false, _, KeyCode::Delete) | (_, false, true, _, KeyCode::Delete) => {
            Action::DeleteWordForward
        }

        _ => Action::None,
    }
}

const fn map_view_key(code: KeyCode, ctrl: bool, alt: bool, shift: bool) -> Action {
    match (ctrl, alt, shift, code) {
        (false, false, _, KeyCode::Esc | KeyCode::Char('q'))
        | (true, false, false, KeyCode::Char('q')) => Action::Quit,

        (false, false, _, KeyCode::Char('j') | KeyCode::Down)
        | (false, false, false, KeyCode::Enter) => Action::ScrollDown(1),
        (false, false, _, KeyCode::Char('k') | KeyCode::Up) => Action::ScrollUp(1),

        (false, false, _, KeyCode::Char(' ' | 'f') | KeyCode::PageDown) => {
            Action::PageDown { extend: false }
        }
        (false, false, _, KeyCode::Char('b') | KeyCode::PageUp) => {
            Action::PageUp { extend: false }
        }
        (false, false, _, KeyCode::Char('d')) | (true, false, false, KeyCode::Char('d')) => {
            Action::ScrollDown(10)
        }
        (false, false, _, KeyCode::Char('u')) | (true, false, false, KeyCode::Char('u')) => {
            Action::ScrollUp(10)
        }

        (false, false, _, KeyCode::Char('g') | KeyCode::Home) => {
            Action::MoveDocStart { extend: false }
        }
        (false, false, _, KeyCode::Char('G') | KeyCode::End) => {
            Action::MoveDocEnd { extend: false }
        }

        (false, false, _, KeyCode::Char('/')) | (true, false, false, KeyCode::Char('f')) => {
            Action::StartSearch
        }
        (false, false, false, KeyCode::Char('n')) => Action::SearchNext,
        (false, false, true, KeyCode::Char('n' | 'N'))
        | (false, false, false, KeyCode::Char('N')) => Action::SearchPrev,

        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn test_prompt_mode_enter_and_shift_enter() {
        assert_eq!(
            map_key(
                press(KeyCode::Enter, KeyModifiers::NONE),
                EditorMode::Prompt
            ),
            Action::SaveAndAccept
        );
        assert_eq!(
            map_key(
                press(KeyCode::Enter, KeyModifiers::SHIFT),
                EditorMode::Prompt
            ),
            Action::InsertNewline
        );
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::ALT), EditorMode::Prompt),
            Action::InsertNewline
        );
        assert_eq!(
            map_key(
                press(KeyCode::Char('j'), KeyModifiers::CONTROL),
                EditorMode::Prompt
            ),
            Action::InsertNewline
        );
    }

    #[test]
    fn test_normal_mode_enter_and_save_quit() {
        assert_eq!(
            map_key(
                press(KeyCode::Enter, KeyModifiers::NONE),
                EditorMode::Normal
            ),
            Action::InsertNewline
        );
        assert_eq!(
            map_key(
                press(KeyCode::Char('s'), KeyModifiers::CONTROL),
                EditorMode::Normal
            ),
            Action::Save
        );
        assert_eq!(
            map_key(
                press(KeyCode::Char('q'), KeyModifiers::CONTROL),
                EditorMode::Normal
            ),
            Action::Quit
        );
    }

    #[test]
    fn test_esc_aborts_in_both_modes() {
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE), EditorMode::Normal),
            Action::Abort
        );
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE), EditorMode::Prompt),
            Action::Abort
        );
    }

    #[test]
    fn test_tab_and_shift_tab_indent_dedent() {
        assert_eq!(
            map_key(press(KeyCode::Tab, KeyModifiers::NONE), EditorMode::Normal),
            Action::Indent
        );
        assert_eq!(
            map_key(press(KeyCode::Tab, KeyModifiers::SHIFT), EditorMode::Normal),
            Action::Dedent
        );
        assert_eq!(
            map_key(
                press(KeyCode::BackTab, KeyModifiers::NONE),
                EditorMode::Normal
            ),
            Action::Dedent
        );
        assert_eq!(
            map_key(
                press(KeyCode::BackTab, KeyModifiers::SHIFT),
                EditorMode::Normal
            ),
            Action::Dedent
        );
    }

    #[test]
    fn test_view_mode_navigation_and_search() {
        assert_eq!(
            map_key(press(KeyCode::Char('q'), KeyModifiers::NONE), EditorMode::View),
            Action::Quit
        );
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE), EditorMode::View),
            Action::Quit
        );
        assert_eq!(
            map_key(press(KeyCode::Char('j'), KeyModifiers::NONE), EditorMode::View),
            Action::ScrollDown(1)
        );
        assert_eq!(
            map_key(press(KeyCode::Char('k'), KeyModifiers::NONE), EditorMode::View),
            Action::ScrollUp(1)
        );
        assert_eq!(
            map_key(press(KeyCode::Char(' '), KeyModifiers::NONE), EditorMode::View),
            Action::PageDown { extend: false }
        );
        assert_eq!(
            map_key(press(KeyCode::Char('b'), KeyModifiers::NONE), EditorMode::View),
            Action::PageUp { extend: false }
        );
        assert_eq!(
            map_key(press(KeyCode::Char('/'), KeyModifiers::NONE), EditorMode::View),
            Action::StartSearch
        );
        assert_eq!(
            map_key(
                press(KeyCode::Char('f'), KeyModifiers::CONTROL),
                EditorMode::View
            ),
            Action::StartSearch
        );
        assert_eq!(
            map_key(press(KeyCode::Char('n'), KeyModifiers::NONE), EditorMode::View),
            Action::SearchNext
        );
        assert_eq!(
            map_key(press(KeyCode::Char('N'), KeyModifiers::SHIFT), EditorMode::View),
            Action::SearchPrev
        );
    }
}
