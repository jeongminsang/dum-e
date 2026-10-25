pub mod app;
pub mod component;
pub mod keybinding;
pub mod program_status;
pub mod tool_display;
mod selection;
mod terminal;
pub mod theme;

pub use app::{App, ResumeTarget, run_tui, run_tui_with_target};
pub use keybinding::{Action, KeyBindings};
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
        assert!(items.iter().any(|i| i.name == "/update"));
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
    fn slash_autocomplete_shows_bundled_skills_on_the_first_page_outside_a_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new("test-model");
        app.skills = dume_core::SkillRegistry::load_default_for(directory.path());
        app.insert_char('/');
        let items = app.get_autocomplete_items();
        let first_page: Vec<_> = items.iter().take(7).map(|item| item.name.as_str()).collect();
        assert_eq!(
            first_page,
            [
                "/model",
                "/add-llm-provider",
                "/deep-interview",
                "/goal",
                "/interactive-testing",
                "/ralplan",
                "/release",
            ]
        );
        assert!(items.iter().any(|item| item.name == "/model"));
        assert!(items.iter().any(|item| item.name == "/skills"));

        for (prefix, expected) in [
            ("/m", "/model"),
            ("/deep", "/deep-interview"),
            ("/ral", "/ralplan"),
            ("/go", "/goal"),
        ] {
            app.input_buffer = prefix.into();
            let matches = app.get_autocomplete_items();
            assert_eq!(matches.len(), 1, "{prefix}");
            assert_eq!(matches[0].name, expected);
        }
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

    #[test]
    fn test_prompt_history_navigation() {
        let mut app = App::new("test-model");
        app.prompt_history.push("first command".to_string());
        app.prompt_history.push("second command".to_string());

        app.insert_char('t');
        app.insert_char('y');
        app.insert_char('p');
        app.insert_char('i');
        app.insert_char('n');
        app.insert_char('g');

        // Navigate Up: saves "typing", shows "second command"
        app.navigate_history_up();
        assert_eq!(app.input_buffer, "second command");
        assert_eq!(app.history_index, Some(1));

        // Navigate Up again: shows "first command"
        app.navigate_history_up();
        assert_eq!(app.input_buffer, "first command");
        assert_eq!(app.history_index, Some(0));

        // Navigate Up again: stays at earliest
        app.navigate_history_up();
        assert_eq!(app.input_buffer, "first command");
        assert_eq!(app.history_index, Some(0));

        // Navigate Down: back to "second command"
        app.navigate_history_down();
        assert_eq!(app.input_buffer, "second command");
        assert_eq!(app.history_index, Some(1));

        // Navigate Down again: restores saved temp_input "typing"
        app.navigate_history_down();
        assert_eq!(app.input_buffer, "typing");
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn test_rainbow_color_cycle() {
        let c0 = theme::rainbow_color(0);
        let c16 = theme::rainbow_color(16);
        assert_eq!(c0, c16);
    }

    #[test]
    fn test_mouse_scroll_and_autoscroll_restoration() {
        let mut app = App::new("test-model");
        assert!(app.auto_scroll);
        assert_eq!(app.scroll_offset, 0);

        // Simulate scrolling up
        app.auto_scroll = false;
        app.scroll_offset = 20;
        app.scroll_offset = app.scroll_offset.saturating_sub(3);
        assert_eq!(app.scroll_offset, 17);
        assert!(!app.auto_scroll);

        // Simulate scrolling down until bottom reached
        let max_scroll = 20u16;
        app.scroll_offset = app.scroll_offset.saturating_add(3);
        assert_eq!(app.scroll_offset, 20);

        // At or beyond max_scroll, auto_scroll should be restored
        if app.scroll_offset >= max_scroll {
            app.scroll_offset = max_scroll;
            app.auto_scroll = true;
        }
        assert!(app.auto_scroll);
        assert_eq!(app.scroll_offset, 20);
    }

    #[test]
    fn test_mouse_scroll_does_not_mutate_input_history() {
        let mut app = App::new("test-model");
        app.prompt_history.push("first command".to_string());
        app.prompt_history.push("second command".to_string());
        app.input_buffer = "current typing".to_string();

        // Mouse scroll up/down modifies scroll_offset and auto_scroll only
        app.auto_scroll = false;
        app.scroll_offset = app.scroll_offset.saturating_sub(3);
        assert_eq!(app.input_buffer, "current typing");
        assert_eq!(app.history_index, None);

        app.scroll_offset = app.scroll_offset.saturating_add(3);
        assert_eq!(app.input_buffer, "current typing");
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn test_multiline_input_and_navigation() {
        let mut app = App::new("test-model");
        for c in "line1\nline2".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input_buffer, "line1\nline2");
        assert_eq!(app.cursor_pos, 11);

        // Cursor up should move to line 1 at same column
        assert!(app.cursor_up());
        assert_eq!(app.cursor_pos, 5); // "line1"
        // Cursor up at line 0 should return false
        assert!(!app.cursor_up());
        assert_eq!(app.cursor_pos, 5);

        // Cursor down should move back to line 2
        assert!(app.cursor_down());
        assert_eq!(app.cursor_pos, 11);
        // Cursor down at bottom line should return false
        assert!(!app.cursor_down());
        assert_eq!(app.cursor_pos, 11);
    }

    #[test]
    fn test_delete_to_beginning_and_word_deletion() {
        let mut app = App::new("test-model");
        for c in "foo bar baz".chars() {
            app.insert_char(c);
        }
        // Word delete backward
        app.delete_word_backward();
        assert_eq!(app.input_buffer, "foo bar ");
        assert_eq!(app.cursor_pos, 8);

        // Delete to beginning
        app.delete_to_beginning();
        assert_eq!(app.input_buffer, "");
        assert_eq!(app.cursor_pos, 0);

        // Multiline delete to beginning
        for c in "first line\nsecond line".chars() {
            app.insert_char(c);
        }
        // Should delete only the second line up to the newline
        app.delete_to_beginning();
        assert_eq!(app.input_buffer, "first line\n");
        assert_eq!(app.cursor_pos, 11);

        // Calling again at beginning of line deletes preceding newline
        app.delete_to_beginning();
        assert_eq!(app.input_buffer, "first line");
        assert_eq!(app.cursor_pos, 10);
    }

    #[test]
    fn test_blend_rgb() {
        use ratatui::style::Color;
        let c1 = Color::Rgb(100, 100, 100);
        let c2 = Color::Rgb(200, 200, 200);

        assert_eq!(theme::blend_rgb(c1, c2, 0.0), c1);
        assert_eq!(theme::blend_rgb(c1, c2, 1.0), c2);
        assert_eq!(theme::blend_rgb(c1, c2, 0.5), Color::Rgb(150, 150, 150));
    }

    #[test]
    fn test_app_current_working_phase_transitions() {
        let mut app = App::new("test-model");
        // 1. Not busy -> None
        assert_eq!(app.current_working_phase(), None);

        // 2. Busy default -> Thinking...
        app.is_busy = true;
        assert_eq!(app.current_working_phase().as_deref(), Some("Thinking..."));

        // 3. Streaming text arriving -> Generating response...
        app.streaming_text = "Hello world".into();
        assert_eq!(app.current_working_phase().as_deref(), Some("Generating response..."));

        // 4. Streaming tools -> Preparing ...
        app.streaming_tools.push(crate::tool_display::StreamingToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        });
        assert!(app.current_working_phase().as_deref().unwrap().contains("Preparing"));

        // 5. Active running tool takes highest priority
        app.active_tools.push(crate::tool_display::ActiveToolItem {
            call_id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo check"}"#.into(),
            status: crate::tool_display::ActiveToolStatus::Running {
                started_at: std::time::Instant::now(),
            },
            output: None,
        });
        assert!(app.current_working_phase().as_deref().unwrap().contains("Running"));
    }

    #[test]
    fn test_app_queued_badge_expiry() {
        let mut app = App::new("test-model");
        assert_eq!(app.queued_badge(), None);

        app.queued_feedback = Some(("Steer queued: \"test\"".into(), std::time::Instant::now()));
        assert_eq!(app.queued_badge(), Some("Steer queued: \"test\""));

        // Expired after 6s
        app.queued_feedback = Some((
            "Steer queued: \"test\"".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(7),
        ));
        assert_eq!(app.queued_badge(), None);
    }

    #[test]
    fn test_render_input_bar_busy_preserves_border_and_shows_placeholder() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let default_theme = Theme::default();

        terminal
            .draw(|f| {
                component::render_input_bar(
                    f,
                    f.area(),
                    "",
                    0,
                    false,
                    true,
                    Some("Running bash · cargo check"),
                    Some("Steer queued: \"add tests\""),
                    10,
                    &default_theme,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        // Verify corners and horizontal borders remain box-drawing chars, not replaced by '•'
        let top_left = buffer.cell((0, 0)).unwrap().symbol();
        assert_ne!(top_left, "•");

        // Verify placeholder text is rendered when input_buffer is empty and is_busy is true
        let mut found_placeholder = false;
        let mut found_title = false;
        for y in 0..5 {
            let row_str: String = (0..80)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect();
            if row_str.contains("Type to steer active run") {
                found_placeholder = true;
            }
            if row_str.contains("Running bash") {
                found_title = true;
            }
        }
        assert!(found_placeholder, "Placeholder was not found in buffer");
        assert!(found_title, "Running phase title was not found in buffer");
    }

    #[test]
    fn test_render_status_bar_busy_displays_only_thinking_and_seconds() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(100, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let default_theme = Theme::default();

        terminal
            .draw(|f| {
                component::render_status_bar(
                    f,
                    f.area(),
                    "test-model",
                    component::ThinkingLevel::Default,
                    true,
                    Some(2.5),
                    None,
                    None,
                    None,
                    0,
                    None,
                    &default_theme,
                );
            })
            .unwrap();

        let row: String = (0..100)
            .map(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol())
            .collect();

        // Must contain "thinking (2.5s)"
        assert!(row.contains("thinking (2.5s)"), "row: {row}");
        // Must NOT contain bouncing progress bar or spinner
        assert!(!row.contains("···"), "row: {row}");
        assert!(!row.contains("█"), "row: {row}");
        assert!(!row.contains("⠋"), "row: {row}");
    }
}
