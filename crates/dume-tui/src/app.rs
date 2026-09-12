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
            cursor_pos: 0,
            streaming_text: String::new(),
            scroll_offset: 0,
            is_busy: false,
            model: model.into(),
            theme: Theme::default(),
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.input_buffer.insert(self.cursor_pos, c);
        self.cursor_pos += 1;
    }

    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 && !self.input_buffer.is_empty() {
            self.cursor_pos -= 1;
            self.input_buffer.remove(self.cursor_pos);
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor_pos < self.input_buffer.len() {
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
                    if let Event::Key(key) = event {
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
                            Action::ScrollUp => {
                                app.scroll_offset = app.scroll_offset.saturating_add(1);
                            }
                            Action::ScrollDown => {
                                app.scroll_offset = app.scroll_offset.saturating_sub(1);
                            }
                            Action::SubmitInput => {
                                if !app.input_buffer.trim().is_empty() && !app.is_busy {
                                    let content = std::mem::take(&mut app.input_buffer);
                                    app.cursor_pos = 0;
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
                }
            }
            Some(stream_event) = stream_rx.recv() => {
                match stream_event {
                    StreamEvent::TextDelta(delta) => {
                        app.streaming_text.push_str(&delta);
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
                    _ => {}
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
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let provider = AnthropicProvider::new(&api_key);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        let provider = OpenAiProvider::new(&api_key);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else if let Ok(api_key) = std::env::var("GEMINI_API_KEY") {
        let provider = GeminiProvider::new(&api_key);
        if let Err(e) = provider.stream(model, messages, &[], tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
        }
    } else {
        // Mock fallback if no API key is set
        let _ = tx.send(StreamEvent::TextDelta("Hello from DUM-E native Rust engine! Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GEMINI_API_KEY to stream from cloud providers.".to_string())).await;
        let _ = tx.send(StreamEvent::Completed { finish_reason: "stop".to_string() }).await;
    }
}
