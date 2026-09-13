pub mod app;
pub mod component;
pub mod keybinding;
pub mod theme;

pub use app::{run_tui, App};
pub use keybinding::Action;
pub use theme::Theme;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_input_buffer() {
        let mut app = App::new("claude-3-5-sonnet");
        app.insert_char('h');
        app.insert_char('i');
        assert_eq!(app.input_buffer, "hi");
        assert_eq!(app.cursor_pos, 2);

        app.cursor_left();
        assert_eq!(app.cursor_pos, 1);

        app.delete_char();
        assert_eq!(app.input_buffer, "i");
        assert_eq!(app.cursor_pos, 0);
    }

    #[test]
    fn test_app_korean_utf8_input_and_deletion() {
        let mut app = App::new("claude-3-5-sonnet");
        // Insert Korean multi-byte characters (3 bytes per char in UTF-8)
        app.insert_char('안');
        app.insert_char('녕');
        app.insert_char('하');
        app.insert_char('세');
        app.insert_char('요');
        assert_eq!(app.input_buffer, "안녕하세요");
        assert_eq!(app.cursor_pos, 5); // 5 characters, not 15 bytes

        app.cursor_left();
        assert_eq!(app.cursor_pos, 4);

        app.delete_char();
        assert_eq!(app.input_buffer, "안녕하요");
        assert_eq!(app.cursor_pos, 3);
    }
}
