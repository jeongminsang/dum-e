use crate::theme::Theme;
use crate::tool_display::{
    ActiveToolStatus, TranscriptConfig, extract_failure_summary, format_duration_short,
    format_tool_display,
};
use dume_provider::types::{ChatMessage, Role};
use dume_store::RecentSessionSummary;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
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

fn wrap_spans<'a>(
    spans: Vec<Span<'a>>,
    width: usize,
    continuation_prefix: &str,
) -> Vec<Line<'static>> {
    let available_width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0;

    let continuation_prefix_width = UnicodeWidthStr::width(continuation_prefix);

    let push_span = |spans: &mut Vec<Span<'static>>, text: &str, style: Style| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut() {
            if last.style == style {
                let mut s = last.content.to_string();
                s.push_str(text);
                *last = Span::styled(s, style);
                return;
            }
        }
        spans.push(Span::styled(text.to_string(), style));
    };

    for span in spans {
        let style = span.style;
        let content = span.content;

        for word in content.split_inclusive(char::is_whitespace) {
            let word_width = UnicodeWidthStr::width(word);
            if current_width + word_width <= available_width {
                push_span(&mut current_spans, word, style);
                current_width += word_width;
            } else {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                    current_width = 0;
                }

                let max_avail = if !lines.is_empty() && continuation_prefix_width < available_width {
                    available_width - continuation_prefix_width
                } else {
                    available_width
                };

                if word_width <= max_avail {
                    if !lines.is_empty() && continuation_prefix_width > 0 && continuation_prefix_width < available_width {
                        push_span(&mut current_spans, continuation_prefix, Style::default());
                        current_width += continuation_prefix_width;
                    }
                    push_span(&mut current_spans, word, style);
                    current_width += word_width;
                } else {
                    for ch in word.chars() {
                        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if current_width == 0 && !lines.is_empty() && continuation_prefix_width > 0 && continuation_prefix_width < available_width {
                            push_span(&mut current_spans, continuation_prefix, Style::default());
                            current_width += continuation_prefix_width;
                        }
                        if current_width + ch_width <= available_width {
                            let mut buf = [0u8; 4];
                            let s = ch.encode_utf8(&mut buf);
                            push_span(&mut current_spans, s, style);
                            current_width += ch_width;
                        } else {
                            if !current_spans.is_empty() {
                                lines.push(Line::from(std::mem::take(&mut current_spans)));
                                current_width = 0;
                            }
                            if !lines.is_empty() && continuation_prefix_width > 0 && continuation_prefix_width < available_width {
                                push_span(&mut current_spans, continuation_prefix, Style::default());
                                current_width += continuation_prefix_width;
                            }
                            let mut buf = [0u8; 4];
                            let s = ch.encode_utf8(&mut buf);
                            push_span(&mut current_spans, s, style);
                            current_width += ch_width;
                        }
                    }
                }
            }
        }
    }

    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

