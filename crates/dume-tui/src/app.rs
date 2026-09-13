use crate::component::{render_input_bar, render_status_bar, render_transcript};
use crate::keybinding::{Action, handle_key_event};
use crate::theme::Theme;
use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dume_provider::runtime::resolve_provider;
use dume_provider::types::{ChatMessage, StreamEvent, ToolCall};
use dume_worker::agent_loop::ToolDispatcher;
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

enum ConversationEvent {
    Stream(StreamEvent),
    Finished {
        messages: Vec<ChatMessage>,
        error: Option<String>,
    },
}

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

    fn receive(&mut self, event: ConversationEvent) {
        match event {
            ConversationEvent::Stream(StreamEvent::TextDelta(delta)) => {
                self.streaming_text.push_str(&delta);
            }
            ConversationEvent::Stream(StreamEvent::ToolCallDelta {
                name,
                arguments_delta,
                ..
            }) => {
                if let Some(name) = name {
                    self.streaming_text
                        .push_str(&format!("\n[Tool Call: {}] ", name));
                }
                self.streaming_text.push_str(&arguments_delta);
            }
            ConversationEvent::Stream(_) => {}
            ConversationEvent::Finished { messages, error } => {
                self.messages = messages;
                if let Some(error) = error {
                    self.messages
                        .push(ChatMessage::system(format!("Error: {}", error)));
                }
                self.streaming_text.clear();
                self.is_busy = false;
            }
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
    let (stream_tx, mut stream_rx) = mpsc::channel::<ConversationEvent>(100);
    let cancellation = CancellationToken::new();
    let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new(
        std::env::current_dir()?,
        model,
        cancellation.clone(),
    )));
    let mut task = None;

    let result: Result<()> = async {
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
                                Action::Clear if !app.is_busy => {
                                    app.messages.clear();
                                    app.streaming_text.clear();
                                }
                                Action::Clear => {}
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
                                                "DUM-E Commands:\n  /model <provider/model> - Switch model (e.g. anthropic/claude-sonnet-4-5, openai-codex/gpt-5.4)\n  /clear         - Clear conversation transcript\n  /help          - Show this help\nShortcuts:\n  Ctrl+C / Ctrl+D - Exit\n  Ctrl+L          - Clear screen\n  PageUp/Down     - Scroll transcript\n  Mouse Wheel     - Scroll up/down"
                                            ));
                                            continue;
                                        }

                                        app.messages.push(ChatMessage::user(content.clone()));
                                        app.is_busy = true;

                                        // Spawn streaming task
                                        let model_name = app.model.clone();
                                        let msgs = app.messages.clone();
                                        let tx = stream_tx.clone();
                                        let dispatcher = dispatcher.clone();
                                        let cancellation = cancellation.clone();

                                        task = Some(tokio::spawn(async move {
                                            dispatch_stream(&model_name, msgs, tx, dispatcher, cancellation).await;
                                        }));
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
                app.receive(stream_event);
            }
        }

      }
      Ok(())
    }.await;

    cancellation.cancel();
    stream_rx.close();
    if let Some(task) = task {
        let _ = task.await;
    }
    dispatcher.lock().await.cancel_all().await;
    result
}

async fn dispatch_stream(
    model: &str,
    mut messages: Vec<ChatMessage>,
    tx: mpsc::Sender<ConversationEvent>,
    dispatcher: Arc<Mutex<ToolDispatcher>>,
    cancellation: CancellationToken,
) {
    let mut dispatcher = dispatcher.lock().await;
    dispatcher.set_model(model);
    let result =
        dispatch_authenticated(model, &mut messages, &tx, &mut dispatcher, &cancellation).await;
    dispatcher.cancel_all().await;
    let _ = tx
        .send(ConversationEvent::Finished {
            messages,
            error: result.err().map(|error| format!("{error:#}")),
        })
        .await;
}

