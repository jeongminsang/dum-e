use crate::theme::Theme;
use dume_provider::types::{ChatMessage, Role};
use dume_store::RecentSessionSummary;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn wrap_text_line(prefix: &str, text: &str, width: usize) -> Vec<String> {
    let prefix_width = UnicodeWidthStr::width(prefix);
    let available_width = width.saturating_sub(prefix_width).max(1);
    let mut wrapped = Vec::new();

    let mut current_line = String::new();
    let mut current_width = 0;

    for word in text.split_inclusive(char::is_whitespace) {
        let word_width = UnicodeWidthStr::width(word);
        if current_width + word_width <= available_width {
            current_line.push_str(word);
            current_width += word_width;
        } else {
            if !current_line.is_empty() {
                wrapped.push(current_line);
                current_line = String::new();
                current_width = 0;
            }
            if word_width <= available_width {
                current_line.push_str(word);
                current_width += word_width;
            } else {
                // Word is longer than available width, break by characters
                for ch in word.chars() {
                    let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if current_width + ch_width <= available_width {
                        current_line.push(ch);
                        current_width += ch_width;
                    } else {
                        if !current_line.is_empty() {
                            wrapped.push(current_line);
                            current_line = String::new();
                            current_width = 0;
                        }
                        current_line.push(ch);
                        current_width += ch_width;
                    }
                }
            }
        }
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }

    if wrapped.is_empty() {
        wrapped.push(String::new());
    }

    wrapped
}

pub fn build_transcript_lines(
    messages: &[ChatMessage],
    streaming_text: &str,
    recent_sessions: &[RecentSessionSummary],
    effective_width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<bool>) {
    let mut lines = Vec::new();
    let mut is_user_logical_line = Vec::new();

    let has_conversation = messages
        .iter()
        .any(|m| m.role == Role::User || m.role == Role::Assistant);

    if !has_conversation && streaming_text.is_empty() {
        lines.push(Line::from(""));
        is_user_logical_line.push(false);
        lines.push(Line::from(vec![
            Span::styled(
                "   ⚡ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "DUM-E Coding Agent",
                Style::default()
                    .fg(theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" [v{}]", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme.border_focused),
            ),
        ]));
        is_user_logical_line.push(false);
        lines.push(Line::from(Span::styled(
            "   ──────────────────────────────────────────────────────────",
            Style::default().fg(theme.border),
        )));
        is_user_logical_line.push(false);
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Press ", Style::default().fg(theme.foreground)),
            Span::styled(
                "Tab",
                Style::default()
                    .fg(theme.user_msg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to cycle thinking intensity (OFF / LOW / MED / HIGH)",
                Style::default().fg(theme.foreground),
            ),
        ]));
        is_user_logical_line.push(false);
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Type ", Style::default().fg(theme.foreground)),
            Span::styled(
                "/model",
                Style::default()
                    .fg(theme.assistant_msg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to switch LLM models by provider tab",
                Style::default().fg(theme.foreground),
            ),
        ]));
        is_user_logical_line.push(false);
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Type ", Style::default().fg(theme.foreground)),
            Span::styled(
                "/login",
                Style::default()
                    .fg(theme.assistant_msg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to authenticate a provider, including DeepSeek",
                Style::default().fg(theme.foreground),
            ),
        ]));
        is_user_logical_line.push(false);
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Press ", Style::default().fg(theme.foreground)),
            Span::styled(
                "↑ / ↓",
                Style::default()
                    .fg(theme.user_msg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " in the prompt bar to navigate previous input history",
                Style::default().fg(theme.foreground),
            ),
        ]));
        is_user_logical_line.push(false);
        lines.push(Line::from(""));
        is_user_logical_line.push(false);

        // Recent sessions overview section
        if !recent_sessions.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "   📋 Recent Sessions",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )]));
            is_user_logical_line.push(false);
            lines.push(Line::from(Span::styled(
                "   ──────────────────────────────────────────────────────────",
                Style::default().fg(theme.border),
            )));
            is_user_logical_line.push(false);

            let max_display = recent_sessions.len().min(5);
            for s in &recent_sessions[..max_display] {
                let id_short = if s.session_id.len() >= 8 {
                    &s.session_id[..8]
                } else {
                    &s.session_id
                };
                let preview = s.preview.trim().lines().next().unwrap_or("Empty session");
                let preview_truncated = if preview.chars().count() > 42 {
                    let s: String = preview.chars().take(39).collect();
                    format!("{}...", s)
                } else {
                    preview.to_string()
                };

                lines.push(Line::from(vec![
                    Span::styled("   # ", Style::default().fg(theme.border)),
                    Span::styled(
                        format!("{:<8} ", id_short),
                        Style::default()
                            .fg(theme.border_focused)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("({} msgs) ", s.message_count),
                        Style::default().fg(theme.border),
                    ),
                    Span::styled(preview_truncated, Style::default().fg(theme.foreground)),
                ]));
                is_user_logical_line.push(false);
            }
            lines.push(Line::from(""));
            is_user_logical_line.push(false);
        }
    }

    if has_conversation || !messages.is_empty() {
        for msg in messages {
            match msg.role {
                Role::User => {
                    // User prompt block: top and bottom padding lines
                    lines.push(Line::from(""));
                    is_user_logical_line.push(true);
                    for line in msg.content.lines() {
                        let wrapped = wrap_text_line("  ", line, effective_width);
                        for w in wrapped {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    w,
                                    Style::default()
                                        .fg(theme.user_msg)
                                        .add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            is_user_logical_line.push(true);
                        }
                    }
                    lines.push(Line::from(""));
                    is_user_logical_line.push(true);
                    lines.push(Line::from(""));
                    is_user_logical_line.push(false);
                }
                Role::Assistant => {
                    // Assistant response block: clean plain output without 'dum-e' tag
                    for line in msg.content.lines() {
                        let wrapped = wrap_text_line("  ", line, effective_width);
                        for w in wrapped {
                            lines.push(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(w, Style::default().fg(theme.foreground)),
                            ]));
                            is_user_logical_line.push(false);
                        }
                    }
                    lines.push(Line::from(""));
                    is_user_logical_line.push(false);
                }
                Role::Tool => {
                    // Tool call / execution output block
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.user_msg)),
                        Span::styled(
                            "tool execution",
                            Style::default()
                                .fg(theme.border_focused)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    is_user_logical_line.push(false);
                    for line in msg.content.lines() {
                        let wrapped = wrap_text_line("│ ", line, effective_width);
                        for w in wrapped {
                            lines.push(Line::from(vec![
                                Span::styled("│ ", Style::default().fg(theme.border)),
                                Span::styled(w, Style::default().fg(theme.status_bar_fg)),
                            ]));
                            is_user_logical_line.push(false);
                        }
                    }
                    lines.push(Line::from(""));
                    is_user_logical_line.push(false);
                }
                Role::System => {
                    // Notice banner format rather than plain conversational chat
                    lines.push(Line::from(vec![Span::styled(
                        "  [ NOTICE ] ",
                        Style::default()
                            .fg(theme.system_msg)
                            .add_modifier(Modifier::BOLD),
                    )]));
                    is_user_logical_line.push(false);
                    for line in msg.content.lines() {
                        let wrapped = wrap_text_line("  │ ", line, effective_width);
                        for w in wrapped {
                            lines.push(Line::from(vec![
                                Span::styled("  │ ", Style::default().fg(theme.system_msg)),
                                Span::styled(w, Style::default().fg(theme.foreground)),
                            ]));
                            is_user_logical_line.push(false);
                        }
                    }
                    lines.push(Line::from(""));
                    is_user_logical_line.push(false);
                }
            }
        }
    }

    // Streaming text (in progress)
    if !streaming_text.is_empty() {
        for line in streaming_text.lines() {
            let wrapped = wrap_text_line("  ", line, effective_width);
            for w in wrapped {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(w, Style::default().fg(theme.foreground)),
                ]));
                is_user_logical_line.push(false);
            }
        }
        // Streaming cursor indicator
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "▍",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::RAPID_BLINK),
            ),
        ]));
        is_user_logical_line.push(false);
    }

    (lines, is_user_logical_line)
}

