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

        // Test CursorEnd with multi-byte characters
        app.cursor_pos = 0;
        app.cursor_pos = app.input_buffer.chars().count();
        assert_eq!(app.cursor_pos, 4);
    }

    #[test]
    fn test_autocomplete_filtering() {
        let mut app = App::new("test-model");
        // Not starting with / -> no items
        assert!(app.get_autocomplete_items().is_empty());

        // Starting with / -> shows built-in commands and loaded skills
        app.insert_char('/');
        let items = app.get_autocomplete_items();
        assert!(!items.is_empty());
        assert!(items.iter().any(|i| i.name == "/model"));
        assert!(items.iter().any(|i| i.name == "/skills"));

        // Filter with prefix /m
        app.insert_char('m');
        let filtered = app.get_autocomplete_items();
        assert!(filtered.iter().all(|i| i.name.starts_with("/m")));
        assert!(filtered.iter().any(|i| i.name == "/model"));

        // Once space is typed, autocomplete dismisses
        app.insert_char(' ');
        assert!(app.get_autocomplete_items().is_empty());
    }

    #[test]
    fn test_ctrl_c_safeguard_and_double_press() {
        let mut app = App::new("test-model");
        app.insert_char('a');
        app.insert_char('b');
        assert_eq!(app.input_buffer, "ab");

        // 1. First Ctrl+C with input: clears buffer, does not quit
        if !app.input_buffer.is_empty() {
            app.input_buffer.clear();
            app.cursor_pos = 0;
            app.last_ctrl_c = None;
        }
        assert!(app.input_buffer.is_empty());
        assert_eq!(app.cursor_pos, 0);
        assert!(app.last_ctrl_c.is_none());

        // 2. Ctrl+C on empty buffer: arms exit warning
        let now = std::time::Instant::now();
        app.last_ctrl_c = Some(now);

        // 3. Immediate subsequent Ctrl+C within 2 seconds: triggers quit
        let second_press = now + std::time::Duration::from_millis(500);
        let should_quit = app.last_ctrl_c.map_or(false, |t| {
            second_press.duration_since(t) < std::time::Duration::from_secs(2)
        });
        assert!(should_quit);

        // 4. Subsequent Ctrl+C after 3 seconds: does not quit, re-arms
        let late_press = now + std::time::Duration::from_secs(3);
        let should_quit_late = app.last_ctrl_c.map_or(false, |t| {
            late_press.duration_since(t) < std::time::Duration::from_secs(2)
        });
        assert!(!should_quit_late);
    }
}