async fn dispatch_authenticated(
    model: &str,
    messages: &mut Vec<ChatMessage>,
    tx: &mpsc::Sender<ConversationEvent>,
    dispatcher: &mut ToolDispatcher,
    cancellation: &CancellationToken,
) -> Result<()> {
    let cred_store =
        dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    let model = model.to_string();
    run_conversation(
        messages,
        tx,
        dispatcher,
        cancellation,
        10,
        move |messages, tx| {
            let cred_store = cred_store.clone();
            let model = model.clone();
            async move {
                let tools = dume_worker::AgentLoop::tool_definitions();
                let provider = resolve_provider(&model, &cred_store).await?;
                provider.stream(&messages, &tools, tx).await
            }
        },
    )
    .await
}

#[derive(Default)]
struct StreamTurn {
    text: String,
    reasoning: Vec<serde_json::Value>,
    calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
}

impl StreamTurn {
    fn push(&mut self, event: &StreamEvent) -> Result<()> {
        if let StreamEvent::Error(error) = event {
            anyhow::bail!("Model streaming error: {}", error);
        }
        anyhow::ensure!(
            self.finish_reason.is_none(),
            "Stream event after completion"
        );
        match event {
            StreamEvent::TextDelta(delta) => self.text.push_str(delta),
            StreamEvent::CodexReasoning(items) => self.reasoning.extend(items.iter().cloned()),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let call = self.calls.entry(*index).or_insert_with(|| ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                if let Some(id) = id.as_ref().filter(|id| !id.is_empty()) {
                    anyhow::ensure!(
                        call.id.is_empty() || call.id == *id,
                        "Conflicting tool call ID"
                    );
                    call.id.clone_from(id);
                }
                if let Some(name) = name.as_ref().filter(|name| !name.is_empty()) {
                    anyhow::ensure!(
                        call.name.is_empty() || call.name == *name,
                        "Conflicting tool name"
                    );
                    call.name.clone_from(name);
                }
                call.arguments.push_str(arguments_delta);
            }
            StreamEvent::Completed { finish_reason } => {
                self.finish_reason = Some(finish_reason.clone())
            }
            StreamEvent::Error(_) => unreachable!(),
        }
        Ok(())
    }

    fn finish(self) -> Result<(ChatMessage, Vec<ToolCall>)> {
        let reason = self
            .finish_reason
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Stream disconnected without completion"))?;
        anyhow::ensure!(
            matches!(
                reason,
                "stop" | "end_turn" | "tool_calls" | "tool_use" | "STOP" | "stop_sequence"
            ),
            "Model output did not complete normally: {}",
            reason,
        );
        let mut ids = std::collections::HashSet::new();
        for call in self.calls.values() {
            anyhow::ensure!(
                !call.id.is_empty() && !call.name.is_empty(),
                "Incomplete tool call"
            );
            anyhow::ensure!(ids.insert(&call.id), "Duplicate tool call ID");
        }
        let calls: Vec<_> = self.calls.into_values().collect();
        let mut message = if calls.is_empty() {
            ChatMessage::assistant(self.text)
        } else {
            ChatMessage::assistant_with_tool_calls(self.text, calls.clone())
        };
        message.codex_reasoning = self.reasoning;
        Ok((message, calls))
    }
}