pub fn render_transcript(
    f: &mut Frame,
    area: Rect,
    messages: &[ChatMessage],
    streaming_text: &str,
    scroll_offset: u16,
    recent_sessions: &[RecentSessionSummary],
    theme: &Theme,
) {
    let effective_width = area.width.max(1) as usize;
    let (lines, is_user_logical_line) = build_transcript_lines(
        messages,
        streaming_text,
        recent_sessions,
        effective_width,
        theme,
    );

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));

    f.render_widget(paragraph, area);

    // Apply user message background across all columns in visible user screen rows
    // Since lines are pre-wrapped to effective_width, each line maps 1:1 to an absolute screen row
    let scroll = scroll_offset as usize;
    let buf = f.buffer_mut();
    for row_offset in 0..area.height as usize {
        let absolute_row = scroll + row_offset;
        if is_user_logical_line
            .get(absolute_row)
            .copied()
            .unwrap_or(false)
        {
            let y = area.y + row_offset as u16;
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(cell.style().bg(theme.user_msg_bg));
                }
            }
        }
    }
}

pub fn commit_messages_to_scrollback<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    messages: &[ChatMessage],
    width: usize,
    theme: &Theme,
) -> std::io::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let (lines, is_user) = build_transcript_lines(messages, "", &[], width, theme);
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        let area = buf.area;
        let p = Paragraph::new(lines);
        p.render(area, buf);
        for (row_offset, &is_u) in is_user.iter().enumerate() {
            if is_u && (row_offset as u16) < area.height {
                let y = area.y + row_offset as u16;
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(cell.style().bg(theme.user_msg_bg));
                    }
                }
            }
        }
    })?;
    Ok(())
}

pub fn commit_welcome_banner_to_scrollback<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    recent_sessions: &[RecentSessionSummary],
    width: usize,
    theme: &Theme,
) -> std::io::Result<()> {
    let (lines, _) = build_transcript_lines(&[], "", recent_sessions, width, theme);
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        let area = buf.area;
        let p = Paragraph::new(lines);
        p.render(area, buf);
    })?;
    Ok(())
}

pub fn render_streaming_viewport(
    f: &mut Frame,
    area: Rect,
    streaming_text: &str,
    theme: &Theme,
) {
    if streaming_text.is_empty() || area.height == 0 {
        return;
    }
    let effective_width = area.width.max(1) as usize;
    let (lines, _) = build_transcript_lines(&[], streaming_text, &[], effective_width, theme);
    let h = area.height as usize;
    let visible: Vec<Line<'static>> = if lines.len() > h {
        lines[lines.len() - h..].to_vec()
    } else {
        lines
    };
    let paragraph = Paragraph::new(visible);
    f.render_widget(paragraph, area);
}

/// Calculate the total wrapped line count for the transcript given an inner area width.
pub fn calculate_transcript_height(
    width: u16,
    messages: &[ChatMessage],
    streaming_text: &str,
    recent_sessions_count: usize,
) -> u16 {
    let default_theme = Theme::default();
    let dummy_sessions: Vec<RecentSessionSummary> = (0..recent_sessions_count)
        .map(|i| RecentSessionSummary {
            session_id: format!("{:08}", i),
            preview: "preview".to_string(),
            message_count: 0,
            updated_at: 0,
        })
        .collect();
    let effective_width = width.max(1) as usize;
    let (lines, _) = build_transcript_lines(
        messages,
        streaming_text,
        &dummy_sessions,
        effective_width,
        &default_theme,
    );
    lines.len().min(u16::MAX as usize) as u16
}

