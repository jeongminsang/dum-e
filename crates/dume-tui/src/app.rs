use crate::component::{render_input_bar, render_status_bar, render_transcript};
use crate::keybinding::{handle_key_event, Action};
use crate::theme::Theme;
use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use dume_provider::types::{ChatMessage, StreamEvent};
use dume_provider::{AnthropicProvider, GeminiProvider, OpenAiProvider};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Terminal;
use std::io;
use tokio::sync::mpsc;

pub struct App {
    pub messages: Vec<ChatMessage>,
    pub input_buffer: String,
    pub cursor_pos: usize,
    pub streaming_text: String,
    pub scroll_offset: u16,
    pub is_busy: bool,
    pub model: String,
    pub theme: Theme,
}

impl App {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0, // In characters, not bytes!
            streaming_text: String::new(),
            scroll_offset: 0,
            is_busy: false,
            model: model.into(),
            theme: Theme::default(),
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        if self.cursor_pos > chars.len() {
            self.cursor_pos = chars.len();
        }
        chars.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
        self.input_buffer = chars.into_iter().collect();
    }

    pub fn delete_char(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        if self.cursor_pos > 0 && !chars.is_empty() {
            self.cursor_pos -= 1;
            if self.cursor_pos < chars.len() {
                chars.remove(self.cursor_pos);
            }
            self.input_buffer = chars.into_iter().collect();
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    pub fn cursor_right(&mut self) {
        let char_count = self.input_buffer.chars().count();
        if self.cursor_pos < char_count {
            self.cursor_pos += 1;
        }
    }
}

pub async fn run_tui(model: &str) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, model).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    model: &str,
) -> Result<()> {
    let mut app = App::new(model);
    let mut reader = EventStream::new();
    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(100);

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),    // Transcript
                    Constraint::Length(3), // Input bar
                    Constraint::Length(1), // Status bar
                ])
                .split(f.area());

            render_transcript(
                f,
                chunks[0],
                &app.messages,
                &app.streaming_text,
                app.scroll_offset,
                &app.theme,
            );
            render_input_bar(
                f,
                chunks[1],
                &app.input_buffer,
                app.cursor_pos,
                &app.theme,
            );
            render_status_bar(
                f,
                chunks[2],
                &app.model,
                app.is_busy,
                &app.theme,
            );
        })?;

        tokio::select! {
            maybe_event = reader.next() => {
                if let Some(Ok(event)) = maybe_event {
                    match event {
                        Event::Key(key) => {
                            match handle_key_event(key) {
                                Action::Quit => break,
                                Action::Clear => {
                                    app.messages.clear();
                                    app.streaming_text.clear();
                                }
                                Action::InsertChar(c) => app.insert_char(c),
                                Action::DeleteChar => app.delete_char(),
                                Action::CursorLeft => app.cursor_left(),
                                Action::CursorRight => app.cursor_right(),
                                Action::CursorHome => app.cursor_pos = 0,
                                Action::CursorEnd => app.cursor_pos = app.input_buffer.len(),
                                Action::ScrollUp => {
                                    app.scroll_offset = app.scroll_offset.saturating_add(1);
                                }
                                Action::ScrollDown => {
                                    app.scroll_offset = app.scroll_offset.saturating_sub(1);
                                }
                                Action::PageUp => {
                                    app.scroll_offset = app.scroll_offset.saturating_add(10);
                                }
                                Action::PageDown => {
                                    app.scroll_offset = app.scroll_offset.saturating_sub(10);
                                }
                                Action::SubmitInput => {
                                    if !app.input_buffer.trim().is_empty() && !app.is_busy {
                                        let content = std::mem::take(&mut app.input_buffer);
                                        app.cursor_pos = 0;

                                        // Slash command handling
                                        let trimmed = content.trim();
                                        if trimmed.starts_with("/model ") {
                                            let new_model = trimmed[7..].trim().to_string();
                                            app.messages.push(ChatMessage::system(format!("Switched model to '{}'", new_model)));
                                            app.model = new_model;
                                            continue;
                                        } else if trimmed == "/clear" {
                                            app.messages.clear();
                                            app.streaming_text.clear();
                                            continue;
                                        } else if trimmed == "/help" {
                                            app.messages.push(ChatMessage::system(
                                                "DUM-E Commands:\n  /model <name>  - Switch active model (e.g. claude-3-5-sonnet, gpt-4o)\n  /clear         - Clear conversation transcript\n  /help          - Show this help\nShortcuts:\n  Ctrl+C / Ctrl+D - Exit\n  Ctrl+L          - Clear screen\n  PageUp/Down     - Scroll transcript\n  Mouse Wheel     - Scroll up/down"
                                            ));
                                            continue;
                                        }

                                        app.messages.push(ChatMessage::user(content.clone()));
                                        app.is_busy = true;

                                        // Spawn streaming task
                                        let model_name = app.model.clone();
                                        let msgs = app.messages.clone();
                                        let tx = stream_tx.clone();

                                        tokio::spawn(async move {
                                            dispatch_stream(&model_name, &msgs, tx).await;
                                        });
                                    }
                                }
                                Action::None => {}
                            }
                        }
                        Event::Mouse(mouse) => {
                            use crossterm::event::MouseEventKind;
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    app.scroll_offset = app.scroll_offset.saturating_add(3);
                                }
                                MouseEventKind::ScrollDown => {
                                    app.scroll_offset = app.scroll_offset.saturating_sub(3);
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(stream_event) = stream_rx.recv() => {
                match stream_event {
                    StreamEvent::TextDelta(delta) => {
                        app.streaming_text.push_str(&delta);
                    }
                    StreamEvent::ToolCallDelta { name, arguments_delta, .. } => {
                        if let Some(tool_name) = name {
                            app.streaming_text.push_str(&format!("\n[Tool Call: {}] ", tool_name));
                        }
                        if !arguments_delta.is_empty() {
                            app.streaming_text.push_str(&arguments_delta);
                        }
                    }
                    StreamEvent::Completed { .. } => {
                        let final_text = std::mem::take(&mut app.streaming_text);
                        app.messages.push(ChatMessage::assistant(final_text));
                        app.is_busy = false;
                    }
                    StreamEvent::Error(err) => {
                        app.messages.push(ChatMessage::system(format!("Error: {}", err)));
                        app.streaming_text.clear();
                        app.is_busy = false;
                    }
                }
            }
        }

    }

    Ok(())
}

async fn dispatch_stream(
    model: &str,
    messages: &[ChatMessage],
    tx: mpsc::Sender<StreamEvent>,
) {
    let cred_store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());

    if let Some(token) = cred_store.resolve_valid_token("anthropic").await {
        let provider = AnthropicProvider::new(&token);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else if let Some(token) = cred_store.resolve_valid_token("openai").await {
        let provider = OpenAiProvider::new(&token);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else if let Some(token) = cred_store.resolve_valid_token("gemini").await {
        let provider = GeminiProvider::new(&token);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else {
        // Fallback or local Ollama check
        let _ = tx.send(StreamEvent::TextDelta("DUM-E native Rust engine connected. (Authenticate with OAuth via `dume login` or set ANTHROPIC_API_KEY / OPENAI_API_KEY / GEMINI_API_KEY).".to_string())).await;
        let _ = tx.send(StreamEvent::Completed { finish_reason: "stop".to_string() }).await;
    }
}
