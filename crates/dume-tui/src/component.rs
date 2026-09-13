use crate::theme::Theme;
use dume_provider::types::{ChatMessage, Role};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("   ⚡ ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled("DUM-E Clean-Engine Coding Agent", Style::default().fg(theme.foreground).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::from(Span::styled(
            "   ──────────────────────────────────────────────────────────",
            Style::default().fg(theme.border),
        )));
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Press ", Style::default().fg(theme.foreground)),
            Span::styled("Tab", Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD)),
            Span::styled(" to cycle thinking intensity (OFF / LOW / MED / HIGH)", Style::default().fg(theme.foreground)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Type ", Style::default().fg(theme.foreground)),
            Span::styled("/model", Style::default().fg(theme.assistant_msg).add_modifier(Modifier::BOLD)),
            Span::styled(" to switch LLM models by provider tab", Style::default().fg(theme.foreground)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("   • ", Style::default().fg(theme.accent)),
            Span::styled("Type ", Style::default().fg(theme.foreground)),
            Span::styled("/login", Style::default().fg(theme.assistant_msg).add_modifier(Modifier::BOLD)),
            Span::styled(" to authenticate providers (OpenAI Codex, Claude, Gemini, ChatGPT)", Style::default().fg(theme.foreground)),
        ]));
        lines.push(Line::from(""));
    } else {
        for msg in messages {
            match msg.role {
                Role::User => {
                    // User prompt block: full-width horizontal bar highlight without 'user' text tag
                    for line in msg.content.lines() {
                        let line_display_width = UnicodeWidthStr::width(line);
                        let total_pad = (area.width.saturating_sub(4) as usize).saturating_sub(line_display_width);
                        let padding_spaces = " ".repeat(total_pad);
                        lines.push(
                            Line::from(vec![
                                Span::styled(" ", Style::default().bg(theme.user_msg_bg)),
                                Span::styled(
                                    line,
                                    Style::default()
                                        .fg(theme.user_msg)
                                        .add_modifier(Modifier::BOLD)
                                        .bg(theme.user_msg_bg),
                                ),
                                Span::styled(padding_spaces, Style::default().bg(theme.user_msg_bg)),
                            ])
                            .style(Style::default().bg(theme.user_msg_bg)),
                        );
                    }
                    lines.push(Line::from(""));
                }
                Role::Assistant => {
                    // Assistant response block: clean plain output without 'dum-e' tag
                    for line in msg.content.lines() {
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(line, Style::default().fg(theme.foreground)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Role::Tool => {
                    // Tool call / execution output block
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.user_msg)),
                        Span::styled("tool execution", Style::default().fg(theme.border_focused).add_modifier(Modifier::BOLD)),
                    ]));
                    for line in msg.content.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("│ ", Style::default().fg(theme.border)),
                            Span::styled(line, Style::default().fg(theme.status_bar_fg)),
                        ]));
                    }
                    lines.push(Line::from(""));
                }
                Role::System => {
                    lines.push(Line::from(vec![
                        Span::styled("System: ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                        Span::styled(&msg.content, Style::default().fg(theme.status_bar_fg)),
                    ]));
                    lines.push(Line::from(""));
                }
            }
        }

        // Streaming text (in progress)
        if !streaming_text.is_empty() {
            for line in streaming_text.lines() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line, Style::default().fg(theme.foreground)),
                ]));
            }
            // Streaming cursor indicator
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("▍", Style::default().fg(theme.accent).add_modifier(Modifier::RAPID_BLINK)),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));

    f.render_widget(paragraph, area);
}