async fn run_conversation<F, Fut>(
    messages: &mut Vec<ChatMessage>,
    tx: &mpsc::Sender<ConversationEvent>,
    dispatcher: &mut ToolDispatcher,
    cancellation: &CancellationToken,
    max_turns: usize,
    mut stream: F,
) -> Result<()>
where
    F: FnMut(Vec<ChatMessage>, mpsc::Sender<StreamEvent>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    for _ in 0..max_turns {
        let (sub_tx, mut sub_rx) = mpsc::channel(100);
        let collect = async {
            let mut turn = StreamTurn::default();
            while let Some(event) = sub_rx.recv().await {
                turn.push(&event)?;
                if matches!(
                    event,
                    StreamEvent::TextDelta(_) | StreamEvent::ToolCallDelta { .. }
                ) {
                    tx.send(ConversationEvent::Stream(event)).await?;
                }
            }
            turn.finish()
        };
        // Both futures are scoped: errors drop the other side, never detach a producer.
        let (_, (assistant, tool_calls)) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("Conversation cancelled"),
            result = async { tokio::try_join!(stream(messages.clone(), sub_tx), collect) } => result?,
        };
        messages.push(assistant);
        if tool_calls.is_empty() {
            return Ok(());
        }
        for call in tool_calls {
            let name = call.name.clone();
            tx.send(ConversationEvent::Stream(StreamEvent::TextDelta(format!(
                "\n[Tool execution: {}]\n",
                name
            ))))
            .await?;
            messages.push(dispatcher.execute(call).await?);
            tx.send(ConversationEvent::Stream(StreamEvent::TextDelta(format!(
                "[Finished {}]\n",
                name
            ))))
            .await?;
        }
    }
    anyhow::bail!("Conversation turn limit reached ({})", max_turns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dume_provider::types::Role;
    use std::collections::VecDeque;

    fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: &str) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            arguments_delta: args.to_string(),
        }
    }

    fn completed(reason: &str) -> StreamEvent {
        StreamEvent::Completed {
            finish_reason: reason.to_string(),
        }
    }

    #[test]
    fn reasoning_only_turn_is_preserved_without_display() {
        let reasoning = serde_json::json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-secret"});
        let event = StreamEvent::CodexReasoning(vec![reasoning.clone()]);
        let mut turn = StreamTurn::default();
        turn.push(&event).unwrap();
        turn.push(&completed("stop")).unwrap();
        let (message, calls) = turn.finish().unwrap();
        assert!(calls.is_empty());
        assert!(message.content.is_empty());
        assert_eq!(message.codex_reasoning, vec![reasoning]);
        let mut app = App::new("test");
        app.receive(ConversationEvent::Stream(event));
        assert!(app.streaming_text.is_empty());
    }

    async fn scripted(
        messages: &mut Vec<ChatMessage>,
        dispatcher: &mut ToolDispatcher,
        scripts: Vec<(Vec<StreamEvent>, bool)>,
        max_turns: usize,
    ) -> (Result<()>, Vec<Vec<ChatMessage>>, Vec<ConversationEvent>) {
        let (tx, mut rx) = mpsc::channel(1000);
        let mut scripts = VecDeque::from(scripts);
        let mut requests = Vec::new();
        let result = run_conversation(
            messages,
            &tx,
            dispatcher,
            &CancellationToken::new(),
            max_turns,
            |history, tx| {
                requests.push(history);
                let (events, fail) = scripts.pop_front().expect("unexpected model request");
                async move {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    anyhow::ensure!(!fail, "provider request failed");
                    Ok(())
                }
            },
        )
        .await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        (result, requests, events)
    }

    #[tokio::test]
    async fn interleaved_tools_keep_canonical_history_across_user_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "test", CancellationToken::new());
        let mut messages = vec![ChatMessage::user("write then read")];
        let reasoning = serde_json::json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-secret"});
        let scripts = vec![
            (
                vec![
                    StreamEvent::CodexReasoning(vec![reasoning.clone()]),
                    delta(9, Some("read"), Some("read_file"), "{\"path\":"),
                    delta(
                        2,
                        Some("write"),
                        Some("write_file"),
                        "{\"path\":\"result.txt\",",
                    ),
                    delta(9, None, None, "\"result.txt\"}"),
                    delta(
                        2,
                        Some("write"),
                        Some("write_file"),
                        "\"content\":\"hello\"}",
                    ),
                    completed("tool_calls"),
                ],
                false,
            ),
            (
                vec![StreamEvent::TextDelta("Done.".into()), completed("stop")],
                false,
            ),
        ];
        let (result, requests, events) =
            scripted(&mut messages, &mut dispatcher, scripts, 10).await;
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
            "hello"
        );
        let history = &requests[1];
        assert_eq!(history[1].codex_reasoning, vec![reasoning]);
        let calls = history[1].tool_calls.as_ref().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            ["write", "read"]
        );
        assert_eq!(history[2].tool_call_id.as_deref(), Some("write"));
        assert!(history[2].content.contains("Successfully wrote"));
        assert_eq!(history[3], ChatMessage::tool("hello", "read"));
        assert_eq!(messages.last().unwrap(), &ChatMessage::assistant("Done."));

        let mut app = App::new("test");
        app.is_busy = true;
        for event in events {
            app.receive(event);
        }
        assert!(app.streaming_text.contains("[Tool Call: write_file]"));
        assert!(!app.streaming_text.contains("opaque-secret"));
        app.receive(ConversationEvent::Finished {
            messages: messages.clone(),
            error: None,
        });
        assert!(!app.is_busy);
        assert!(app.streaming_text.is_empty());
        assert_eq!(app.messages, messages);
        app.messages.push(ChatMessage::user("what did you write?"));
        let expected = app.messages.clone();
        let (result, requests, _) = scripted(
            &mut app.messages,
            &mut dispatcher,
            vec![(
                vec![StreamEvent::TextDelta("hello".into()), completed("stop")],
                false,
            )],
            10,
        )
        .await;
        result.unwrap();
        assert_eq!(requests[0], expected);
        assert!(requests[0].iter().any(|message| message.role == Role::Tool));
        assert!(
            !requests[0]
                .iter()
                .any(|message| message.content.contains("[Tool execution:"))
        );
    }

    #[tokio::test]
    async fn failed_or_truncated_streams_never_execute_collected_calls() {
        for (suffix, provider_error) in [
            (
                vec![StreamEvent::Error("broken".into()), completed("stop")],
                false,
            ),
            (
                vec![completed("stop"), StreamEvent::Error("late failure".into())],
                false,
            ),
            (vec![], false),
            (vec![completed("length")], false),
            (vec![completed("max_tokens")], false),
            (vec![completed("content_filter")], false),
            (vec![completed("stop")], true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut dispatcher = ToolDispatcher::new(dir.path(), "test", CancellationToken::new());
            let mut messages = vec![ChatMessage::user("write")];
            let original = messages.clone();
            let mut events = vec![delta(
                0,
                Some("write"),
                Some("write_file"),
                "{\"path\":\"bad.txt\",\"content\":\"bad\"}",
            )];
            events.extend(suffix);
            let (result, requests, _) = scripted(
                &mut messages,
                &mut dispatcher,
                vec![(events, provider_error)],
                10,
            )
            .await;
            assert!(result.is_err());
            assert_eq!(requests.len(), 1);
            assert_eq!(messages, original);
            assert!(!dir.path().join("bad.txt").exists());
        }
    }

    #[tokio::test]
    async fn malformed_arguments_return_correlated_errors_without_side_effects() {
        for args in [
            "{",
            "null",
            "{\"path\":\"bad.txt\"}",
            "{\"path\":4,\"content\":\"bad\"}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut dispatcher = ToolDispatcher::new(dir.path(), "test", CancellationToken::new());
            let mut messages = vec![ChatMessage::user("write")];
            let (result, requests, _) = scripted(
                &mut messages,
                &mut dispatcher,
                vec![
                    (
                        vec![
                            delta(0, Some("invalid"), Some("write_file"), args),
                            completed("tool_calls"),
                        ],
                        false,
                    ),
                    (vec![completed("stop")], false),
                ],
                10,
            )
            .await;
            result.unwrap();
            let response = &requests[1][2];
            assert_eq!(response.tool_call_id.as_deref(), Some("invalid"));
            assert!(response.content.contains("error"));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn turn_limit_is_terminal_and_keeps_completed_tool_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "test", CancellationToken::new());
        let mut messages = vec![ChatMessage::user("keep working")];
        let scripts = (0..10)
            .map(|index| {
                (
                    vec![
                        delta(
                            0,
                            Some(&format!("call_{index}")),
                            Some("read_file"),
                            "{\"path\":\"missing\"}",
                        ),
                        completed("tool_calls"),
                    ],
                    false,
                )
            })
            .collect();
        let (result, requests, _) = scripted(&mut messages, &mut dispatcher, scripts, 10).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("turn limit reached (10)"));
        assert_eq!(requests.len(), 10);
        assert_eq!(messages.len(), 21);
        let mut app = App::new("test");
        app.is_busy = true;
        app.receive(ConversationEvent::Finished {
            messages,
            error: Some(error),
        });
        assert!(!app.is_busy);
        assert_eq!(app.messages[20].tool_call_id.as_deref(), Some("call_9"));
        assert!(app.messages[21].content.contains("turn limit"));
    }

    #[tokio::test]
    async fn advertised_subagent_tools_use_worker_dispatch_and_isolation() {
        // Not a Git repository: the real dispatcher must report isolation failure,
        // without reaching credentials or a provider.
        let dir = tempfile::tempdir().unwrap();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "test", CancellationToken::new());
        let mut messages = vec![ChatMessage::user("delegate")];
        let (result, requests, _) = scripted(
            &mut messages,
            &mut dispatcher,
            vec![
                (
                    vec![
                        delta(
                            0,
                            Some("spawn"),
                            Some("spawn_subagent"),
                            "{\"id\":\"child\",\"prompt\":\"work\",\"sub_dir\":\"child\"}",
                        ),
                        delta(
                            1,
                            Some("wait"),
                            Some("wait_subagent"),
                            "{\"id\":\"child\",\"timeout_ms\":5000}",
                        ),
                        delta(
                            2,
                            Some("cancel"),
                            Some("cancel_subagent"),
                            "{\"id\":\"child\"}",
                        ),
                        completed("tool_calls"),
                    ],
                    false,
                ),
                (vec![completed("stop")], false),
            ],
            10,
        )
        .await;
        dispatcher.cancel_all().await;
        result.unwrap();
        assert!(requests[1][2].content.contains("scheduled"));
        assert!(requests[1][3].content.contains("Subagent isolation failed"));
        assert!(requests[1][4].content.contains("stopped"));
        assert!(!dir.path().join("child/.git").exists());
    }

    #[test]
    fn incomplete_or_conflicting_tool_metadata_is_rejected() {
        for events in [
            vec![delta(0, None, Some("bash"), "{}")],
            vec![delta(0, Some("id"), None, "{}")],
            vec![
                delta(0, Some("id"), Some("bash"), "{}"),
                delta(0, Some("other"), None, ""),
            ],
            vec![
                delta(0, Some("id"), Some("bash"), "{}"),
                delta(1, Some("id"), Some("bash"), "{}"),
            ],
        ] {
            let mut turn = StreamTurn::default();
            let result = events
                .iter()
                .try_for_each(|event| turn.push(event))
                .and_then(|_| turn.push(&completed("tool_calls")))
                .and_then(|_| turn.finish().map(|_| ()));
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_drops_live_stream_without_executing_pending_calls() {
        let dir = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "test", cancellation.clone());
        let mut messages = vec![ChatMessage::user("write")];
        let (tx, _rx) = mpsc::channel(100);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct OnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let result = run_conversation(
            &mut messages,
            &tx,
            &mut dispatcher,
            &cancellation,
            10,
            |_, tx| {
                let token = cancellation.clone();
                let dropped = dropped.clone();
                async move {
                    let _guard = OnDrop(dropped);
                    tx.send(delta(
                        0,
                        Some("write"),
                        Some("write_file"),
                        "{\"path\":\"bad.txt\",\"content\":\"bad\"}",
                    ))
                    .await
                    .unwrap();
                    token.cancel();
                    std::future::pending::<Result<()>>().await
                }
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(messages.len(), 1);
        assert!(!dir.path().join("bad.txt").exists());
    }
}
