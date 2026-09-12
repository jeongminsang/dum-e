use crate::theme::Theme;
use dume_provider::types::{ChatMessage, Role};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_transcript(
    f: &mut Frame,
    area: Rect,
    messages: &[ChatMessage],
    streaming_text: &str,
    scroll_offset: u16,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" DUM-E Agent Session ");

    let mut lines = Vec::new();

    if messages.is_empty() && streaming_text.is_empty() {
        lines.push(Line::from(Span::styled(
            "Type a message and press Enter to begin interacting with DUM-E.",
            Style::default().fg(theme.foreground).add_modifier(Modifier::ITALIC),
        )));
    } else {
        for msg in messages {
            let (label, color) = match msg.role {
                Role::User => ("You: ", theme.user_msg),
                Role::Assistant => ("DUM-E: ", theme.assistant_msg),
                Role::System => ("System: ", theme.system_msg),
                Role::Tool => ("Tool Output: ", theme.system_msg),
            };

            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(&msg.content, Style::default().fg(theme.foreground)),
            ]));
            lines.push(Line::from(""));
        }

        if !streaming_text.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("DUM-E: ", Style::default().fg(theme.assistant_msg).add_modifier(Modifier::BOLD)),
                Span::styled(streaming_text, Style::default().fg(theme.foreground)),
                Span::styled(" ▍", Style::default().fg(theme.accent).add_modifier(Modifier::RAPID_BLINK)),
            ]));
            lines.push(Line::from(""));
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true })
        .scroll((scroll_offset, 0));

    f.render_widget(paragraph, area);
}

pub fn render_input_bar(
    f: &mut Frame,
    area: Rect,
    input_buffer: &str,
    cursor_pos: usize,
    theme: &Theme,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused))
        .title(" Prompt (Enter to submit, Ctrl+C to exit) ");

    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(input_buffer, Style::default().fg(theme.foreground)),
    ]))
    .block(block);

    f.render_widget(paragraph, area);

    // Position cursor
    let x = area.x + 3 + cursor_pos as u16;
    let y = area.y + 1;
    if x < area.x + area.width - 1 && y < area.y + area.height {
        f.set_cursor_position((x, y));
    }
}

pub fn render_status_bar(
    f: &mut Frame,
    area: Rect,
    model: &str,
    is_busy: bool,
    theme: &Theme,
) {
    let status_indicator = if is_busy {
        Span::styled(" [THINKING...] ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" [READY] ", Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD))
    };

    let line = Line::from(vec![
        Span::styled(" DUM-E Clean-Engine ", Style::default().fg(theme.status_bar_fg).add_modifier(Modifier::BOLD)),
        Span::styled(format!("| Model: {} |", model), Style::default().fg(theme.status_bar_fg)),
        status_indicator,
    ]);

    let paragraph = Paragraph::new(line)
        .style(Style::default().bg(theme.status_bar_bg));

    f.render_widget(paragraph, area);
}