/// Calculate the total wrapped line count for the transcript given an inner area width.
pub fn calculate_transcript_height(
    width: u16,
    messages: &[ChatMessage],
    streaming_text: &str,
) -> u16 {
    let effective_width = width.max(1) as usize;
    let mut total_lines: usize = 0;

    if messages.is_empty() && streaming_text.is_empty() {
        return 7;
    }

    let prefix_indent = 2; // "  " or "│ "

    for msg in messages {
        match msg.role {
            Role::User | Role::Assistant => {
                // No extra header line
                for line in msg.content.lines() {
                    let line_len = prefix_indent + UnicodeWidthStr::width(line);
                    let wrapped_count = (line_len + effective_width - 1) / effective_width;
                    total_lines += wrapped_count.max(1);
                }
            }
            Role::Tool | Role::System => {
                // 1 header line ("● tool execution" or "system")
                total_lines += 1;
                for line in msg.content.lines() {
                    let line_len = prefix_indent + line.chars().count();
                    let wrapped_count = (line_len + effective_width - 1) / effective_width;
                    total_lines += wrapped_count.max(1);
                }
            }
        }

        if msg.content.is_empty() {
            total_lines += 1;
        }
        // Blank line between message blocks
        total_lines += 1;
    }

    if !streaming_text.is_empty() {
        // Blinking cursor indicator line
        total_lines += 1;
        for line in streaming_text.lines() {
            let line_len = prefix_indent + line.chars().count();
            let wrapped_count = (line_len + effective_width - 1) / effective_width;
            total_lines += wrapped_count.max(1);
        }
        // Blank line
        total_lines += 1;
    }

    total_lines.min(u16::MAX as usize) as u16
}


