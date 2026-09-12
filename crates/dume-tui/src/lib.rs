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
}
