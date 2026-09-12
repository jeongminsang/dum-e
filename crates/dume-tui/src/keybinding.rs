use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    SubmitInput,
    InsertChar(char),
    DeleteChar,
    CursorLeft,
    CursorRight,
    ScrollUp,
    ScrollDown,
    Clear,
    None,
}

pub fn handle_key_event(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Clear,
        KeyCode::Enter => Action::SubmitInput,
        KeyCode::Backspace => Action::DeleteChar,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::Char(c) => Action::InsertChar(c),
        _ => Action::None,
    }
}