pub fn render_input_bar(
    f: &mut Frame,
    area: Rect,
    input_buffer: &str,
    cursor_pos: usize,
    is_exit_warned: bool,
    is_busy: bool,
    anim_tick: usize,
    theme: &Theme,
) {
    let title = if is_busy {
        " Working... (Esc / Ctrl+C to cancel) "
    } else if is_exit_warned {
        " Prompt (Press Ctrl+C again within 2s to exit) "
    } else {
        " Prompt (Enter to submit, ↑/↓ for history, Esc to clear, Ctrl+C to exit) "
    };

    let border_color = if is_exit_warned {
        theme.accent
    } else {
        theme.border_focused
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);

    let raw_lines: Vec<&str> = input_buffer.split('\n').collect();
    let mut lines = Vec::with_capacity(raw_lines.len());
    for (idx, line) in raw_lines.iter().enumerate() {
        let prefix = if idx == 0 { "> " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(*line, Style::default().fg(theme.foreground)),
        ]));
    }

    let chars: Vec<char> = input_buffer.chars().collect();
    let pos = cursor_pos.min(chars.len());
    let mut cursor_line = 0usize;
    let mut cursor_col_width = 0usize;

    for (i, &c) in chars.iter().enumerate() {
        if i == pos {
            break;
        }
        if c == '\n' {
            cursor_line += 1;
            cursor_col_width = 0;
        } else {
            cursor_col_width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }

    let visible_height = area.height.saturating_sub(2) as usize;
    let scroll_y = if cursor_line >= visible_height {
        (cursor_line + 1 - visible_height) as u16
    } else {
        0
    };

    let paragraph = Paragraph::new(lines).block(block).scroll((scroll_y, 0));

    f.render_widget(paragraph, area);

    // If working, render a traveling dot (•) orbiting the input border perimeter
    if is_busy && area.width >= 2 && area.height >= 2 {
        let w = area.width as usize;
        let h = area.height as usize;
        let perimeter = 2 * (w + h) - 4;
        if perimeter > 0 {
            let pos = anim_tick % perimeter;
            let (dx, dy) = if pos < w {
                // Top border: left to right
                (pos, 0)
            } else if pos < w + h - 1 {
                // Right border: top to bottom
                (w - 1, pos - (w - 1))
            } else if pos < 2 * w + h - 2 {
                // Bottom border: right to left
                (w - 1 - (pos - (w + h - 1)), h - 1)
            } else {
                // Left border: bottom to top
                (0, h - 1 - (pos - (2 * w + h - 2)))
            };

            let dot_x = area.x + dx as u16;
            let dot_y = area.y + dy as u16;
            let buf = f.buffer_mut();
            if dot_x < buf.area.width && dot_y < buf.area.height {
                if let Some(cell) = buf.cell_mut((dot_x, dot_y)) {
                    cell.set_symbol("•");
                    cell.set_style(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }

    let x = area.x + 3 + cursor_col_width as u16;
    let y = area.y + 1 + (cursor_line as u16).saturating_sub(scroll_y);
    if x < area.x + area.width - 1 && y < area.y + area.height - 1 {
        f.set_cursor_position((x, y));
    }
}

pub struct AutocompleteItem {
    pub name: String,
    pub description: String,
}

pub fn render_autocomplete_dropdown(
    f: &mut Frame,
    input_area: Rect,
    items: &[AutocompleteItem],
    selected_idx: usize,
    theme: &Theme,
) {
    if items.is_empty() {
        return;
    }

    let item_height = items.len().min(7) as u16;
    let popup_height = item_height + 2;
    let popup_width = input_area.width.min(70);

    let popup_y = input_area.y.saturating_sub(popup_height);
    let popup_area = Rect::new(input_area.x, popup_y, popup_width, popup_height);

    f.render_widget(ratatui::widgets::Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Suggestions (↑/↓ to navigate, Tab/Enter to complete) ");

    let max_visible = item_height as usize;
    let scroll_offset = if selected_idx >= max_visible {
        selected_idx - (max_visible.saturating_sub(1))
    } else {
        0
    };

    let mut lines = Vec::new();
    for (i, item) in items
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(max_visible)
    {
        let is_selected = i == selected_idx;
        let prefix = if is_selected { "▶ " } else { "  " };

        let name_style = if is_selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD)
        };

        let desc_style = if is_selected {
            Style::default().fg(theme.status_bar_fg)
        } else {
            Style::default().fg(theme.border)
        };

        let bg = if is_selected {
            Style::default().bg(theme.status_bar_bg)
        } else {
            Style::default()
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(format!("{:<15}", item.name), name_style),
                Span::styled(format!(" - {}", item.description), desc_style),
            ])
            .style(bg),
        );
    }

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, popup_area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub fn next(self) -> Self {
        match self {
            ThinkingLevel::Off => ThinkingLevel::Low,
            ThinkingLevel::Low => ThinkingLevel::Medium,
            ThinkingLevel::Medium => ThinkingLevel::High,
            ThinkingLevel::High => ThinkingLevel::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "OFF",
            ThinkingLevel::Low => "LOW",
            ThinkingLevel::Medium => "MED",
            ThinkingLevel::High => "HIGH",
        }
    }
}

pub fn render_status_bar(
    f: &mut Frame,
    area: Rect,
    model: &str,
    thinking: ThinkingLevel,
    is_busy: bool,
    elapsed_secs: Option<f32>,
    last_run: Option<&str>,
    session_elapsed: Option<&str>,
    session_usage: Option<&str>,
    anim_tick: usize,
    available_update: Option<&str>,
    theme: &Theme,
) {
    if area.width < 96 {
        let short_model: String = model.chars().take(8).collect();
        let status = if is_busy {
            elapsed_secs
                .map(|seconds| format!("RUN {:.0}s", seconds))
                .unwrap_or_else(|| "RUN".into())
        } else {
            last_run.unwrap_or("READY").to_string()
        };
        let elapsed = session_elapsed
            .map(|value| format!("S{value}"))
            .unwrap_or_default();
        let usage = session_usage
            .map(|value| {
                value
                    .strip_prefix("TOKENS ")
                    .unwrap_or(value)
                    .replace(" +", "+")
                    .replace(" incomplete", "!")
            })
            .unwrap_or_default();
        let line = Line::from(format!(
            "{short_model} {} {status} {elapsed} T{usage}",
            thinking.label()
        ));
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.status_bar_bg)),
            area,
        );
        return;
    }
    let thinking_span = match thinking {
        ThinkingLevel::Off => Span::styled(" Thinking: OFF ", Style::default().fg(theme.border)),
        ThinkingLevel::Low => Span::styled(
            " Thinking: LOW ",
            Style::default()
                .fg(theme.user_msg)
                .add_modifier(Modifier::BOLD),
        ),
        ThinkingLevel::Medium => Span::styled(
            " Thinking: MED ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        ThinkingLevel::High => Span::styled(
            " Thinking: HIGH ",
            Style::default()
                .fg(theme.assistant_msg)
                .add_modifier(Modifier::BOLD),
        ),
    };

    let mut spans = vec![
        Span::styled(
            format!(" Model: {} ", model),
            Style::default()
                .fg(theme.status_bar_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("|", Style::default().fg(theme.border)),
        thinking_span,
    ];

    if let Some(version) = available_update {
        spans.push(Span::styled(
            format!(" [UPDATE: v{} available (type /update)] ", version),
            Style::default()
                .fg(theme.system_msg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if is_busy {
        // Braille spinner frames
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spinner_char = SPINNER[anim_tick % SPINNER.len()];

        // Ping-pong bouncing progress bar: [···█···]
        const BAR_WIDTH: usize = 9;
        let cycle = (BAR_WIDTH - 1) * 2;
        let pos = (anim_tick / 2) % cycle;
        let active_idx = if pos < BAR_WIDTH { pos } else { cycle - pos };

        let mut bar = String::with_capacity(BAR_WIDTH + 2);
        bar.push('[');
        for i in 0..BAR_WIDTH {
            if i == active_idx {
                bar.push('█');
            } else {
                bar.push('·');
            }
        }
        bar.push(']');

        let time_str = match elapsed_secs {
            Some(secs) => format!(" ({:.1}s)", secs),
            None => String::new(),
        };

        spans.push(Span::styled(
            format!(" {} thinking {}{} ", spinner_char, bar, time_str),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            " [READY] ",
            Style::default()
                .fg(theme.user_msg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let timing = if is_busy {
        elapsed_secs.map(|seconds| format!(" RUN {:.0}s ", seconds))
    } else {
        last_run.map(|last| format!(" LAST {last} "))
    };
    if let Some(timing) = timing {
        spans.push(Span::styled(timing, Style::default().fg(theme.accent)));
    }
    if let Some(session_elapsed) = session_elapsed {
        spans.push(Span::styled(
            format!(" SESSION {session_elapsed} "),
            Style::default().fg(theme.status_bar_fg),
        ));
    }
    if let Some(session_usage) = session_usage {
        spans.push(Span::styled(
            format!(" {session_usage} "),
            Style::default().fg(theme.status_bar_fg),
        ));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(theme.status_bar_bg));
    f.render_widget(paragraph, area);
}

pub fn render_model_selector_modal(
    f: &mut Frame,
    area: Rect,
    models: &[dume_provider::ModelInfo],
    selected_idx: usize,
    filter: &str,
    provider_tabs: &[String],
    active_tab_idx: usize,
    thinking: ThinkingLevel,
    current_model: &str,
    target: &crate::app::ModelSelectorTarget,
    theme: &Theme,
) {
    let model_offset = target.model_offset();
    let parent_model = current_model;
    let current_model = target.current_model(parent_model);
    let width = (area.width.saturating_sub(8)).min(94).max(40);
    let height = (area.height.saturating_sub(4)).min(26).max(14);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(if model_offset == 0 {
            " Select LLM Model (Tab: Thinking, Left/Right: Provider Tab, ↑/↓: Navigate, Esc: Cancel) "
        } else {
            " Select Subagent Model (Left/Right: Provider Tab, ↑/↓: Navigate, Enter: Select, Esc: Cancel) "
        });

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });

    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(2), // Provider Tabs
            ratatui::layout::Constraint::Length(2), // Search Bar + Thinking Indicator
            ratatui::layout::Constraint::Min(4),    // Model list
        ])
        .split(inner);

    // 1. Provider Tabs
    let mut tab_spans = Vec::new();
    for (i, tab) in provider_tabs.iter().enumerate() {
        let is_active = i == active_tab_idx;
        let tab_style = if is_active {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(theme.status_bar_bg)
        } else {
            Style::default().fg(theme.border)
        };
        tab_spans.push(Span::styled(format!(" [ {} ] ", tab), tab_style));
        tab_spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(tab_spans)), chunks[0]);

    // 2. Search bar + Thinking level button
    let thinking_style = match thinking {
        ThinkingLevel::Off => Style::default().fg(theme.border),
        ThinkingLevel::Low => Style::default()
            .fg(theme.user_msg)
            .add_modifier(Modifier::BOLD),
        ThinkingLevel::Medium => Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        ThinkingLevel::High => Style::default()
            .fg(theme.assistant_msg)
            .add_modifier(Modifier::BOLD),
    };

    let search_line = Line::from(vec![
        Span::styled(
            "Search: ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(filter, Style::default().fg(theme.foreground)),
        Span::styled(
            " ▍",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::RAPID_BLINK),
        ),
        Span::styled(
            if model_offset == 0 {
                format!("   [ Tab: Thinking Level = {} ]", thinking.label())
            } else {
                format!("   Parent: {parent_model}")
            },
            thinking_style,
        ),
    ]);
    f.render_widget(
        Paragraph::new(search_line).block(Block::default().borders(Borders::BOTTOM)),
        chunks[1],
    );

    // 3. Model List
    let list_area = chunks[2];
    let max_visible = list_area.height as usize;
    let scroll_offset = if selected_idx >= max_visible {
        selected_idx - (max_visible.saturating_sub(1))
    } else {
        0
    };

    let mut lines = Vec::new();
    if model_offset > 0 && scroll_offset == 0 {
        let selected = selected_idx == 0;
        let prefix = if selected { "▶ " } else { "  " };
        let tag = if current_model.is_empty() {
            " (current)"
        } else {
            ""
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}Inherit parent model{tag}"),
            if selected {
                Style::default()
                    .fg(theme.accent)
                    .bg(theme.status_bar_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.foreground)
            },
        )));
    }
    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No authenticated models match the current filter or provider.",
            Style::default()
                .fg(theme.border)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        for (i, m) in models
            .iter()
            .enumerate()
            .skip(scroll_offset.saturating_sub(model_offset))
            .take(max_visible.saturating_sub(lines.len()))
        {
            let is_selected = i + model_offset == selected_idx;
            let is_current =
                m.id == current_model || format!("{}/{}", m.provider, m.id) == current_model;

            let prefix = if is_selected { "▶ " } else { "  " };
            let tag = if is_current { " (current)" } else { "" };
            let reasoning_badge = if m.reasoning { " [Reasoning]" } else { "" };

            let name_style = if is_selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.foreground)
            };

            let prov_style = if is_selected {
                Style::default()
                    .fg(theme.user_msg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.border)
            };

            let bg = if is_selected {
                Style::default().bg(theme.status_bar_bg)
            } else {
                Style::default()
            };

            lines.push(
                Line::from(vec![
                    Span::styled(prefix, name_style),
                    Span::styled(format!("{:<15}", m.provider), prov_style),
                    Span::styled(m.id.as_str(), name_style),
                    Span::styled(reasoning_badge, Style::default().fg(theme.accent)),
                    Span::styled(tag, Style::default().fg(theme.assistant_msg)),
                ])
                .style(bg),
            );
        }
    }

    f.render_widget(Paragraph::new(lines), list_area);
    f.render_widget(block, modal_area);
}