pub fn render_input_bar(
    f: &mut Frame,
    area: Rect,
    input_buffer: &str,
    cursor_pos: usize,
    is_exit_warned: bool,
    theme: &Theme,
) {
    let title = if is_exit_warned {
        " Prompt (Press Ctrl+C again within 2s to exit) "
    } else {
        " Prompt (Enter to submit, Esc to clear, Ctrl+C to exit) "
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

    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(input_buffer, Style::default().fg(theme.foreground)),
    ]))
    .block(block);

    f.render_widget(paragraph, area);

    // Position cursor based on visual display width of characters before cursor
    let prefix_width: usize = input_buffer
        .chars()
        .take(cursor_pos)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();

    let x = area.x + 3 + prefix_width as u16;
    let y = area.y + 1;
    if x < area.x + area.width - 1 && y < area.y + area.height {
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

    let item_height = items.len().min(6) as u16;
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
    for (i, item) in items.iter().enumerate().skip(scroll_offset).take(max_visible) {
        let is_selected = i == selected_idx;
        let prefix = if is_selected { "▶ " } else { "  " };

        let name_style = if is_selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.foreground).add_modifier(Modifier::BOLD)
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

        lines.push(Line::from(vec![
            Span::styled(prefix, name_style),
            Span::styled(format!("{:<15}", item.name), name_style),
            Span::styled(format!(" - {}", item.description), desc_style),
        ]).style(bg));
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
    anim_tick: usize,
    theme: &Theme,
) {
    let thinking_span = match thinking {
        ThinkingLevel::Off => Span::styled(" Thinking: OFF ", Style::default().fg(theme.border)),
        ThinkingLevel::Low => Span::styled(" Thinking: LOW ", Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD)),
        ThinkingLevel::Medium => Span::styled(" Thinking: MED ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        ThinkingLevel::High => Span::styled(" Thinking: HIGH ", Style::default().fg(theme.assistant_msg).add_modifier(Modifier::BOLD)),
    };

    let mut spans = vec![
        Span::styled(" DUM-E Clean-Engine ", Style::default().fg(theme.status_bar_fg).add_modifier(Modifier::BOLD)),
        Span::styled(format!("| Model: {} |", model), Style::default().fg(theme.status_bar_fg)),
        thinking_span,
    ];

    if is_busy {
        // Braille spinner frames
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let spinner_char = SPINNER[anim_tick % SPINNER.len()];

        // Ping-pong bouncing progress bar: [···█···]
        const BAR_WIDTH: usize = 9;
        let cycle = (BAR_WIDTH - 1) * 2;
        let pos = (anim_tick / 2) % cycle;
        let active_idx = if pos < BAR_WIDTH {
            pos
        } else {
            cycle - pos
        };

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

        spans.push(Span::styled(
            format!(" {} thinking {} ", spinner_char, bar),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            " [READY] ",
            Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD),
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
    theme: &Theme,
) {
    let width = (area.width.saturating_sub(8)).min(94).max(40);
    let height = (area.height.saturating_sub(4)).min(26).max(14);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(" Select LLM Model (Tab: Thinking, Left/Right: Provider Tab, ↑/↓: Navigate, Esc: Cancel) ");

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
        ThinkingLevel::Low => Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD),
        ThinkingLevel::Medium => Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ThinkingLevel::High => Style::default().fg(theme.assistant_msg).add_modifier(Modifier::BOLD),
    };

    let search_line = Line::from(vec![
        Span::styled("Search: ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(filter, Style::default().fg(theme.foreground)),
        Span::styled(" ▍", Style::default().fg(theme.accent).add_modifier(Modifier::RAPID_BLINK)),
        Span::styled(format!("   [ Tab: Thinking Level = {} ]", thinking.label()), thinking_style),
    ]);
    f.render_widget(Paragraph::new(search_line).block(Block::default().borders(Borders::BOTTOM)), chunks[1]);

    // 3. Model List
    let list_area = chunks[2];
    let max_visible = list_area.height as usize;
    let scroll_offset = if selected_idx >= max_visible {
        selected_idx - (max_visible.saturating_sub(1))
    } else {
        0
    };

    let mut lines = Vec::new();
    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No authenticated models match the current filter or provider.",
            Style::default().fg(theme.border).add_modifier(Modifier::ITALIC),
        )));
    } else {
        for (i, m) in models.iter().enumerate().skip(scroll_offset).take(max_visible) {
            let is_selected = i == selected_idx;
            let is_current = m.id == current_model || format!("{}/{}", m.provider, m.id) == current_model;

            let prefix = if is_selected { "▶ " } else { "  " };
            let tag = if is_current { " (current)" } else { "" };
            let reasoning_badge = if m.reasoning { " [Reasoning]" } else { "" };

            let name_style = if is_selected {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.foreground)
            };

            let prov_style = if is_selected {
                Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.border)
            };

            let bg = if is_selected {
                Style::default().bg(theme.status_bar_bg)
            } else {
                Style::default()
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(format!("{:<15}", m.provider), prov_style),
                Span::styled(format!("{:<32}", m.id), name_style),
                Span::styled(reasoning_badge, Style::default().fg(theme.accent)),
                Span::styled(tag, Style::default().fg(theme.assistant_msg)),
            ]).style(bg));
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
    let height = 14.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(ratatui::widgets::Clear, modal_area);

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
        Style::default().fg(theme.status_bar_fg).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, p) in providers.iter().enumerate() {
        let is_selected = i == selected_idx;
        let prefix = if is_selected { "▶ " } else { "  " };

        let status_span = if p.is_authenticated {
            Span::styled("[Active ✓]  ", Style::default().fg(theme.user_msg).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("[No Auth]   ", Style::default().fg(theme.border))
        };

        let name_style = if is_selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.foreground).add_modifier(Modifier::BOLD)
        };

        let bg = if is_selected {
            Style::default().bg(theme.status_bar_bg)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, name_style),
            status_span,
            Span::styled(format!("{:<18}", p.name), name_style),
            Span::styled(format!("({})", p.auth_type), Style::default().fg(theme.border)),
        ]).style(bg));
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
        .title(format!(" Enter API Key for {} (Esc to cancel) ", provider_name));

    let inner = modal_area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 2,
    });

    let masked: String = input_value.chars().map(|_| '•').collect();

    let lines = vec![
        Line::from(Span::styled(
            format!("Paste your {} API key below and press Enter:", provider_name),
            Style::default().fg(theme.status_bar_fg),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Key: ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
            Span::styled(&masked, Style::default().fg(theme.foreground)),
            Span::styled(" ▍", Style::default().fg(theme.accent).add_modifier(Modifier::RAPID_BLINK)),
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
        .title(format!(" {} Browser Authentication (Esc to cancel) ", provider_name));

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
            Style::default().fg(theme.accent).add_modifier(Modifier::UNDERLINED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(theme.foreground).add_modifier(Modifier::BOLD)),
            Span::styled("Waiting for OAuth authorization in browser...", Style::default().fg(theme.assistant_msg)),
            Span::styled(" ▍", Style::default().fg(theme.accent).add_modifier(Modifier::RAPID_BLINK)),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    f.render_widget(block, modal_area);
    f.render_widget(paragraph, inner);
}



