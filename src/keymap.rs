use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
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
    InsertTab,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    ToggleComment,
    ToggleFold,
    Undo,
    Redo,
    None,
}

#[must_use]
pub const fn map_key(key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match (ctrl, alt, shift, key.code) {
        // Esc: Quit
        (false, false, _, KeyCode::Esc) => Action::Quit,

        // Undo & Redo (VS Code & karet parity)
        (true, false, false, KeyCode::Char('z')) => Action::Undo,
        (true, false, false, KeyCode::Char('y')) | (true, false, true, KeyCode::Char('z' | 'Z')) => {
            Action::Redo
        }

        // Select all
        (true, false, false, KeyCode::Char('a')) => Action::SelectAll,

        // Folding
        (true, false, false, KeyCode::Char('f')) => Action::ToggleFold,

        // Comments
        (true, false, false, KeyCode::Char('/' | '7')) => Action::ToggleComment,

        // Horizontal navigation
        (false, false, _, KeyCode::Left) => Action::MoveLeft { extend: shift },
        (false, false, _, KeyCode::Right) => Action::MoveRight { extend: shift },
        (true, false, _, KeyCode::Left) | (false, true, _, KeyCode::Left) => {
            Action::MoveWordLeft { extend: shift }
        }
        (true, false, _, KeyCode::Right) | (false, true, _, KeyCode::Right) => {
            Action::MoveWordRight { extend: shift }
        }

        // Vertical navigation
        (false, false, _, KeyCode::Up) => Action::MoveUp { extend: shift },
        (false, false, _, KeyCode::Down) => Action::MoveDown { extend: shift },

        // Line bounds
        (false, false, _, KeyCode::Home) => Action::MoveLineStart { extend: shift },
        (false, false, _, KeyCode::End) => Action::MoveLineEnd { extend: shift },

        // Document bounds
        (true, false, _, KeyCode::Home) => Action::MoveDocStart { extend: shift },
        (true, false, _, KeyCode::End) => Action::MoveDocEnd { extend: shift },

        // Paging
        (false, false, _, KeyCode::PageUp) => Action::PageUp { extend: shift },
        (false, false, _, KeyCode::PageDown) => Action::PageDown { extend: shift },

        // Typing / Inserting
        (false, false, _, KeyCode::Tab) => Action::InsertTab,
        (false, false, _, KeyCode::Enter) => Action::InsertNewline,
        (false, false, _, KeyCode::Char(c)) => Action::InsertChar(c),

        // Deleting
        (false, false, false, KeyCode::Backspace) => Action::DeleteBackward,
        (false, false, false, KeyCode::Delete) => Action::DeleteForward,
        (true, false, _, KeyCode::Backspace) | (false, true, _, KeyCode::Backspace) => {
            Action::DeleteWordBackward
        }
        (true, false, _, KeyCode::Delete) | (false, true, _, KeyCode::Delete) => {
            Action::DeleteWordForward
        }

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
    fn test_map_key_navigation() {
        assert_eq!(
            map_key(press(KeyCode::Left, KeyModifiers::NONE)),
            Action::MoveLeft { extend: false }
        );
        assert_eq!(
            map_key(press(KeyCode::Left, KeyModifiers::SHIFT)),
            Action::MoveLeft { extend: true }
        );
        assert_eq!(
            map_key(press(KeyCode::Right, KeyModifiers::CONTROL)),
            Action::MoveWordRight { extend: false }
        );
        assert_eq!(
            map_key(press(KeyCode::Home, KeyModifiers::NONE)),
            Action::MoveLineStart { extend: false }
        );
    }

    #[test]
    fn test_map_key_editing() {
        assert_eq!(
            map_key(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::InsertChar('x')
        );
        assert_eq!(
            map_key(press(KeyCode::Tab, KeyModifiers::NONE)),
            Action::InsertTab
        );
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Action::InsertNewline
        );
        assert_eq!(
            map_key(press(KeyCode::Backspace, KeyModifiers::NONE)),
            Action::DeleteBackward
        );
        assert_eq!(
            map_key(press(KeyCode::Backspace, KeyModifiers::CONTROL)),
            Action::DeleteWordBackward
        );
    }

    #[test]
    fn test_map_key_commands() {
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            map_key(press(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Action::Undo
        );
        assert_eq!(
            map_key(press(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            Action::Redo
        );
        assert_eq!(
            map_key(press(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Action::ToggleFold
        );
        assert_eq!(
            map_key(press(KeyCode::Char('/'), KeyModifiers::CONTROL)),
            Action::ToggleComment
        );
    }
}