pub fn build_transcript_lines(
    messages: &[ChatMessage],
    streaming_text: &str,
    recent_sessions: &[RecentSessionSummary],
    effective_width: usize,
    theme: &Theme,
    config: Option<&TranscriptConfig>,
) -> (Vec<Line<'static>>, Vec<bool>) {
    let mut lines = Vec::new();
    let mut is_user_logical_line = Vec::new();

    let empty_set = std::collections::HashSet::new();
    let expanded_tools = config.map(|c| c.expanded_tools).unwrap_or(&empty_set);
    let selected_tool_id = config.and_then(|c| c.selected_tool_id);
    let active_tools = config.map(|c| c.active_tools).unwrap_or(&[]);
    let streaming_tools = config.map(|c| c.streaming_tools).unwrap_or(&[]);

    let has_conversation = messages
        .iter()
        .any(|m| m.role == Role::User || m.role == Role::Assistant);

    if !has_conversation && streaming_text.is_empty() && active_tools.is_empty() && streaming_tools.is_empty() {
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
                " to cycle supported thinking levels (DEFAULT uses the model default)",
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
        let mut last_assistant_tool_calls: Option<&Vec<dume_provider::types::ToolCall>> = None;
        for msg in messages {
            match msg.role {
                Role::User => {
                    last_assistant_tool_calls = None;
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
                    last_assistant_tool_calls = msg.tool_calls.as_ref();
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
                    let matched_call = msg.tool_call_id.as_ref().and_then(|id| {
                        last_assistant_tool_calls
                            .and_then(|calls| calls.iter().find(|c| &c.id == id))
                    });
                    let (name, args_str) = match matched_call {
                        Some(c) => (c.name.as_str(), c.arguments.as_str()),
                        None => ("tool", ""),
                    };
                    let display = format_tool_display(name, args_str);
                    let is_error = msg.is_error.unwrap_or_else(|| {
                        msg.content.contains("Tool failed")
                            || msg.content.contains("(exit code ")
                            || msg.content.starts_with("Missing or invalid '")
                    });
                    let duration_str = msg.duration_ms.map(format_duration_short);
                    let call_id = msg.tool_call_id.as_deref().unwrap_or("");
                    let is_selected = selected_tool_id.is_some_and(|id| id == call_id);
                    let is_expanded = expanded_tools.contains(call_id);

                    let mut spans = Vec::new();
                    if is_selected {
                        spans.push(Span::styled(
                            "▶ ",
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::raw("  "));
                    }

                    if is_error {
                        spans.push(Span::styled(
                            "✖ ",
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        ));
                        spans.push(Span::styled(
                            format!("Failed · {}", display.target),
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        spans.push(Span::styled(
                            "✔ ",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ));
                        spans.push(Span::styled(
                            format!("{} · {}", display.kind_completed, display.target),
                            Style::default()
                                .fg(theme.foreground)
                                .add_modifier(Modifier::BOLD),
                        ));
                    }

                    if let Some(dur) = duration_str {
                        spans.push(Span::styled(
                            format!(" ({dur})"),
                            Style::default().fg(theme.status_bar_fg),
                        ));
                    }

                    if is_error {
                        let failure_summary = extract_failure_summary(&msg.content);
                        spans.push(Span::styled(
                            format!(" — {failure_summary}"),
                            Style::default().fg(theme.system_msg),
                        ));
                    }

                    if is_selected {
                        if is_expanded {
                            spans.push(Span::styled(
                                " [Ctrl+o to collapse]",
                                Style::default().fg(theme.border),
                            ));
                        } else {
                            spans.push(Span::styled(
                                " [Ctrl+o to expand]",
                                Style::default().fg(theme.border),
                            ));
                        }
                    }

                    for line in wrap_spans(spans, effective_width, "    ") {
                        lines.push(line);
                        is_user_logical_line.push(false);
                    }

                    if is_expanded {
                        for line in msg.content.lines() {
                            let wrapped = wrap_text_line("  │ ", line, effective_width);
                            for w in wrapped {
                                lines.push(Line::from(vec![
                                    Span::styled("  │ ", Style::default().fg(theme.border)),
                                    Span::styled(w, Style::default().fg(theme.status_bar_fg)),
                                ]));
                                is_user_logical_line.push(false);
                            }
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

    // Active tools (in progress or just completed during streaming)
    for active in active_tools {
        let display = format_tool_display(&active.name, &active.arguments);
        let is_selected = selected_tool_id.is_some_and(|id| id == active.call_id.as_str());
        let is_expanded = expanded_tools.contains(&active.call_id);

        let mut spans = Vec::new();
        if is_selected {
            spans.push(Span::styled(
                "▶ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("  "));
        }

        match &active.status {
            ActiveToolStatus::Running { started_at } => {
                spans.push(Span::styled(
                    "● ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!("{} · {}", display.kind_running, display.target),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
                let elapsed = started_at.elapsed().as_millis() as u64;
                if elapsed >= 1000 {
                    spans.push(Span::styled(
                        format!(" ({})", format_duration_short(elapsed)),
                        Style::default().fg(theme.status_bar_fg),
                    ));
                }
            }
            ActiveToolStatus::Completed { duration_ms } => {
                spans.push(Span::styled(
                    "✔ ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!("{} · {}", display.kind_completed, display.target),
                    Style::default()
                        .fg(theme.foreground)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" ({})", format_duration_short(*duration_ms)),
                    Style::default().fg(theme.status_bar_fg),
                ));
            }
            ActiveToolStatus::Failed { duration_ms, error } => {
                spans.push(Span::styled(
                    "✖ ",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!("Failed · {}", display.target),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" ({})", format_duration_short(*duration_ms)),
                    Style::default().fg(theme.status_bar_fg),
                ));
                spans.push(Span::styled(
                    format!(" — {error}"),
                    Style::default().fg(theme.system_msg),
                ));
            }
        }

        if is_selected {
            if is_expanded {
                spans.push(Span::styled(
                    " [Ctrl+o to collapse]",
                    Style::default().fg(theme.border),
                ));
            } else {
                spans.push(Span::styled(
                    " [Ctrl+o to expand]",
                    Style::default().fg(theme.border),
                ));
            }
        }

        for line in wrap_spans(spans, effective_width, "    ") {
            lines.push(line);
            is_user_logical_line.push(false);
        }

        if is_expanded {
            if let Some(output) = &active.output {
                for line in output.lines() {
                    let wrapped = wrap_text_line("  │ ", line, effective_width);
                    for w in wrapped {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(theme.border)),
                            Span::styled(w, Style::default().fg(theme.status_bar_fg)),
                        ]));
                        is_user_logical_line.push(false);
                    }
                }
            }
        }
    }

    // Streaming tool calls (accumulated from ToolCallDelta before ToolStarted)
    for streaming in streaming_tools {
        if active_tools.iter().any(|a| (!a.call_id.is_empty() && a.call_id == streaming.id) || (!streaming.name.is_empty() && a.name == streaming.name)) {
            continue;
        }
        let display = format_tool_display(&streaming.name, &streaming.arguments);
        let call_id = if streaming.id.is_empty() {
            streaming.name.as_str()
        } else {
            streaming.id.as_str()
        };
        let is_selected = selected_tool_id.is_some_and(|id| id == call_id);
        let is_expanded = expanded_tools.contains(call_id);

        let mut spans = Vec::new();
        if is_selected {
            spans.push(Span::styled(
                "▶ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("  "));
        }

        spans.push(Span::styled(
            "● ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{} · {}", display.kind_running, display.target),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));

        if is_selected {
            if is_expanded {
                spans.push(Span::styled(
                    " [Ctrl+o to collapse]",
                    Style::default().fg(theme.border),
                ));
            } else {
                spans.push(Span::styled(
                    " [Ctrl+o to expand]",
                    Style::default().fg(theme.border),
                ));
            }
        }

        for line in wrap_spans(spans, effective_width, "    ") {
            lines.push(line);
            is_user_logical_line.push(false);
        }

        if is_expanded && !streaming.arguments.is_empty() {
            for line in streaming.arguments.lines() {
                let wrapped = wrap_text_line("  │ ", line, effective_width);
                for w in wrapped {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(theme.border)),
                        Span::styled(w, Style::default().fg(theme.status_bar_fg)),
                    ]));
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

    // The untrimmed paragraph wrapper renders whitespace-only lines as two rows.
    for line in &mut lines {
        if line.spans.iter().all(|span| span.content.trim().is_empty()) {
            line.spans.clear();
        }
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
    config: Option<&TranscriptConfig>,
) {
    let effective_width = area.width.max(1) as usize;
    let (lines, is_user_logical_line) = build_transcript_lines(
        messages,
        streaming_text,
        recent_sessions,
        effective_width,
        theme,
        config,
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
    let (lines, is_user) = build_transcript_lines(messages, "", &[], width, theme, None);
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
    let (lines, _) = build_transcript_lines(&[], "", recent_sessions, width, theme, None);
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
    let (lines, _) = build_transcript_lines(&[], streaming_text, &[], effective_width, theme, None);
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
    config: Option<&TranscriptConfig>,
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
        config,
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
    active_phase: Option<&str>,
    queued_badge: Option<&str>,
    anim_tick: usize,
    theme: &Theme,
) {
    let border_color = if is_exit_warned {
        theme.accent
    } else {
        theme.border_focused
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if is_busy {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spinner_char = SPINNER[anim_tick % SPINNER.len()];
        let phase = active_phase.unwrap_or("Working...");

        let left_title = Line::from(vec![
            Span::styled(
                format!(" {} ", spinner_char),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", phase),
                Style::default()
                    .fg(theme.foreground)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        block = block.title(left_title);

        let right_hint = if input_buffer.is_empty() {
            Span::styled(
                " [Esc / Ctrl+C to cancel] ",
                Style::default().fg(theme.border),
            )
        } else {
            Span::styled(
                " [Enter: Steer | Alt+Enter: Follow-up | Esc: Clear] ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
        };
        block = block.title(Line::from(right_hint).right_aligned());
    } else {
        let left_title = if is_exit_warned {
            Line::from(Span::styled(
                " Prompt (Press Ctrl+C again within 2s to exit) ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                " Prompt (Enter to submit, ↑/↓ for history, Esc to clear, Ctrl+C to exit) ",
                Style::default().fg(theme.border_focused),
            ))
        };
        block = block.title(left_title);
    }

    if let Some(badge) = queued_badge {
        block = block.title_bottom(Line::from(vec![
            Span::styled(
                format!(" ↳ {} ", badge),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let mut lines = Vec::new();
    if input_buffer.is_empty() {
        let placeholder = if is_busy {
            "Type to steer active run (Enter) or queue follow-up (Alt+Enter)..."
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(
                "> ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                placeholder,
                Style::default()
                    .fg(theme.border)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    } else {
        let raw_lines: Vec<&str> = input_buffer.split('\n').collect();
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

    // If working, render a smooth luminous dual-wave shimmer along the border
    if is_busy && area.width >= 2 && area.height >= 2 {
        let w = area.width;
        let h = area.height;
        let perimeter = (2 * w + 2 * h).saturating_sub(4) as usize;
        if perimeter > 0 {
            // Period of one full rotation in animation ticks
            let cycle_len = 40.0f32;
            let phase = ((anim_tick as f32) / cycle_len).fract();
            let phase2 = (phase + 0.5).fract();

            let buf = f.buffer_mut();
            let mut apply_shimmer = |x: u16, y: u16, cell_idx: usize| {
                let cell_pos = cell_idx as f32 / perimeter as f32;
                let d1 = (phase - cell_pos).rem_euclid(1.0);
                let d2 = (phase2 - cell_pos).rem_euclid(1.0);
                let tail_len = 0.22f32;
                let i1 = if d1 <= tail_len {
                    (1.0 - d1 / tail_len).powi(2)
                } else {
                    0.0
                };
                let i2 = if d2 <= tail_len {
                    (1.0 - d2 / tail_len).powi(2)
                } else {
                    0.0
                };
                let mut intensity = i1.max(i2);

                let a1 = (cell_pos - phase).rem_euclid(1.0);
                let a2 = (cell_pos - phase2).rem_euclid(1.0);
                if a1 <= 0.03 {
                    intensity = intensity.max(1.0 - a1 / 0.03);
                }
                if a2 <= 0.03 {
                    intensity = intensity.max(1.0 - a2 / 0.03);
                }

                if intensity > 0.02 && x < buf.area.width && y < buf.area.height {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        let sym = cell.symbol();
                        // Only highlight border glyphs without corrupting title text
                        if sym == "─"
                            || sym == "│"
                            || sym == "╭"
                            || sym == "╮"
                            || sym == "╰"
                            || sym == "╯"
                            || sym == "┌"
                            || sym == "┐"
                            || sym == "└"
                            || sym == "┘"
                        {
                            let color = if intensity <= 0.8 {
                                crate::theme::blend_rgb(
                                    theme.border_focused,
                                    theme.accent,
                                    intensity / 0.8,
                                )
                            } else {
                                crate::theme::blend_rgb(
                                    theme.accent,
                                    ratatui::style::Color::Rgb(245, 245, 255),
                                    (intensity - 0.8) / 0.2,
                                )
                            };
                            cell.set_style(cell.style().fg(color));
                            if intensity > 0.85 {
                                cell.set_style(cell.style().add_modifier(Modifier::BOLD));
                            }
                        }
                    }
                }
            };

            let mut idx = 0usize;
            // Top edge (left to right)
            for x in area.x..area.x + w {
                apply_shimmer(x, area.y, idx);
                idx += 1;
            }
            // Right edge (top to bottom, excluding corners)
            for y in area.y + 1..area.y + h - 1 {
                apply_shimmer(area.x + w - 1, y, idx);
                idx += 1;
            }
            // Bottom edge (right to left)
            for x in (area.x..area.x + w).rev() {
                apply_shimmer(x, area.y + h - 1, idx);
                idx += 1;
            }
            // Left edge (bottom to top, excluding corners)
            for y in (area.y + 1..area.y + h - 1).rev() {
                apply_shimmer(area.x, y, idx);
                idx += 1;
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

pub use dume_provider::reasoning::ReasoningEffort as ThinkingLevel;

fn status_model_spans(
    model: &str,
    is_busy: bool,
    anim_tick: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(theme.status_bar_fg)
        .add_modifier(Modifier::BOLD);
    if !is_busy {
        return vec![Span::styled(model.to_owned(), style)];
    }
    let (Color::Rgb(fr, fg, fb), Color::Rgb(br, bg, bb)) =
        (theme.status_bar_fg, theme.status_bar_bg)
    else {
        return vec![Span::styled(model.to_owned(), style)];
    };
    let width = model.width() as f32;
    let half_width = (width * 0.1).max(3.0);
    // One two-second sweep on the existing 80 ms animation clock.
    let position = (anim_tick % 25) as f32 / 25.0 * (width + 2.0 * half_width) - half_width;
    let mut column = 0.0;
    Span::raw(model)
        .styled_graphemes(style)
        .map(|grapheme| {
            let glyph_width = grapheme.symbol.width() as f32;
            let center = column + glyph_width / 2.0;
            column += glyph_width;
            let distance = ((center - position).abs() / half_width).min(1.0);
            let intensity = 0.5 * (1.0 + (std::f32::consts::PI * distance).cos());
            let alpha = 0.5 + 0.5 * intensity;
            let blend = |foreground: u8, background: u8| {
                (f32::from(background) + (f32::from(foreground) - f32::from(background)) * alpha)
                    .round() as u8
            };
            Span::styled(
                grapheme.symbol.to_owned(),
                style.fg(Color::Rgb(blend(fr, br), blend(fg, bg), blend(fb, bb))),
            )
        })
        .collect()
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
        let thinking_label = if thinking == ThinkingLevel::Default {
            "DEF"
        } else {
            thinking.label()
        };
        let details = format!(" {thinking_label} {status} {elapsed} T{usage}");
        let model_name = model.split_once('/').map_or(model, |(_, name)| name);
        let model_width = usize::from(area.width).saturating_sub(details.width());
        let short_model = if model_name.width() <= model_width {
            model_name.to_string()
        } else if model_width == 0 {
            String::new()
        } else {
            let mut name = String::new();
            let mut width = 0;
            for grapheme in Span::raw(model_name).styled_graphemes(Style::default()) {
                let glyph_width = grapheme.symbol.width();
                if width + glyph_width > model_width - 1 {
                    break;
                }
                name.push_str(grapheme.symbol);
                width += glyph_width;
            }
            name.push('…');
            name
        };
        let mut spans = status_model_spans(&short_model, is_busy, anim_tick, theme);
        spans.push(Span::raw(details));
        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.status_bar_bg)),
            area,
        );
        return;
    }
    let thinking_span = Span::styled(
        format!(" Thinking: {} ", thinking.label()),
        Style::default()
            .fg(if thinking == ThinkingLevel::Default {
                theme.border
            } else {
                theme.accent
            })
            .add_modifier(Modifier::BOLD),
    );

    let mut spans = vec![Span::styled(
        " Model: ",
        Style::default()
            .fg(theme.status_bar_fg)
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend(status_model_spans(model, is_busy, anim_tick, theme));
    spans.extend([
        Span::raw(" "),
        Span::styled("|", Style::default().fg(theme.border)),
        thinking_span,
    ]);

    if let Some(version) = available_update {
        spans.push(Span::styled(
            format!(" [UPDATE: v{} available (type /update)] ", version),
            Style::default()
                .fg(theme.system_msg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if is_busy {
        let time_str = match elapsed_secs {
            Some(secs) => format!(" ({:.1}s)", secs),
            None => String::new(),
        };

        spans.push(Span::styled(
            format!(" thinking{} ", time_str),
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
        None
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
    let thinking_style = Style::default()
        .fg(if thinking == ThinkingLevel::Default {
            theme.border
        } else {
            theme.accent
        })
        .add_modifier(Modifier::BOLD);

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
            if let Some(model) = selected_idx
                .checked_sub(model_offset)
                .and_then(|index| models.get(index))
            {
                if model.supported_efforts().len() <= 1 {
                    "   [ Thinking: N/A ]".to_string()
                } else {
                    format!("   [ Tab: Thinking Level = {} ]", thinking.label())
                }
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

pub fn render_logout_selector_modal(
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
        .title(" Select Provider to Log Out (↑/↓, Enter to log out, Esc to cancel) ");

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Choose an LLM provider to log out and remove credentials:",
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
    fn transcript_blank_lines_keep_message_positions_and_backgrounds_aligned() {
        let messages = vec![
            ChatMessage::user("first\n\n    \nlast"),
            ChatMessage::assistant("ASSISTANT_REPLY\n\n    \nASSISTANT_END"),
            ChatMessage::user("NEXT"),
        ];
        let theme = Theme::default();
        for width in [32, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_transcript(
                        frame,
                        frame.area(),
                        &messages,
                        "stream\n\n    \nSTREAM_END",
                        0,
                        &[],
                        &theme,
                        None,
                    );
                })
                .unwrap();
            let rows = rendered_rows(&terminal);
            assert_eq!(rows[4].trim(), "last", "{rows:?}");
            assert_eq!(rows[7].trim(), "ASSISTANT_REPLY", "{rows:?}");
            assert_eq!(rows[10].trim(), "ASSISTANT_END", "{rows:?}");
            assert_eq!(rows[13].trim(), "NEXT", "{rows:?}");
            assert_eq!(rows[19].trim(), "STREAM_END", "{rows:?}");
            let buffer = terminal.backend().buffer();
            for y in 0..24 {
                let highlighted = matches!(y, 0..=5 | 12..=14);
                for x in 0..width {
                    assert_eq!(
                        buffer[(x, y)].bg == theme.user_msg_bg,
                        highlighted,
                        "background at ({x}, {y}), rows: {rows:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_skill_workflows_do_not_push_followups_below_the_viewport() {
        let workflow = include_str!("../../../.dume/skills/deep-interview.md");
        let mut messages = Vec::new();
        for turn in 0..3 {
            messages.push(ChatMessage::user(format!("Request {turn}\n\n{workflow}")));
            messages.push(ChatMessage::assistant(format!("Reply {turn}")));
            messages.push(ChatMessage::user(format!("FOLLOWUP_{turn}")));
        }
        let theme = Theme::default();
        let height = 18;
        for width in [24, 80, 120] {
            let total = calculate_transcript_height(width, &messages, "", 0, None);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_transcript(
                        frame,
                        frame.area(),
                        &messages,
                        "",
                        total.saturating_sub(height),
                        &[],
                        &theme,
                        None,
                    );
                })
                .unwrap();
            let rows = rendered_rows(&terminal);
            assert_eq!(
                rows[usize::from(height - 3)].trim(),
                "FOLLOWUP_2",
                "{rows:?}"
            );
            let buffer = terminal.backend().buffer();
            for x in 0..width {
                assert_eq!(buffer[(x, height - 3)].bg, theme.user_msg_bg);
                assert_ne!(buffer[(x, height - 1)].bg, theme.user_msg_bg);
            }
        }
    }

    #[test]
    fn test_tool_transcript_formatting_folding_and_expansion() {
        let theme = Theme::default();
        let messages = vec![
            ChatMessage::assistant_with_tool_calls(
                "Reading file and running check",
                vec![
                    dume_provider::types::ToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"app.rs\",\"start_line\":4420,\"end_line\":4450}".into(),
                    },
                    dume_provider::types::ToolCall {
                        id: "call_check".into(),
                        name: "bash".into(),
                        arguments: "{\"command\":\"cargo check\"}".into(),
                    },
                ],
            ),
            ChatMessage::tool("line 4420: pub fn foo() {\nline 4421: }\n", "call_read")
                .with_duration_ms(12),
            ChatMessage::tool("Tool failed (exit code 101):\nerror: cannot find value `bar`", "call_check")
                .with_duration_ms(350)
                .with_is_error(true),
        ];

        // 1. Collapsed test (default)
        let empty_set = std::collections::HashSet::new();
        let cfg_collapsed = TranscriptConfig {
            expanded_tools: &empty_set,
            selected_tool_id: None,
            active_tools: &[],
            streaming_tools: &[],
        };
        let (lines, _) = build_transcript_lines(&messages, "", &[], 100, &theme, Some(&cfg_collapsed));
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let full_text = text.join("\n");

        // Tool 1: Success, collapsed to 1 line, showing Read · app.rs:4420–4450 and duration
        assert!(full_text.contains("✔ Read · app.rs:4420–4450"));
        assert!(full_text.contains("(12ms)"));
        // Raw content not present when collapsed
        assert!(!full_text.contains("line 4420: pub fn foo()"));

        // Tool 2: Failed, shows failure reason even when collapsed
        assert!(full_text.contains("✖ Failed · cargo check"));
        assert!(full_text.contains("exit code 101"));
        assert!(full_text.contains("(350ms)"));

        // 2. Expanded test
        let mut expanded_set = std::collections::HashSet::new();
        expanded_set.insert("call_read".to_string());
        let cfg_expanded = TranscriptConfig {
            expanded_tools: &expanded_set,
            selected_tool_id: Some("call_read"),
            active_tools: &[],
            streaming_tools: &[],
        };
        let (lines_exp, _) = build_transcript_lines(&messages, "", &[], 100, &theme, Some(&cfg_expanded));
        let full_text_exp = lines_exp.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");

        // Now contains raw output lines
        assert!(full_text_exp.contains("line 4420: pub fn foo()"));
        // Shows selection indicator
        assert!(full_text_exp.contains("▶"));
        assert!(full_text_exp.contains("Ctrl+o"));
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
                            ThinkingLevel::Default,
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

    fn rendered_status(width: u16, model: &str) -> String {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect::new(0, 0, width, 1),
                    model,
                    ThinkingLevel::Default,
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
            let output = rendered_status(width, "test-model");
            assert!(output.contains("ERROR 3s"));
            assert!(output.contains("S7s"));
            assert!(output.contains("T42+1!"));
        }
    }

    #[test]
    fn model_shimmer_preserves_complete_graphemes_and_static_fallback() {
        let theme = Theme::default();
        for model in ["", "한글모델", "e\u{301}", "👩‍💻"] {
            let spans = status_model_spans(model, true, 12, &theme);
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>(),
                model
            );
            assert_eq!(Line::from(spans.clone()).width(), model.width());
            if model == "e\u{301}" || model == "👩‍💻" {
                assert_eq!(spans.len(), 1);
            }
        }
        assert_eq!(
            status_model_spans("model", false, 0, &theme),
            status_model_spans("model", false, 12, &theme)
        );
        let fallback = Theme {
            status_bar_fg: Color::White,
            ..theme
        };
        assert_eq!(
            status_model_spans("model", true, 0, &fallback),
            status_model_spans("model", true, 12, &fallback)
        );
    }

    #[test]
    fn status_model_shimmer_changes_colors_without_moving_text_or_metrics() {
        for width in [32, 80, 140] {
            let render = |busy, tick| {
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| {
                        render_status_bar(
                            frame,
                            Rect::new(0, 0, width, 1),
                            "openai-codex/gpt-6-astra",
                            ThinkingLevel::Default,
                            busy,
                            Some(3.0),
                            Some("DONE 3s"),
                            Some("7s"),
                            Some("TOKENS 42"),
                            tick,
                            None,
                            &Theme::default(),
                        );
                    })
                    .unwrap();
                terminal.backend().buffer().clone()
            };
            let first = render(true, 0);
            let next = render(true, 12);
            let model_range = if width < 96 { 0..11 } else { 8..32 };
            let cells = |buffer: &ratatui::buffer::Buffer| {
                buffer.content()[model_range.clone()].to_vec()
            };
            let first_cells = cells(&first);
            let next_cells = cells(&next);
            assert_eq!(
                first_cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>(),
                next_cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            );
            assert_ne!(first_cells, next_cells);
            assert!(next_cells.windows(2).any(|pair| pair[0].fg != pair[1].fg));
            assert!(
                first_cells
                    .iter()
                    .chain(&next_cells)
                    .all(|cell| cell.bg == Theme::default().status_bar_bg)
            );
            if width < 96 {
                assert_eq!(&first.content()[11..], &next.content()[11..]);
            } else {
                assert_eq!(&first.content()[..8], &next.content()[..8]);
            }
            assert_eq!(render(false, 0), render(false, 12));
        }
    }

    #[test]
    fn narrow_status_identifies_model_instead_of_only_provider() {
        for width in [40, 80] {
            let output = rendered_status(width, "openai-codex/gpt-6-astra");
            assert!(output.contains("gpt-6-astra"), "{output}");
            assert!(output.contains("ERROR 3s"), "{output}");
            assert!(output.contains("T42+1!"), "{output}");
        }
        let output = rendered_status(32, "opencodex/한글모델이름이아주긴경우");
        // Wide glyphs occupy a symbol cell followed by a blank buffer cell.
        assert!(output.starts_with("한 글 모 …"), "{output}");
        assert!(output.contains("ERROR 3s"), "{output}");
        assert!(output.contains("T42+1!"), "{output}");
        let output = rendered_status(120, "openai-codex/gpt-6-astra");
        assert!(output.contains("Model: openai-codex/gpt-6-astra"));
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

    #[test]
    fn test_narrow_column_long_tool_header_wraps_lines_matching_transcript_height() {
        let theme = Theme::default();
        let long_path = "crates/dume-core/src/submodules/very/deeply/nested/path/to/some_source_file.rs:100–200";
        let messages = vec![
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![dume_provider::types::ToolCall {
                    id: "call_long".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({
                        "path": long_path,
                    }).to_string(),
                }],
            ),
            ChatMessage::tool("Tool failed: path does not exist on disk in current directory", "call_long")
                .with_duration_ms(245)
                .with_is_error(true),
            ChatMessage::assistant("Final response after tool execution."),
        ];

        let empty_set = std::collections::HashSet::new();
        let cfg = TranscriptConfig {
            expanded_tools: &empty_set,
            selected_tool_id: None,
            active_tools: &[],
            streaming_tools: &[],
        };

        let width = 40;
        let height = calculate_transcript_height(width, &messages, "", 0, Some(&cfg));
        let (lines, _) = build_transcript_lines(&messages, "", &[], width as usize, &theme, Some(&cfg));

        // Lines count and calculate_transcript_height must match exactly
        assert_eq!(height as usize, lines.len());

        // Header was split into multiple lines so every line fits within the 40-column boundary
        for (idx, line) in lines.iter().enumerate() {
            let line_width = UnicodeWidthStr::width(line.to_string().as_str());
            assert!(
                line_width <= width as usize,
                "Line {idx} width {line_width} exceeds viewport width {width}: {:?}",
                line.to_string()
            );
        }

        // The final response must be present at the bottom of the transcript
        let all_text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(all_text.contains("Final response after tool execution."));
        let last_line = lines.last().unwrap().to_string();
        assert!(last_line.is_empty() || lines.iter().any(|l| l.to_string().contains("Final response")));
    }

    #[test]
    fn test_active_tool_expansion_renders_output_during_progress() {
        let theme = Theme::default();
        let active_items = vec![crate::tool_display::ActiveToolItem {
            call_id: "active_read".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"crates/dume-tui/src/app.rs\"}".into(),
            status: crate::tool_display::ActiveToolStatus::Completed { duration_ms: 85 },
            output: Some("pub fn run() {\n    // active output content\n}".into()),
        }];

        // Collapsed
        let empty_set = std::collections::HashSet::new();
        let cfg_collapsed = TranscriptConfig {
            expanded_tools: &empty_set,
            selected_tool_id: Some("active_read"),
            active_tools: &active_items,
            streaming_tools: &[],
        };
        let (lines_col, _) = build_transcript_lines(&[], "", &[], 80, &theme, Some(&cfg_collapsed));
        let text_col = lines_col.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text_col.contains("Read · crates/dume-tui/src/app.rs"));
        assert!(!text_col.contains("active output content"));

        // Expanded
        let mut expanded_set = std::collections::HashSet::new();
        expanded_set.insert("active_read".into());
        let cfg_expanded = TranscriptConfig {
            expanded_tools: &expanded_set,
            selected_tool_id: Some("active_read"),
            active_tools: &active_items,
            streaming_tools: &[],
        };
        let (lines_exp, _) = build_transcript_lines(&[], "", &[], 80, &theme, Some(&cfg_expanded));
        let text_exp = lines_exp.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text_exp.contains("Read · crates/dume-tui/src/app.rs"));
        assert!(text_exp.contains("active output content"));
        assert!(text_exp.contains("│"));
    }

    #[test]
    fn test_streaming_tool_call_summary_and_expansion() {
        let theme = Theme::default();
        let streaming_calls = vec![crate::tool_display::StreamingToolCall {
            id: "stream_write".into(),
            name: "write_file".into(),
            arguments: "{\"path\":\"src/main.rs\",\"content\":\"pub fn main() {}\"}".into(),
        }];

        // Collapsed
        let empty_set = std::collections::HashSet::new();
        let cfg_collapsed = TranscriptConfig {
            expanded_tools: &empty_set,
            selected_tool_id: None,
            active_tools: &[],
            streaming_tools: &streaming_calls,
        };
        let (lines_col, _) = build_transcript_lines(&[], "", &[], 80, &theme, Some(&cfg_collapsed));
        let text_col = lines_col.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text_col.contains("Writing · src/main.rs"));
        assert!(!text_col.contains("pub fn main()"));

        // Expanded
        let mut expanded_set = std::collections::HashSet::new();
        expanded_set.insert("stream_write".into());
        let cfg_expanded = TranscriptConfig {
            expanded_tools: &expanded_set,
            selected_tool_id: Some("stream_write"),
            active_tools: &[],
            streaming_tools: &streaming_calls,
        };
        let (lines_exp, _) = build_transcript_lines(&[], "", &[], 80, &theme, Some(&cfg_expanded));
        let text_exp = lines_exp.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text_exp.contains("Writing · src/main.rs"));
        assert!(text_exp.contains("pub fn main()"));
    }
}