pub struct LoginProviderChoice {
    pub id: &'static str,
    pub name: &'static str,
    pub auth_type: &'static str,
    pub is_authenticated: bool,
}

pub fn render_login_selector_modal(
    f: &mut Frame,
    area: Rect,
    providers: &[LoginProviderChoice],
    selected_idx: usize,
    theme: &Theme,
) {
    let width = 72.min(area.width.saturating_sub(6));
    let height = 12.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(
        ratatui::widgets::Clear,
        Rect::new(area.x, y, area.width, height),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Select Authentication Provider (↑/↓, Enter to select, Esc to cancel) ");

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Choose an LLM provider to log in or update credentials:",
        Style::default()
            .fg(theme.status_bar_fg)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, p) in providers.iter().enumerate() {
        let is_selected = i == selected_idx;
        let prefix = if is_selected { "▶ " } else { "  " };

        let status_span = if p.is_authenticated {
            Span::styled(
                "[Active ✓]  ",
                Style::default()
                    .fg(theme.user_msg)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("[No Auth]   ", Style::default().fg(theme.border))
        };

        let name_style = if is_selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD)
        };

        let bg = if is_selected {
            Style::default().bg(theme.status_bar_bg)
        } else {
            Style::default()
        };

        lines.push(
            Line::from(vec![
                Span::styled(prefix, name_style),
                status_span,
                Span::styled(format!("{:<18}", p.name), name_style),
                Span::styled(
                    format!("({})", p.auth_type),
                    Style::default().fg(theme.border),
                ),
            ])
            .style(bg),
        );
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_api_key_modal(
    f: &mut Frame,
    area: Rect,
    provider_name: &str,
    input_value: &str,
    theme: &Theme,
) {
    let width = 74.min(area.width.saturating_sub(6));
    let height = 8;
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(format!(
            " Enter API Key for {} (Esc to cancel) ",
            provider_name
        ));

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let masked: String = input_value.chars().map(|_| '•').collect();

    let lines = vec![
        Line::from(Span::styled(
            format!(
                "Paste your {} API key below and press Enter:",
                provider_name
            ),
            Style::default().fg(theme.status_bar_fg),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Key: ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&masked, Style::default().fg(theme.foreground)),
            Span::styled(
                " ▍",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::RAPID_BLINK),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_oauth_waiting_modal(
    f: &mut Frame,
    area: Rect,
    provider_name: &str,
    url: &str,
    theme: &Theme,
) {
    let width = 78.min(area.width.saturating_sub(4));
    let height = 10;
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(format!(
            " {} Browser Authentication (Esc to cancel) ",
            provider_name
        ));

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let lines = vec![
        Line::from(Span::styled(
            "Your browser should open automatically. If not, open this URL:",
            Style::default().fg(theme.status_bar_fg),
        )),
        Line::from(Span::styled(
            url,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Status: ",
                Style::default()
                    .fg(theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Waiting for OAuth authorization in browser...",
                Style::default().fg(theme.assistant_msg),
            ),
            Span::styled(
                " ▍",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::RAPID_BLINK),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_update_modal(
    f: &mut Frame,
    area: Rect,
    info: &dume_core::UpdateInfo,
    status_text: &str,
    in_progress: bool,
    theme: &Theme,
) {
    let width = 74.min(area.width.saturating_sub(4));
    let height = 16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let title = if in_progress {
        " In-Place Updating DUM-E (Please wait...) "
    } else {
        " DUM-E Update Available (Enter: Install in-place, Esc: Cancel) "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(title);

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "Current Version: ",
            Style::default().fg(theme.status_bar_fg),
        ),
        Span::styled(
            format!("v{}", info.current_version),
            Style::default().fg(theme.border),
        ),
        Span::styled("  ➔  ", Style::default().fg(theme.accent)),
        Span::styled("New Version: ", Style::default().fg(theme.status_bar_fg)),
        Span::styled(
            format!("v{}", info.latest_version),
            Style::default()
                .fg(theme.user_msg)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Asset: ", Style::default().fg(theme.status_bar_fg)),
        Span::styled(&info.asset_name, Style::default().fg(theme.foreground)),
    ]));
    lines.push(Line::from(""));

    if !info.release_notes.is_empty() {
        lines.push(Line::from(Span::styled(
            "Release Notes:",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for note_line in info.release_notes.lines().take(4) {
            lines.push(Line::from(Span::styled(
                format!("  {}", note_line),
                Style::default().fg(theme.foreground),
            )));
        }
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled(
            "Status: ",
            Style::default()
                .fg(theme.status_bar_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            status_text,
            Style::default().fg(if in_progress {
                theme.accent
            } else {
                theme.user_msg
            }),
        ),
    ]));

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_btw_modal(
    f: &mut Frame,
    area: Rect,
    history: &[(String, String)], // (role, content)
    input: &str,
    is_streaming: bool,
    streaming_reply: &str,
    theme: &Theme,
) {
    let width = 74.min(area.width.saturating_sub(4));
    let height = 22.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" /btw Side-Chat (Isolated Context, No Tools | Esc: Close, Enter: Ask) ");

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines = Vec::new();
    if history.is_empty() && streaming_reply.is_empty() {
        lines.push(Line::from(Span::styled(
            "Quickly ask questions without contaminating the main agent task session history.",
            Style::default().fg(theme.status_bar_fg),
        )));
        lines.push(Line::from(""));
    } else {
        for (role, msg) in history
            .iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let (prefix, style) = if role == "user" {
                (
                    "You: ",
                    Style::default()
                        .fg(theme.user_msg)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("AI:  ", Style::default().fg(theme.assistant_msg))
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(msg, Style::default().fg(theme.foreground)),
            ]));
        }
        if is_streaming {
            lines.push(Line::from(vec![
                Span::styled("AI:  ", Style::default().fg(theme.assistant_msg)),
                Span::styled(streaming_reply, Style::default().fg(theme.foreground)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ]));
        }
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled(
            "Query: ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(input, Style::default().fg(theme.foreground)),
        Span::styled("█", Style::default().fg(theme.accent)),
    ]));

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_resume_picker_modal(
    f: &mut Frame,
    area: Rect,
    sessions: &[RecentSessionSummary],
    selected_idx: usize,
    theme: &Theme,
) {
    let width = 76.min(area.width.saturating_sub(4));
    let height = 18.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Resume Conversation (Enter: Select, Esc: Cancel, ↑/↓: Navigate) ");

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines = Vec::new();
    if sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "No saved conversations found.",
            Style::default().fg(theme.status_bar_fg),
        )));
    } else {
        for (i, s) in sessions.iter().enumerate() {
            let is_selected = i == selected_idx;
            let cursor = if is_selected { " ➔ " } else { "   " };
            let short_id = if s.session_id.len() >= 8 {
                &s.session_id[..8]
            } else {
                &s.session_id
            };
            let preview = s.preview.trim().lines().next().unwrap_or("Empty session");
            let preview_trunc = if preview.chars().count() > 36 {
                let p: String = preview.chars().take(33).collect();
                format!("{}...", p)
            } else {
                preview.to_string()
            };

            let row_style = if is_selected {
                Style::default()
                    .fg(theme.user_msg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.foreground)
            };

            lines.push(Line::from(vec![
                Span::styled(cursor, Style::default().fg(theme.accent)),
                Span::styled(
                    format!("{:<9}", short_id),
                    if is_selected {
                        Style::default()
                            .fg(theme.border_focused)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.border_focused)
                    },
                ),
                Span::styled(
                    format!("({} msgs) ", s.message_count),
                    Style::default().fg(theme.border),
                ),
                Span::styled(preview_trunc, row_style),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}

pub fn render_routing_settings_modal(
    f: &mut Frame,
    area: Rect,
    draft: &crate::app::RoutingDraft,
    theme: &Theme,
) {
    let width = 76.min(area.width.saturating_sub(2).max(1));
    let height = 18.min(area.height.saturating_sub(2).max(1));
    let modal_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(ratatui::widgets::Clear, modal_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" 라우팅 설정 ");
    let settings = &draft.settings;
    let classifier = settings.classifier.as_ref();
    let backend = classifier
        .map(|c| match c.backend {
            dume_provider::ClassifierBackend::OpenCodeZen => "OpenCode Zen",
            dume_provider::ClassifierBackend::LocalSystemOne => "LocalSystemOne",
        })
        .unwrap_or("OpenCode Zen");
    let mut rows = vec![
        format!(
            "{} 사용: {}",
            marker(draft.selected, 0),
            if settings.enabled { "예" } else { "아니요" }
        ),
        format!("{} 분류 서버: {}", marker(draft.selected, 1), backend),
        format!(
            "{} 서버 URL: {}",
            marker(draft.selected, 2),
            routing_visible(draft, 2, draft.field_edit)
        ),
        format!(
            "{} 분류 모델: {}",
            marker(draft.selected, 3),
            routing_visible(draft, 3, draft.field_edit)
        ),
        format!(
            "{} 표준 계획: {}",
            marker(draft.selected, 4),
            role_visible(settings.models.standard.as_deref())
        ),
        format!(
            "{} 복잡 계획: {}",
            marker(draft.selected, 5),
            role_visible(settings.models.complex.as_deref())
        ),
        format!(
            "{} 구현: {}",
            marker(draft.selected, 6),
            role_visible(settings.models.implementation.as_deref())
        ),
        format!(
            "{} 분류기 키: {} [{}]",
            marker(draft.selected, 7),
            routing_key_mask(draft),
            draft.key_state
        ),
        format!("{} 키 저장", marker(draft.selected, 8)),
        format!("{} 키 삭제", marker(draft.selected, 9)),
        format!("{} 설정 저장", marker(draft.selected, 10)),
        format!("{} 수동 연결 확인", marker(draft.selected, 11)),
        format!("{} 닫기", marker(draft.selected, 12)),
    ];
    if draft.picker_role.is_some() {
        rows.push("↑/↓ 모델 선택 · Enter 적용 · Esc 취소".into());
        rows.push(format!(
            "{} 기본 모델 상속",
            if draft.picker_selected == 0 {
                "›"
            } else {
                " "
            }
        ));
        if let Some(models) = &draft.picker_models {
            for (idx, model) in models.iter().enumerate() {
                let selection = idx + 1;
                rows.push(format!(
                    "{} {}/{}",
                    if selection == draft.picker_selected {
                        "›"
                    } else {
                        " "
                    },
                    model.provider,
                    model.id
                ));
            }
        }
    } else {
        rows.push("미저장 키 입력은 연결 확인에 사용되지 않습니다.".into());
        rows.push("Enter 실행 · Backspace 삭제 · ←/→ 커서 · Tab 다음 항목".into());
    }
    rows.push(draft.message.clone());
    f.render_widget(block, modal_area);
    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });
    let selected_row = if draft.picker_role.is_some() {
        14 + draft.picker_selected
    } else {
        draft.selected
    };
    let visual_start = rows
        .iter()
        .take(selected_row)
        .map(|row| wrapped_row_count(row, inner.width))
        .sum::<usize>();
    let selected_visual_height = rows
        .get(selected_row)
        .map_or(1, |row| wrapped_row_count(row, inner.width));
    let visible_rows = inner.height.max(1) as usize;
    let cursor_visual_offset = if draft.field_edit && matches!(draft.selected, 2 | 3 | 7) {
        let value = if draft.selected == 7 {
            draft.key_input.as_str()
        } else {
            draft
                .settings
                .classifier
                .as_ref()
                .map(|config| {
                    if draft.selected == 2 {
                        config.base_url.as_str()
                    } else {
                        config.model.as_str()
                    }
                })
                .unwrap_or("")
        };
        let prefix: String = value.chars().take(draft.cursor).collect();
        (UnicodeWidthStr::width(prefix.as_str()) + 12) / usize::from(inner.width.max(1))
    } else {
        0
    };
    let target_visual_row =
        visual_start + cursor_visual_offset.min(selected_visual_height.saturating_sub(1));
    let scroll = target_visual_row
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(usize::from(u16::MAX)) as u16;
    f.render_widget(
        Paragraph::new(rows.into_iter().map(Line::from).collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
    );
}

pub fn render_routing_probe_confirm_modal(
    f: &mut Frame,
    area: Rect,
    confirmed: bool,
    theme: &Theme,
) {
    let width = 72.min(area.width.saturating_sub(2).max(1));
    let height = 9.min(area.height.saturating_sub(2).max(1));
    let modal = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(ratatui::widgets::Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" 외부 연결 확인 ");
    f.render_widget(block, modal);
    f.render_widget(
        Paragraph::new(vec![
            Line::from("고정된 테스트 문구가 설정 서버로 전송됩니다."),
            Line::from(
                "외부 서버 전송으로 API 비용이 발생할 수 있으며 단가는 확인되지 않았습니다.",
            ),
            Line::from(format!(
                "{} 취소     {} 전송 확인",
                if confirmed { "  " } else { "[선택]" },
                if confirmed { "[선택]" } else { "  " }
            )),
            Line::from("←/→ 또는 Tab 선택 · Enter 적용 · Esc 취소"),
        ])
        .wrap(Wrap { trim: false }),
        modal.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 1,
        }),
    );
}

fn marker(selected: usize, index: usize) -> &'static str {
    if selected == index { "›" } else { " " }
}
fn role_visible(value: Option<&str>) -> &str {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("기본 모델 사용 (상속)")
}
fn routing_visible(draft: &crate::app::RoutingDraft, index: usize, editing: bool) -> String {
    if draft.selected != index || !editing {
        return if index == 2 {
            draft
                .settings
                .classifier
                .as_ref()
                .map(|c| c.base_url.clone())
                .unwrap_or_default()
        } else {
            draft
                .settings
                .classifier
                .as_ref()
                .map(|c| c.model.clone())
                .unwrap_or_default()
        };
    }
    let value = if index == 2 {
        draft
            .settings
            .classifier
            .as_ref()
            .map(|c| c.base_url.as_str())
            .unwrap_or("")
    } else {
        draft
            .settings
            .classifier
            .as_ref()
            .map(|c| c.model.as_str())
            .unwrap_or("")
    };
    let cursor = value
        .char_indices()
        .nth(draft.cursor)
        .map_or(value.len(), |(index, _)| index);
    format!("{}│{}", &value[..cursor], &value[cursor..])
}

fn routing_key_mask(draft: &crate::app::RoutingDraft) -> String {
    let count = draft.key_input.chars().count().min(24);
    if draft.selected == 7 && draft.field_edit {
        let cursor = draft.cursor.min(count);
        format!(
            "{}│{}",
            "•".repeat(cursor),
            "•".repeat(count.saturating_sub(cursor))
        )
    } else {
        "•".repeat(count)
    }
}

fn wrapped_row_count(row: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    UnicodeWidthStr::width(row).max(1).div_ceil(width)
}

#[cfg(test)]
mod status_bar_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        (area.y..area.bottom())
            .map(|y| {
                let mut row = String::new();
                let mut skip_until = area.x;
                for x in area.x..area.right() {
                    let cell = &buffer[(x, y)];
                    if x < skip_until {
                        continue;
                    }
                    let symbol = cell.symbol();
                    row.push_str(symbol);
                    skip_until = x + UnicodeWidthStr::width(symbol).max(1) as u16;
                }
                row
            })
            .collect()
    }

    #[test]
    fn subagent_model_picker_renders_inheritance_current_model_and_scrolled_selection() {
        let base = dume_provider::resolve_model("openai/gpt-5.4").unwrap();
        let models: Vec<_> = (0..24)
            .map(|index| {
                let mut model = base.clone();
                model.id = format!("child-{index}");
                model
            })
            .collect();
        for (width, height) in [(60, 18), (100, 30)] {
            for selected_idx in [0, 1, 24] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let target = crate::app::ModelSelectorTarget::Subagents {
                    path: std::path::PathBuf::new(),
                    model: if selected_idx == 0 {
                        None
                    } else {
                        Some("openai/child-0".into())
                    },
                };
                terminal
                    .draw(|frame| {
                        render_model_selector_modal(
                            frame,
                            frame.area(),
                            &models,
                            selected_idx,
                            "",
                            &["All".into(), "openai".into()],
                            0,
                            ThinkingLevel::Off,
                            "parent/model",
                            &target,
                            &Theme::default(),
                        );
                    })
                    .unwrap();
                let output = rendered_rows(&terminal).join("\n");
                assert!(output.contains("Select Subagent Model"), "{output}");
                assert!(!output.contains("Tab: Thinking"), "{output}");
                if selected_idx == 0 {
                    assert!(
                        output.contains("▶ Inherit parent model (current)"),
                        "{output}"
                    );
                } else if selected_idx == 1 {
                    assert!(output.contains("Inherit parent model"), "{output}");
                    assert!(
                        !output.contains("Inherit parent model (current)"),
                        "{output}"
                    );
                    assert!(
                        output.lines().any(|line| line.contains('▶')
                            && line.contains("child-0")
                            && line.contains("(current)")),
                        "{output}"
                    );
                } else {
                    assert!(
                        output
                            .lines()
                            .any(|line| line.contains('▶') && line.contains("child-23")),
                        "{output}"
                    );
                }
            }
        }
    }

    #[test]
    fn routing_modal_masks_unsaved_secret_and_keeps_selection_visible_at_narrow_width() {
        for (width, height) in [(32, 12), (100, 28)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut draft = crate::app::RoutingDraft::new(
                dume_worker::RoutingSettings::default(),
                "없음".into(),
            );
            draft.selected = 12;
            draft.key_input = "super-secret-key".into();
            terminal
                .draw(|frame| {
                    render_routing_settings_modal(frame, frame.area(), &draft, &Theme::default())
                })
                .unwrap();
            let rows = rendered_rows(&terminal);
            let output = rows.join("\n");
            assert!(output.contains("super-secret-key") == false);
            assert!(
                rows.iter().any(|row| row.contains("닫기")),
                "rows: {rows:?}"
            );
            assert!(
                rows.iter().any(|row| row.contains("라우팅")),
                "rows: {rows:?}"
            );
        }
    }

    #[test]
    fn routing_modal_scrolls_long_edit_cursor_into_narrow_viewport() {
        let backend = TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut draft =
            crate::app::RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into());
        draft.settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::OpenCodeZen,
            base_url: format!("http://127.0.0.1/{}", "경로".repeat(24)),
            model: "model".into(),
        });
        draft.selected = 2;
        draft.field_edit = true;
        draft.cursor = draft
            .settings
            .classifier
            .as_ref()
            .unwrap()
            .base_url
            .chars()
            .count();
        terminal
            .draw(|frame| {
                render_routing_settings_modal(frame, frame.area(), &draft, &Theme::default())
            })
            .unwrap();
        let rows = rendered_rows(&terminal);
        assert!(rows.iter().any(|row| row.contains("›")), "rows: {rows:?}");
        assert!(rows.iter().any(|row| row.contains('│')), "rows: {rows:?}");
    }

    fn rendered_status(width: u16) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect::new(0, 0, width, 1),
                    "test-model",
                    ThinkingLevel::Off,
                    false,
                    None,
                    Some("ERROR 3s"),
                    Some("7s"),
                    Some("TOKENS 42 +1 incomplete"),
                    0,
                    None,
                    &Theme::default(),
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn status_bar_renders_session_outcome_and_usage_at_narrow_and_wide_sizes() {
        for width in [40, 80] {
            let output = rendered_status(width);
            assert!(output.contains("ERROR 3s"));
            assert!(output.contains("S7s"));
            assert!(output.contains("T42+1!"));
        }
    }

    #[test]
    fn routing_modal_renders_korean_enabled_state_at_narrow_and_wide_sizes() {
        for (width, height) in [(30, 10), (100, 30)] {
            for enabled in [false, true] {
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| {
                        render_routing_settings_modal(
                            frame,
                            frame.area(),
                            &{
                                let mut settings = dume_worker::RoutingSettings::default();
                                settings.enabled = enabled;
                                let mut draft =
                                    crate::app::RoutingDraft::new(settings, "없음".into());
                                draft.message = "라우팅 설정을 저장했습니다.".into();
                                draft
                            },
                            &Theme::default(),
                        );
                    })
                    .unwrap();
                let rows = rendered_rows(&terminal);
                assert!(
                    rows.iter().any(|row| row.contains("라우팅")),
                    "rows: {rows:?}"
                );
                let selected = if enabled {
                    "› 사용: 예"
                } else {
                    "› 사용: 아니요"
                };
                assert!(
                    rows.iter().any(|row| row.contains(selected)),
                    "rows: {rows:?}"
                );
            }
        }
    }

    #[test]
    fn test_commit_messages_and_welcome_banner_to_scrollback() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let banner_res = commit_welcome_banner_to_scrollback(&mut terminal, &[], 80, &Theme::default());
        assert!(banner_res.is_ok());

        let messages = vec![
            ChatMessage::user("hello world"),
            ChatMessage::assistant("hi there!"),
        ];
        let msg_res = commit_messages_to_scrollback(&mut terminal, &messages, 80, &Theme::default());
        assert!(msg_res.is_ok());
    }

    #[test]
    fn test_render_streaming_viewport() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                render_streaming_viewport(f, f.area(), "Streaming output line 1\nLine 2", &Theme::default());
            })
            .unwrap();

        let rows = rendered_rows(&terminal);
        assert!(rows.iter().any(|r| r.contains("Streaming output line 1")));
        assert!(rows.iter().any(|r| r.contains("Line 2")));
    }
}
