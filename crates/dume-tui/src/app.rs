use crate::component::{
    AutocompleteItem, LoginProviderChoice, ThinkingLevel, calculate_transcript_height,
    render_api_key_modal, render_autocomplete_dropdown, render_btw_modal, render_input_bar,
    render_login_selector_modal, render_model_selector_modal, render_oauth_waiting_modal,
    render_status_bar, render_transcript, render_update_modal,
};
use crate::keybinding::{Action, handle_key_event};
use crate::theme::Theme;
use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dume_core::skills::SkillRegistry;
use dume_core::updater::UpdateInfo;
use dume_provider::ModelInfo;
use dume_provider::runtime::resolve_provider;
use dume_provider::types::{ChatMessage, StreamEvent, ToolCall};
use dume_store::{HarnessStore, RecentSessionSummary};
use dume_worker::agent_loop::ToolDispatcher;
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

fn save_credential_and_rotate_session(
    store: &dume_provider::CredentialStore,
    provider: &str,
    key: &str,
    session_id: &mut String,
) -> Result<()> {
    store.save_credential(provider, key)?;
    *session_id = dume_provider::types::StreamRequestContext::new().session_id;
    Ok(())
}

fn delete_credential_and_rotate_session(
    store: &dume_provider::CredentialStore,
    provider: &str,
    session_id: &mut String,
) -> Result<()> {
    store.delete(provider)?;
    *session_id = dume_provider::types::StreamRequestContext::new().session_id;
    Ok(())
}

enum ConversationEvent {
    Stream(StreamEvent),
    Finished {
        messages: Vec<ChatMessage>,
        error: Option<String>,
        session_id: Option<String>,
    },
    OAuthComplete {
        provider: String,
        result: Result<()>,
    },
    UpdateAvailable(UpdateInfo),
    UpdateProgress {
        message: String,
    },
    UpdateFinished {
        result: Result<std::path::PathBuf, String>,
    },
    BtwStreamDelta(String),
    BtwFinished(Result<String, String>),
    CatalogRefreshFinished(Result<Option<usize>, String>),
}

fn selectable_models() -> Vec<ModelInfo> {
    let store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    let authenticated: BTreeSet<_> = [
        "anthropic", "openai", "google", "deepseek", "openai-codex", "opencode", "opencode-go",
    ]
    .into_iter()
    .filter(|provider| store.has_credential(provider))
    .collect();
    dume_provider::ModelCatalog::list_all_models()
        .unwrap_or_default()
        .into_iter()
        .filter(|model| {
            dume_provider::is_model_supported(model) && authenticated.contains(model.provider.as_str())
        })
        .collect()
}

fn spawn_codex_catalog_refresh(tx: mpsc::Sender<ConversationEvent>) {
    tokio::spawn(async move {
        let result = dume_provider::ModelCatalog::refresh_codex_cache_if_stale()
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx.send(ConversationEvent::CatalogRefreshFinished(result)).await;
    });
}

pub enum ModalState {
    None,
    ModelSelector {
        models: Vec<ModelInfo>,
        filtered: Vec<ModelInfo>,
        selected_idx: usize,
        filter: String,
        provider_tabs: Vec<String>,
        active_tab_idx: usize,
    },
    LoginSelector {
        selected_idx: usize,
    },
    ApiKeyInput {
        provider_id: String,
        provider_name: String,
        input: String,
    },
    CodexOAuthWaiting {
        url: String,
        cancel_token: CancellationToken,
    },
    Update {
        info: UpdateInfo,
        status_text: String,
        in_progress: bool,
    },
    BtwChat {
        history: Vec<(String, String)>,
        input: String,
        is_streaming: bool,
        streaming_reply: String,
    },
}

pub struct App {
    pub messages: Vec<ChatMessage>,
    pub input_buffer: String,
    pub cursor_pos: usize,
    pub streaming_text: String,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub is_busy: bool,
    pub model: String,
    pub thinking: ThinkingLevel,
    pub theme: Theme,
    pub skills: SkillRegistry,
    pub autocomplete_index: usize,
    pub autocomplete_dismissed: bool,
    pub modal: ModalState,
    pub last_ctrl_c: Option<std::time::Instant>,
    pub available_update: Option<UpdateInfo>,
    pub session_usage: (i64, i64, i64, i64, i64), // (input, output, total, cache_read, cache_write)
    pub current_turn_usage: dume_provider::types::TokenUsage,
    pub session_id: String,
    pub prompt_history: Vec<String>,
    pub history_index: Option<usize>,
    pub temp_input: String,
    pub busy_started_at: Option<std::time::Instant>,
    pub recent_sessions: Vec<RecentSessionSummary>,
}

fn get_persisted_or_default_model(model: Option<&str>) -> String {
    if let Some(m) = model {
        if !m.is_empty() {
            return m.to_string();
        }
    }

    // Check last-used model from ~/.dume/agent/models-store.json or settings
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let store_path = std::path::PathBuf::from(home).join(".dume/agent/models-store.json");
    if let Ok(content) = std::fs::read_to_string(&store_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(last_model) = val.get("last_used_model").and_then(|v| v.as_str()) {
                if !last_model.trim().is_empty() {
                    return last_model.to_string();
                }
            }
        }
    }

    // Default to gpt 5.6 luna (openai-codex/gpt-5.6-luna)
    "openai-codex/gpt-5.6-luna".to_string()
}

pub fn persist_last_used_model(model: &str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let store_path = std::path::PathBuf::from(home).join(".dume/agent/models-store.json");
    let mut obj = if let Ok(content) = std::fs::read_to_string(&store_path) {
        serde_json::from_str::<serde_json::Value>(&content)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if let Some(map) = obj.as_object_mut() {
        map.insert("last_used_model".to_string(), serde_json::json!(model));
    }
    if let Some(parent) = store_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string_pretty(&obj) {
        let _ = std::fs::write(&store_path, serialized);
    }
}

fn load_recent_sessions_overview() -> Vec<RecentSessionSummary> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let db_path = std::path::PathBuf::from(home).join(".dume/agent/harness.db");
    let artifacts_dir = std::path::PathBuf::from(".dume/artifacts");
    if let Ok(store) = HarnessStore::open(&db_path, &artifacts_dir) {
        store.list_recent_sessions(5).unwrap_or_default()
    } else {
        Vec::new()
    }
}

impl App {
    pub fn new(model: impl Into<String>) -> Self {
        let m = model.into();
        let effective_model = if m == "anthropic/claude-sonnet-4-5" || m.is_empty() {
            get_persisted_or_default_model(None)
        } else {
            get_persisted_or_default_model(Some(&m))
        };

        let recent_sessions = load_recent_sessions_overview();

        Self {
            messages: Vec::new(),
            input_buffer: String::new(),
            cursor_pos: 0, // In characters, not bytes!
            streaming_text: String::new(),
            scroll_offset: 0,
            auto_scroll: true,
            is_busy: false,
            model: effective_model,
            thinking: ThinkingLevel::Off,
            theme: Theme::default(),
            skills: SkillRegistry::load_default(),
            autocomplete_index: 0,
            autocomplete_dismissed: false,
            modal: ModalState::None,
            last_ctrl_c: None,
            available_update: None,
            session_usage: (0, 0, 0, 0, 0),
            current_turn_usage: dume_provider::types::TokenUsage::default(),
            session_id: dume_provider::types::StreamRequestContext::new().session_id,
            prompt_history: Vec::new(),
            history_index: None,
            temp_input: String::new(),
            busy_started_at: None,
            recent_sessions,
        }
    }

    /// Return filtered autocomplete suggestions when input starts with '/'
    pub fn get_autocomplete_items(&self) -> Vec<AutocompleteItem> {
        if self.autocomplete_dismissed || !self.input_buffer.starts_with('/') {
            return Vec::new();
        }

        // Only show autocomplete when user hasn't typed arguments yet
        let trimmed = self.input_buffer.trim_start();
        if trimmed.contains(' ') {
            return Vec::new();
        }

        let query = trimmed.strip_prefix('/').unwrap_or("").to_lowercase();

        let mut items = Vec::new();

        // Built-in commands
        let builtins = [
            (
                "/model",
                "Switch model (e.g. /model anthropic/claude-sonnet-4-5)",
            ),
            (
                "/login",
                "Save provider API key (e.g. /login anthropic <key>)",
            ),
            (
                "/logout",
                "Clear saved provider credentials (e.g. /logout anthropic)",
            ),
            ("/update", "Check and install DUM-E in-place update"),
            ("/usage", "Display session token usage statistics"),
            (
                "/btw",
                "Isolated side-question chat without tools or session pollution",
            ),
            ("/clear", "Clear conversation transcript"),
            ("/skills", "List all available skills"),
            ("/help", "Show help and command list"),
        ];

        for (cmd, desc) in builtins {
            if query.is_empty() || cmd.strip_prefix('/').unwrap_or("").starts_with(&query) {
                items.push(AutocompleteItem {
                    name: cmd.to_string(),
                    description: desc.to_string(),
                });
            }
        }

        // Skills from registry
        for skill in self.skills.list() {
            let slash_name = format!("/{}", skill.name);
            if query.is_empty() || skill.name.to_lowercase().starts_with(&query) {
                items.push(AutocompleteItem {
                    name: slash_name,
                    description: skill.description.clone(),
                });
            }
        }

        items
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

    pub fn navigate_history_up(&mut self) {
        if self.prompt_history.is_empty() {
            return;
        }
        let new_idx = match self.history_index {
            None => {
                self.temp_input = self.input_buffer.clone();
                self.prompt_history.len().saturating_sub(1)
            }
            Some(idx) => idx.saturating_sub(1),
        };
        self.history_index = Some(new_idx);
        if let Some(hist) = self.prompt_history.get(new_idx) {
            self.input_buffer = hist.clone();
            self.cursor_pos = self.input_buffer.chars().count();
        }
    }

    pub fn navigate_history_down(&mut self) {
        if let Some(idx) = self.history_index {
            if idx + 1 < self.prompt_history.len() {
                let new_idx = idx + 1;
                self.history_index = Some(new_idx);
                if let Some(hist) = self.prompt_history.get(new_idx) {
                    self.input_buffer = hist.clone();
                    self.cursor_pos = self.input_buffer.chars().count();
                }
            } else {
                self.history_index = None;
                self.input_buffer = std::mem::take(&mut self.temp_input);
                self.cursor_pos = self.input_buffer.chars().count();
            }
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
            ConversationEvent::Stream(StreamEvent::Usage(usage)) => {
                self.current_turn_usage.merge_cumulative(&usage);
            }
            ConversationEvent::Stream(StreamEvent::Completed { .. }) => {
                self.session_usage.0 += self.current_turn_usage.input_tokens;
                self.session_usage.1 += self.current_turn_usage.output_tokens;
                self.session_usage.2 += self.current_turn_usage.total_tokens;
                if let Some(r) = self.current_turn_usage.cache_read_tokens {
                    self.session_usage.3 += r;
                }
                if let Some(w) = self.current_turn_usage.cache_write_tokens {
                    self.session_usage.4 += w;
                }
                self.current_turn_usage = dume_provider::types::TokenUsage::default();
            }
            ConversationEvent::Stream(_) => {}
            ConversationEvent::Finished { messages, error, session_id } => {
                if let Some(session_id) = session_id {
                    self.session_id = session_id;
                }
                if self.current_turn_usage.input_tokens > 0 || self.current_turn_usage.output_tokens > 0 {
                    self.session_usage.0 += self.current_turn_usage.input_tokens;
                    self.session_usage.1 += self.current_turn_usage.output_tokens;
                    self.session_usage.2 += self.current_turn_usage.total_tokens;
                    if let Some(r) = self.current_turn_usage.cache_read_tokens {
                        self.session_usage.3 += r;
                    }
                    if let Some(w) = self.current_turn_usage.cache_write_tokens {
                        self.session_usage.4 += w;
                    }
                    self.current_turn_usage = dume_provider::types::TokenUsage::default();
                }
                self.messages = messages;
                if let Some(error) = error {
                    self.messages
                        .push(ChatMessage::ui_notice(format!("Error: {}", error)));
                }
                self.streaming_text.clear();
                self.is_busy = false;
                self.busy_started_at = None;
            }
            ConversationEvent::OAuthComplete { provider, result } => {
                match result {
                    Ok(()) => {
                        self.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                        self.messages.push(ChatMessage::ui_notice(format!(
                            "Successfully authenticated {}! You can now use this provider.",
                            provider
                        )));
                    }
                    Err(e) => {
                        self.messages.push(ChatMessage::ui_notice(format!(
                            "Authentication failed for {}: {}",
                            provider, e
                        )));
                    }
                }
                self.modal = ModalState::None;
            }
            ConversationEvent::UpdateAvailable(info) => {
                self.available_update = Some(info.clone());
                self.messages.push(ChatMessage::ui_notice(format!(
                    "Update available: v{} (Current: v{}). Type /update to install in-place.",
                    info.latest_version, info.current_version
                )));
            }
            ConversationEvent::UpdateProgress { message } => {
                if let ModalState::Update {
                    status_text,
                    in_progress,
                    ..
                } = &mut self.modal
                {
                    *status_text = message;
                    *in_progress = true;
                }
            }
            ConversationEvent::UpdateFinished { result } => {
                match result {
                    Ok(path) => {
                        self.messages.push(ChatMessage::ui_notice(format!(
                            "Successfully updated DUM-E in-place to the latest version! (Binary: {})",
                            path.display()
                        )));
                        if let Some(info) = &self.available_update {
                            self.messages.push(ChatMessage::ui_notice(format!(
                                "DUM-E is now running on updated binary (v{}). Your session remains active.",
                                info.latest_version
                            )));
                        }
                        self.available_update = None;
                    }
                    Err(e) => {
                        self.messages.push(ChatMessage::ui_notice(format!(
                            "In-place update failed: {}",
                            e
                        )));
                    }
                }
                self.modal = ModalState::None;
            }
            ConversationEvent::BtwStreamDelta(delta) => {
                if let ModalState::BtwChat {
                    streaming_reply,
                    is_streaming,
                    ..
                } = &mut self.modal
                {
                    streaming_reply.push_str(&delta);
                    *is_streaming = true;
                }
            }
            ConversationEvent::BtwFinished(res) => {
                if let ModalState::BtwChat {
                    history,
                    is_streaming,
                    streaming_reply,
                    ..
                } = &mut self.modal
                {
                    *is_streaming = false;
                    let reply = match res {
                        Ok(text) => text,
                        Err(e) => format!("Error: {}", e),
                    };
                    history.push(("assistant".to_string(), reply));
                    streaming_reply.clear();
                }
            }
            ConversationEvent::CatalogRefreshFinished(result) => match result {
                Ok(Some(count)) => {
                    self.messages.push(ChatMessage::ui_notice(format!(
                        "Model catalog updated ({count} models)."
                    )));
                    if let ModalState::ModelSelector {
                        models,
                        filtered,
                        selected_idx,
                        filter,
                        provider_tabs,
                        active_tab_idx,
                    } = &mut self.modal {
                        let selected = filtered.get(*selected_idx).map(|m| (m.provider.clone(), m.id.clone()));
                        let active_provider = provider_tabs.get(*active_tab_idx).cloned().unwrap_or_default();
                        *models = selectable_models();
                        *provider_tabs = std::iter::once("All".to_string())
                            .chain(models.iter().map(|m| m.provider.clone()))
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        if let Some(index) = provider_tabs.iter().position(|p| p == &active_provider) {
                            *active_tab_idx = index;
                        } else {
                            *active_tab_idx = 0;
                        }
                        let tab = &provider_tabs[*active_tab_idx];
                        let query = filter.to_lowercase();
                        *filtered = models.iter().filter(|m| {
                            (tab == "All" || &m.provider == tab)
                                && (m.id.to_lowercase().contains(&query)
                                    || m.provider.to_lowercase().contains(&query))
                        }).cloned().collect();
                        *selected_idx = selected
                            .and_then(|(provider, id)| filtered.iter().position(|m| m.provider == provider && m.id == id))
                            .unwrap_or(0);
                    }
                }
                Ok(None) => {}
                Err(error) => self.messages.push(ChatMessage::ui_notice(format!(
                    "Model catalog refresh failed; using local models: {error}"
                ))),
            },
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let db_path = std::path::PathBuf::from(home).join(".dume/agent/harness.db");
    let artifacts_dir = std::path::PathBuf::from(".dume/artifacts");
    let harness_store = Arc::new(HarnessStore::open(&db_path, &artifacts_dir)?);
    let mut raw_dispatcher = ToolDispatcher::new(
        std::env::current_dir()?,
        model,
        cancellation.clone(),
    );
    raw_dispatcher = raw_dispatcher.with_store(harness_store);
    let dispatcher = Arc::new(Mutex::new(raw_dispatcher));
    let mut task = None;
    let mut current_stream_cancel: Option<CancellationToken> = None;
    let mut anim_tick: usize = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));

    // Spawn non-blocking background task to check for latest release
    let update_tx = stream_tx.clone();
    tokio::spawn(async move {
        let current_version = env!("CARGO_PKG_VERSION");
        let repo = "jeongminsang/dum-e";
        if let Ok(Some(info)) = dume_core::updater::check_for_update(repo, current_version) {
            let _ = update_tx
                .send(ConversationEvent::UpdateAvailable(info))
                .await;
        }
    });

    // Spawn non-blocking background catalog refresh
    let catalog_refresh = dume_provider::ModelCatalog::refresh_remote_catalog_background();
    let catalog_tx = stream_tx.clone();
    tokio::spawn(async move {
        let result = catalog_refresh
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| format!("{error:#}")));
        let _ = catalog_tx.send(ConversationEvent::CatalogRefreshFinished(result)).await;
    });
    spawn_codex_catalog_refresh(stream_tx.clone());

    let result: Result<()> = async {
      loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),    // Transcript
                    Constraint::Length(5), // Input bar
                    Constraint::Length(1), // Status bar
                ])
                .split(f.area());

            let transcript_area = chunks[0];
            let inner_width = transcript_area.width;
            let inner_height = transcript_area.height;
            let total_lines = calculate_transcript_height(
                inner_width,
                &app.messages,
                &app.streaming_text,
                app.recent_sessions.len(),
            );
            let max_scroll = total_lines.saturating_sub(inner_height);

            if app.auto_scroll {
                app.scroll_offset = max_scroll;
            } else if app.scroll_offset > max_scroll {
                app.scroll_offset = max_scroll;
            }

            render_transcript(
                f,
                transcript_area,
                &app.messages,
                &app.streaming_text,
                app.scroll_offset,
                &app.recent_sessions,
                &app.theme,
            );
            let is_exit_warned = app.last_ctrl_c.map_or(false, |t| t.elapsed() < std::time::Duration::from_secs(2));
            render_input_bar(
                f,
                chunks[1],
                &app.input_buffer,
                app.cursor_pos,
                is_exit_warned,
                app.is_busy,
                anim_tick,
                &app.theme,
            );

            // Render autocomplete popup above input bar if active
            let autocomplete_items = app.get_autocomplete_items();
            if !autocomplete_items.is_empty() {
                render_autocomplete_dropdown(
                    f,
                    chunks[1],
                    &autocomplete_items,
                    app.autocomplete_index,
                    &app.theme,
                );
            }

            let update_str = app.available_update.as_ref().map(|u| u.latest_version.as_str());
            let elapsed_secs = app.busy_started_at.map(|t| t.elapsed().as_secs_f32());
            render_status_bar(
                f,
                chunks[2],
                &app.model,
                app.thinking,
                app.is_busy,
                elapsed_secs,
                anim_tick,
                update_str,
                &app.theme,
            );

            // Render active modal on top
            match &app.modal {
                ModalState::None => {}
                ModalState::ModelSelector {
                    filtered,
                    selected_idx,
                    filter,
                    provider_tabs,
                    active_tab_idx,
                    ..
                } => {
                    render_model_selector_modal(
                        f,
                        f.area(),
                        filtered,
                        *selected_idx,
                        filter,
                        provider_tabs,
                        *active_tab_idx,
                        app.thinking,
                        &app.model,
                        &app.theme,
                    );
                }
                ModalState::LoginSelector { selected_idx } => {
                    let cred_store = dume_provider::CredentialStore::new(
                        dume_provider::CredentialStore::default_path(),
                    );

                    let has_anthropic = cred_store.has_credential("anthropic");
                    let has_openai = cred_store.has_credential("openai");
                    let has_google = cred_store.has_credential("google");
                    let has_deepseek = cred_store.has_credential("deepseek");
                    let has_codex = cred_store.has_credential("openai-codex");
                    let has_opencode = cred_store.has_credential("opencode");
                    let has_opencode_go = cred_store.has_credential("opencode-go");

                    let choices = [
                        LoginProviderChoice {
                            id: "anthropic",
                            name: "Anthropic Claude",
                            auth_type: "API Key",
                            is_authenticated: has_anthropic,
                        },
                        LoginProviderChoice {
                            id: "openai",
                            name: "OpenAI ChatGPT",
                            auth_type: "API Key",
                            is_authenticated: has_openai,
                        },
                        LoginProviderChoice {
                            id: "google",
                            name: "Google Gemini",
                            auth_type: "API Key",
                            is_authenticated: has_google,
                        },
                        LoginProviderChoice {
                            id: "deepseek",
                            name: "DeepSeek",
                            auth_type: "API Key",
                            is_authenticated: has_deepseek,
                        },
                        LoginProviderChoice {
                            id: "openai-codex",
                            name: "OpenAI Codex",
                            auth_type: "OAuth Browser",
                            is_authenticated: has_codex,
                        },
                        LoginProviderChoice {
                            id: "opencode",
                            name: "OpenCode Zen",
                            auth_type: "API Key",
                            is_authenticated: has_opencode,
                        },
                        LoginProviderChoice {
                            id: "opencode-go",
                            name: "OpenCode Go",
                            auth_type: "API Key",
                            is_authenticated: has_opencode_go,
                        },
                    ];

                    render_login_selector_modal(
                        f,
                        f.area(),
                        &choices,
                        *selected_idx,
                        &app.theme,
                    );
                }
                ModalState::ApiKeyInput {
                    provider_name,
                    input,
                    ..
                } => {
                    render_api_key_modal(
                        f,
                        f.area(),
                        provider_name,
                        input,
                        &app.theme,
                    );
                }
                ModalState::CodexOAuthWaiting { url, .. } => {
                    render_oauth_waiting_modal(
                        f,
                        f.area(),
                        "OpenAI Codex",
                        url,
                        &app.theme,
                    );
                }
                ModalState::Update {
                    info,
                    status_text,
                    in_progress,
                } => {
                    render_update_modal(
                        f,
                        f.area(),
                        info,
                        status_text,
                        *in_progress,
                        &app.theme,
                    );
                }
                ModalState::BtwChat {
                    history,
                    input,
                    is_streaming,
                    streaming_reply,
                } => {
                    render_btw_modal(
                        f,
                        f.area(),
                        history,
                        input,
                        *is_streaming,
                        streaming_reply,
                        &app.theme,
                    );
                }
            }
        })?;

        tokio::select! {
            maybe_event = reader.next() => {
                if let Some(Ok(event)) = maybe_event {
                    match event {
                        Event::Key(key) => {
                            // 1. Handle Modal Events if modal is active
                            match &mut app.modal {
                                ModalState::ModelSelector {
                                    models,
                                    filtered,
                                    selected_idx,
                                    filter,
                                    provider_tabs,
                                    active_tab_idx,
                                } => {
                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Tab => {
                                            app.thinking = app.thinking.next();
                                        }
                                        crossterm::event::KeyCode::Left => {
                                            if !provider_tabs.is_empty() {
                                                if *active_tab_idx == 0 {
                                                    *active_tab_idx = provider_tabs.len() - 1;
                                                } else {
                                                    *active_tab_idx -= 1;
                                                }
                                                let tab = &provider_tabs[*active_tab_idx];
                                                let q = filter.to_lowercase();
                                                *filtered = models
                                                    .iter()
                                                    .filter(|m| {
                                                        (tab == "All" || m.provider.eq_ignore_ascii_case(tab))
                                                            && (m.id.to_lowercase().contains(&q)
                                                                || m.provider.to_lowercase().contains(&q))
                                                    })
                                                    .cloned()
                                                    .collect();
                                                *selected_idx = 0;
                                            }
                                        }
                                        crossterm::event::KeyCode::Right => {
                                            if !provider_tabs.is_empty() {
                                                if *active_tab_idx + 1 >= provider_tabs.len() {
                                                    *active_tab_idx = 0;
                                                } else {
                                                    *active_tab_idx += 1;
                                                }
                                                let tab = &provider_tabs[*active_tab_idx];
                                                let q = filter.to_lowercase();
                                                *filtered = models
                                                    .iter()
                                                    .filter(|m| {
                                                        (tab == "All" || m.provider.eq_ignore_ascii_case(tab))
                                                            && (m.id.to_lowercase().contains(&q)
                                                                || m.provider.to_lowercase().contains(&q))
                                                    })
                                                    .cloned()
                                                    .collect();
                                                *selected_idx = 0;
                                            }
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            if !filtered.is_empty() {
                                                if *selected_idx == 0 {
                                                    *selected_idx = filtered.len() - 1;
                                                } else {
                                                    *selected_idx -= 1;
                                                }
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            if !filtered.is_empty() {
                                                if *selected_idx + 1 >= filtered.len() {
                                                    *selected_idx = 0;
                                                } else {
                                                    *selected_idx += 1;
                                                }
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            if let Some(selected) = filtered.get(*selected_idx) {
                                                let new_model = format!("{}/{}", selected.provider, selected.id);
                                                persist_last_used_model(&new_model);
                                                app.messages.push(ChatMessage::ui_notice(format!("Switched model to '{}'", new_model)));
                                                app.model = new_model;
                                                app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                            }
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Backspace => {
                                            filter.pop();
                                            let q = filter.to_lowercase();
                                            let tab = &provider_tabs[*active_tab_idx];
                                            *filtered = models
                                                .iter()
                                                .filter(|m| {
                                                    (tab == "All" || m.provider.eq_ignore_ascii_case(tab))
                                                        && (m.id.to_lowercase().contains(&q)
                                                            || m.provider.to_lowercase().contains(&q))
                                                })
                                                .cloned()
                                                .collect();
                                            *selected_idx = 0;
                                        }
                                        crossterm::event::KeyCode::Char(c) => {
                                            filter.push(c);
                                            let q = filter.to_lowercase();
                                            let tab = &provider_tabs[*active_tab_idx];
                                            *filtered = models
                                                .iter()
                                                .filter(|m| {
                                                    (tab == "All" || m.provider.eq_ignore_ascii_case(tab))
                                                        && (m.id.to_lowercase().contains(&q)
                                                            || m.provider.to_lowercase().contains(&q))
                                                })
                                                .cloned()
                                                .collect();
                                            *selected_idx = 0;
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }
                                ModalState::LoginSelector { selected_idx } => {
                                    let provider_ids = [
                                        "anthropic",
                                        "openai",
                                        "google",
                                        "deepseek",
                                        "openai-codex",
                                        "opencode",
                                        "opencode-go",
                                    ];
                                    let provider_names = [
                                        "Anthropic Claude",
                                        "OpenAI ChatGPT",
                                        "Google Gemini",
                                        "DeepSeek",
                                        "OpenAI Codex",
                                        "OpenCode Zen",
                                        "OpenCode Go",
                                    ];

                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            if *selected_idx == 0 {
                                                *selected_idx = provider_ids.len() - 1;
                                            } else {
                                                *selected_idx -= 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            if *selected_idx + 1 >= provider_ids.len() {
                                                *selected_idx = 0;
                                            } else {
                                                *selected_idx += 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            let p_id = provider_ids[*selected_idx];
                                            let p_name = provider_names[*selected_idx];

                                            if p_id == "openai-codex" {
                                                use dume_provider::oauth;
                                                match oauth::get_oauth_config("openai-codex") {
                                                    Some(config) => {
                                                        match oauth::generate_pkce() {
                                                            Ok(pkce) => {
                                                                let state = oauth::generate_state().unwrap_or_else(|_| "dum-e-login".to_string());
                                                                match oauth::build_authorization_url(&config, &state, &pkce.challenge) {
                                                                    Ok(auth_url) => {
                                                                        // Open browser automatically on mac
                                                                        let _ = std::process::Command::new("open").arg(&auth_url).spawn();

                                                                        let cancel_token = CancellationToken::new();
                                                                        let tx = stream_tx.clone();
                                                                        let cancel_child = cancel_token.clone();

                                                                        tokio::spawn(async move {
                                                                            let flow = async {
                                                                                let listener = oauth::bind_oauth_callback(config.port).await?;
                                                                                let code = oauth::wait_for_oauth_callback(listener, config.callback_path, &state).await?;
                                                                                let token = oauth::exchange_code_for_token(
                                                                                    &config,
                                                                                    &code,
                                                                                    &config.redirect_uri(),
                                                                                    &pkce.verifier,
                                                                                    &state,
                                                                                ).await?;
                                                                                let cred = dume_provider::Credential::from_oauth("openai-codex", token, None)?;
                                                                                let store = dume_provider::CredentialStore::new(
                                                                                    dume_provider::CredentialStore::default_path(),
                                                                                );
                                                                                store.save("openai-codex", &cred)?;
                                                                                Ok::<_, anyhow::Error>(())
                                                                            };

                                                                            tokio::select! {
                                                                                res = flow => {
                                                                                    let _ = tx.send(ConversationEvent::OAuthComplete {
                                                                                        provider: "OpenAI Codex".to_string(),
                                                                                        result: res,
                                                                                    }).await;
                                                                                }
                                                                                _ = cancel_child.cancelled() => {}
                                                                            }
                                                                        });

                                                                        app.modal = ModalState::CodexOAuthWaiting {
                                                                            url: auth_url,
                                                                            cancel_token,
                                                                        };
                                                                    }
                                                                    Err(e) => {
                                                                        app.messages.push(ChatMessage::ui_notice(format!("Failed to build OAuth URL: {}", e)));
                                                                        app.modal = ModalState::None;
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                app.messages.push(ChatMessage::ui_notice(format!("PKCE generation failed: {}", e)));
                                                                app.modal = ModalState::None;
                                                            }
                                                        }
                                                    }
                                                    None => {
                                                        app.messages.push(ChatMessage::ui_notice("OAuth config for openai-codex not found."));
                                                        app.modal = ModalState::None;
                                                    }
                                                }
                                            } else {
                                                app.modal = ModalState::ApiKeyInput {
                                                    provider_id: p_id.to_string(),
                                                    provider_name: p_name.to_string(),
                                                    input: String::new(),
                                                };
                                            }
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }
                                ModalState::ApiKeyInput { provider_id, provider_name, input } => {
                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Backspace => {
                                            input.pop();
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            let key = input.trim();
                                            if !key.is_empty() {
                                                let cred_store = dume_provider::CredentialStore::new(
                                                    dume_provider::CredentialStore::default_path(),
                                                );
                                                match save_credential_and_rotate_session(
                                                    &cred_store,
                                                    provider_id,
                                                    key,
                                                    &mut app.session_id,
                                                ) {
                                                    Ok(()) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!(
                                                            "Successfully authenticated {} with API key.",
                                                            provider_name
                                                        )));
                                                    }
                                                    Err(e) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!(
                                                            "Failed to save credential for {}: {}",
                                                            provider_name, e
                                                        )));
                                                    }
                                                }
                                            }
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Char(c) => {
                                            input.push(c);
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }
                                ModalState::CodexOAuthWaiting { cancel_token, .. } => {
                                    if key.code == crossterm::event::KeyCode::Esc {
                                        cancel_token.cancel();
                                        app.modal = ModalState::None;
                                        app.messages.push(ChatMessage::ui_notice("Cancelled OpenAI Codex login."));
                                    }
                                    continue;
                                }
                                ModalState::Update { info, in_progress, status_text } => {
                                    if *in_progress {
                                        // Ignore inputs while update is underway
                                        continue;
                                    }
                                    if key.code == crossterm::event::KeyCode::Esc {
                                        app.modal = ModalState::None;
                                    } else if key.code == crossterm::event::KeyCode::Enter {
                                        *in_progress = true;
                                        *status_text = "Starting update...".to_string();
                                        let update_tx = stream_tx.clone();
                                        let update_info = info.clone();

                                        tokio::spawn(async move {
                                            let info_clone = update_info.clone();
                                            let tx_progress = update_tx.clone();
                                            let res = tokio::task::spawn_blocking(move || {
                                                dume_core::updater::apply_update(&info_clone, |msg| {
                                                    let _ = tx_progress.blocking_send(ConversationEvent::UpdateProgress {
                                                        message: msg.to_string(),
                                                    });
                                                })
                                            }).await;

                                            let final_res = match res {
                                                Ok(Ok(path)) => Ok(path),
                                                Ok(Err(e)) => Err(e.to_string()),
                                                Err(join_err) => Err(join_err.to_string()),
                                            };

                                            let _ = update_tx.send(ConversationEvent::UpdateFinished { result: final_res }).await;
                                        });
                                    }
                                    continue;
                                }
                                ModalState::BtwChat { history, input, is_streaming, .. } => {
                                    if key.code == crossterm::event::KeyCode::Esc {
                                        app.modal = ModalState::None;
                                    } else if key.code == crossterm::event::KeyCode::Backspace {
                                        input.pop();
                                    } else if key.code == crossterm::event::KeyCode::Enter {
                                        if !input.trim().is_empty() && !*is_streaming {
                                            let q = std::mem::take(input);
                                            history.push(("user".to_string(), q.clone()));
                                            *is_streaming = true;

                                            let model_name = app.model.clone();
                                            let btw_history = history.clone();
                                            let tx = stream_tx.clone();

                                            tokio::spawn(async move {
                                                let cred_store = dume_provider::CredentialStore::new(
                                                    dume_provider::CredentialStore::default_path(),
                                                );
                                                let provider_res = dume_provider::runtime::resolve_provider(&model_name, &cred_store).await;
                                                match provider_res {
                                                    Ok(provider) => {
                                                        let mut msgs = vec![
                                                            ChatMessage::system("You are a helpful coding assistant answering a quick side-question without tools. Keep your answer direct and concise.")
                                                        ];
                                                        for (r, content) in btw_history.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
                                                            if r == "user" {
                                                                msgs.push(ChatMessage::user(content));
                                                            } else {
                                                                msgs.push(ChatMessage::assistant(content));
                                                            }
                                                        }
                                                        let (sub_tx, mut sub_rx) = tokio::sync::mpsc::channel(50);
                                                        let stream_tx = tx.clone();
                                                        let btw_ctx = dume_provider::types::StreamRequestContext::new();
                                                        tokio::spawn(async move {
                                                            let _ = provider.stream_with_context(&msgs, &[], Some(&btw_ctx), sub_tx).await;
                                                        });
                                                        let mut full_text = String::new();
                                                        while let Some(evt) = sub_rx.recv().await {
                                                            match evt {
                                                                StreamEvent::TextDelta(d) => {
                                                                    full_text.push_str(&d);
                                                                    let _ = stream_tx.send(ConversationEvent::BtwStreamDelta(d)).await;
                                                                }
                                                                StreamEvent::Completed { .. } => break,
                                                                StreamEvent::Error(err) => {
                                                                    let _ = stream_tx.send(ConversationEvent::BtwFinished(Err(err))).await;
                                                                    return;
                                                                }
                                                                _ => {}
                                                            }
                                                        }
                                                        let _ = stream_tx.send(ConversationEvent::BtwFinished(Ok(full_text))).await;
                                                    }
                                                    Err(e) => {
                                                        let _ = tx.send(ConversationEvent::BtwFinished(Err(e.to_string()))).await;
                                                    }
                                                }
                                            });
                                        }
                                    } else if let crossterm::event::KeyCode::Char(c) = key.code {
                                        input.push(c);
                                    }
                                    continue;
                                }
                                ModalState::None => {}
                            }

                            // 2. Normal Editor and Transcript Handling
                            let autocomplete_items = app.get_autocomplete_items();
                            let is_autocomplete_active = !autocomplete_items.is_empty();

                            match handle_key_event(key) {
                                Action::Quit => {
                                    // 1. If currently busy streaming, first Ctrl+C cancels the active turn
                                    if app.is_busy {
                                        if let Some(cancel) = current_stream_cancel.take() {
                                            cancel.cancel();
                                        }
                                        app.messages.push(ChatMessage::ui_notice("Streaming interrupted by user."));
                                        app.is_busy = false;
                                        app.streaming_text.clear();
                                        app.last_ctrl_c = None;
                                        continue;
                                    }

                                    // 2. If input buffer has content, first Ctrl+C clears the buffer to prevent accidental exit
                                    if !app.input_buffer.is_empty() {
                                        app.input_buffer.clear();
                                        app.cursor_pos = 0;
                                        app.last_ctrl_c = None;
                                        continue;
                                    }

                                    // 3. Double Ctrl+C protection: must press Ctrl+C twice within 2 seconds to quit
                                    let now = std::time::Instant::now();
                                    if let Some(last_time) = app.last_ctrl_c {
                                        if now.duration_since(last_time) < std::time::Duration::from_secs(2) {
                                            break;
                                        }
                                    }
                                    app.last_ctrl_c = Some(now);
                                    continue;
                                }
                                Action::Escape => {
                                    if is_autocomplete_active {
                                        app.autocomplete_dismissed = true;
                                    } else if !app.input_buffer.is_empty() {
                                        app.input_buffer.clear();
                                        app.cursor_pos = 0;
                                    } else if app.is_busy {
                                        if let Some(cancel) = current_stream_cancel.take() {
                                            cancel.cancel();
                                        }
                                        app.messages.push(ChatMessage::ui_notice("Streaming interrupted by user."));
                                        app.is_busy = false;
                                        app.streaming_text.clear();
                                    }
                                    app.last_ctrl_c = None;
                                }
                                Action::Clear if !app.is_busy => {
                                    app.messages.clear();
                                    app.streaming_text.clear();
                                    app.auto_scroll = true;
                                    app.scroll_offset = 0;
                                    app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                }
                                Action::Clear => {}
                                Action::InsertChar(c) => {
                                    app.autocomplete_dismissed = false;
                                    app.insert_char(c);
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteChar => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_char();
                                    app.autocomplete_index = 0;
                                }
                                Action::CursorLeft => app.cursor_left(),
                                Action::CursorRight => app.cursor_right(),
                                Action::CursorHome => app.cursor_pos = 0,
                                Action::CursorEnd => app.cursor_pos = app.input_buffer.chars().count(),
                                Action::ScrollUp => {
                                    if is_autocomplete_active {
                                        if app.autocomplete_index == 0 {
                                            app.autocomplete_index = autocomplete_items.len().saturating_sub(1);
                                        } else {
                                            app.autocomplete_index -= 1;
                                        }
                                    } else if !app.prompt_history.is_empty() {
                                        app.navigate_history_up();
                                    } else {
                                        app.auto_scroll = false;
                                        app.scroll_offset = app.scroll_offset.saturating_sub(1);
                                    }
                                }
                                Action::ScrollDown => {
                                    if is_autocomplete_active {
                                        if app.autocomplete_index + 1 >= autocomplete_items.len() {
                                            app.autocomplete_index = 0;
                                        } else {
                                            app.autocomplete_index += 1;
                                        }
                                    } else if app.history_index.is_some() {
                                        app.navigate_history_down();
                                    } else {
                                        app.scroll_offset = app.scroll_offset.saturating_add(1);
                                    }
                                }
                                Action::Tab => {
                                    if is_autocomplete_active && !autocomplete_items.is_empty() {
                                        let selected = &autocomplete_items[app.autocomplete_index];
                                        let mut completion = selected.name.clone();
                                        completion.push(' ');
                                        app.input_buffer = completion;
                                        app.cursor_pos = app.input_buffer.chars().count();
                                        app.autocomplete_dismissed = true;
                                    } else {
                                        app.thinking = app.thinking.next();
                                    }
                                }
                                Action::PageUp => {
                                    app.auto_scroll = false;
                                    app.scroll_offset = app.scroll_offset.saturating_sub(10);
                                }
                                Action::PageDown => {
                                    app.scroll_offset = app.scroll_offset.saturating_add(10);
                                }
                                Action::SubmitInput => {
                                    if is_autocomplete_active && !autocomplete_items.is_empty() {
                                        let selected = &autocomplete_items[app.autocomplete_index];
                                        app.input_buffer = selected.name.clone();
                                    }

                                    if !app.input_buffer.trim().is_empty() && !app.is_busy {
                                        let content = std::mem::take(&mut app.input_buffer);
                                        app.cursor_pos = 0;
                                        app.auto_scroll = true;
                                        app.autocomplete_dismissed = false;
                                        app.autocomplete_index = 0;

                                        let trimmed = content.trim();
                                        if trimmed == "/model" || trimmed == "/model " {
                                            let models = selectable_models();

                                            if models.is_empty() {
                                                app.messages.push(ChatMessage::ui_notice(
                                                    "No authenticated providers found. Please run /login to authenticate a provider (e.g. Anthropic, OpenAI, Google, DeepSeek, OpenAI Codex, OpenCode Zen, OpenCode Go)."
                                                ));
                                                continue;
                                            }

                                            // Build provider tabs
                                            let mut provider_tabs = vec!["All".to_string()];
                                            for m in &models {
                                                if !provider_tabs.iter().any(|t| t.eq_ignore_ascii_case(&m.provider)) {
                                                    provider_tabs.push(m.provider.clone());
                                                }
                                            }

                                            let current = app.model.clone();
                                            let initial_idx = models.iter().position(|m| m.id == current || format!("{}/{}", m.provider, m.id) == current).unwrap_or(0);
                                            app.modal = ModalState::ModelSelector {
                                                filtered: models.clone(),
                                                models,
                                                selected_idx: initial_idx,
                                                filter: String::new(),
                                                provider_tabs,
                                                active_tab_idx: 0,
                                            };
                                            continue;
                                        } else if trimmed.starts_with("/model ") {
                                            let new_model = trimmed[7..].trim().to_string();
                                            persist_last_used_model(&new_model);
                                            app.messages.push(ChatMessage::ui_notice(format!("Switched model to '{}'", new_model)));
                                            app.model = new_model;
                                            app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                            continue;
                                        } else if trimmed == "/login" {
                                            // Open interactive login provider selector modal
                                            app.modal = ModalState::LoginSelector { selected_idx: 0 };
                                            continue;
                                        } else if trimmed.starts_with("/login ") {
                                            let rest = trimmed[7..].trim();
                                            let parts: Vec<&str> = rest.split_whitespace().collect();
                                            if parts.len() < 2 {
                                                app.messages.push(ChatMessage::ui_notice(
                                                    "Usage: /login <provider> <api-key>\nExample: /login anthropic sk-ant-..."
                                                ));
                                            } else {
                                                let provider = parts[0];
                                                let key = parts[1..].join(" ");
                                                let cred_store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
                                                match save_credential_and_rotate_session(
                                                    &cred_store,
                                                    provider,
                                                    &key,
                                                    &mut app.session_id,
                                                ) {
                                                    Ok(()) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!(
                                                            "Successfully saved API key for provider '{}'",
                                                            provider
                                                        )));
                                                    }
                                                    Err(e) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!(
                                                            "Failed to save credential for '{}': {}",
                                                            provider, e
                                                        )));
                                                    }
                                                }
                                            }
                                            continue;
                                        } else if trimmed == "/logout" || trimmed.starts_with("/logout ") {
                                            let provider = if trimmed == "/logout" {
                                                "anthropic"
                                            } else {
                                                trimmed[8..].trim()
                                            };
                                            let cred_store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
                                            match delete_credential_and_rotate_session(
                                                &cred_store,
                                                provider,
                                                &mut app.session_id,
                                            ) {
                                                Ok(()) => {
                                                    app.messages.push(ChatMessage::ui_notice(format!(
                                                        "Logged out from provider '{}'",
                                                        provider
                                                    )));
                                                }
                                                Err(e) => {
                                                    app.messages.push(ChatMessage::ui_notice(format!(
                                                        "Failed to logout from '{}': {}",
                                                        provider, e
                                                    )));
                                                }
                                            }
                                            continue;
                                        } else if trimmed == "/usage" {
                                            let (input, output, total, cache_read, cache_write) = app.session_usage;
                                            app.messages.push(ChatMessage::ui_notice(format!(
                                                "Session Token Usage:\n  Input Tokens:       {}\n  Output Tokens:      {}\n  Total Tokens:       {}\n  Cache Read Tokens:  {}\n  Cache Write Tokens: {}",
                                                input, output, total, cache_read, cache_write
                                            )));
                                            continue;
                                        } else if trimmed == "/btw" || trimmed.starts_with("/btw ") {
                                            let initial_query = if trimmed.starts_with("/btw ") {
                                                trimmed[5..].trim().to_string()
                                            } else {
                                                String::new()
                                            };
                                            app.modal = ModalState::BtwChat {
                                                history: Vec::new(),
                                                input: initial_query,
                                                is_streaming: false,
                                                streaming_reply: String::new(),
                                            };
                                            continue;
                                        } else if trimmed == "/clear" {
                                            app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                            app.messages.clear();
                                            app.streaming_text.clear();
                                            app.auto_scroll = true;
                                            app.scroll_offset = 0;
                                            continue;
                                        } else if trimmed == "/skills" {
                                            app.skills.reload();
                                            let skills = app.skills.list();
                                            let mut text = String::from("Available Skills:\n");
                                            if skills.is_empty() {
                                                text.push_str("  (No skills found in .dume/skills or ~/.dume/skills)\n");
                                            } else {
                                                for skill in skills {
                                                    text.push_str(&format!("  /{:<18} - {}\n", skill.name, skill.description));
                                                }
                                            }
                                            app.messages.push(ChatMessage::ui_notice(text.trim_end().to_string()));
                                            continue;
                                        } else if trimmed == "/update" {
                                            if let Some(info) = app.available_update.clone() {
                                                app.modal = ModalState::Update {
                                                    info,
                                                    status_text: "Ready to install. Press Enter to proceed.".to_string(),
                                                    in_progress: false,
                                                };
                                            } else {
                                                app.messages.push(ChatMessage::ui_notice("Checking for updates..."));
                                                let update_tx = stream_tx.clone();
                                                tokio::spawn(async move {
                                                    let current_version = env!("CARGO_PKG_VERSION");
                                                    let repo = "jeongminsang/dum-e";
                                                    match dume_core::updater::check_for_update(repo, current_version) {
                                                        Ok(Some(info)) => {
                                                            let _ = update_tx.send(ConversationEvent::UpdateAvailable(info)).await;
                                                        }
                                                        Ok(None) => {
                                                            let _ = update_tx.send(ConversationEvent::Finished {
                                                                messages: vec![ChatMessage::ui_notice(format!(
                                                                    "DUM-E is already up to date (v{}).",
                                                                    current_version
                                                                ))],
                                                                error: None,
                                                                session_id: None,
                                                            }).await;
                                                        }
                                                        Err(e) => {
                                                            let _ = update_tx.send(ConversationEvent::Finished {
                                                                messages: vec![ChatMessage::ui_notice(format!(
                                                                    "Update check failed: {}",
                                                                    e
                                                                ))],
                                                                error: None,
                                                                session_id: None,
                                                            }).await;
                                                        }
                                                    }
                                                });
                                            }
                                            continue;
                                        } else if trimmed == "/help" {
                                            app.skills.reload();
                                            let mut help = String::from(
                                                "DUM-E Commands:\n  /model <provider/model>    - Switch model (e.g. anthropic/claude-sonnet-4-5, openai/gpt-4o)\n  /login <provider> <key>    - Save API key directly in TUI\n  /logout <provider>         - Clear saved credentials\n  /update                    - Check and install latest version in-place\n  /clear                     - Clear conversation transcript\n  /skills                    - List available skills\n  /help                      - Show this help\n\nShortcuts:\n  Ctrl+C / Ctrl+D - Exit\n  Ctrl+L          - Clear screen\n  PageUp/Down     - Scroll transcript\n  Mouse Wheel     - Scroll up/down\n"
                                            );
                                            let skills = app.skills.list();
                                            if !skills.is_empty() {
                                                help.push_str("\nAvailable Skill Commands:\n");
                                                for skill in skills {
                                                    help.push_str(&format!("  /{:<22} - {}\n", skill.name, skill.description));
                                                }
                                            }
                                            app.messages.push(ChatMessage::ui_notice(help.trim_end().to_string()));
                                            continue;
                                        } else if let Some(cmd) = trimmed.strip_prefix('/') {
                                            app.skills.reload();
                                            let (skill_name, rest_args) = match cmd.split_once(char::is_whitespace) {
                                                Some((name, rest)) => (name, rest.trim()),
                                                None => (cmd, ""),
                                            };

                                            if let Some(skill) = app.skills.get_or_reload(skill_name).cloned() {
                                                let is_already_in_context = dume_core::skills::SkillRegistry::is_content_in_messages(
                                                    &skill.content,
                                                    app.messages.iter().map(|m| m.content.as_str()),
                                                );
                                                let prompt_content = if is_already_in_context {
                                                    if rest_args.is_empty() {
                                                        format!("Execute skill '{}' (already loaded in context)", skill.name)
                                                    } else {
                                                        format!("Execute skill '{}' with instructions: {}", skill.name, rest_args)
                                                    }
                                                } else if rest_args.is_empty() {
                                                    format!("Execute skill '{}':

{}", skill.name, skill.content)
                                                } else {
                                                    format!("Execute skill '{}' with instructions: {}

{}", skill.name, rest_args, skill.content)
                                                };
                                                app.prompt_history.push(format!("/{}", cmd));
                                                app.history_index = None;
                                                app.temp_input.clear();
                                                app.messages.push(ChatMessage::user(format!("/{}", cmd)));
                                                app.messages.push(ChatMessage::ui_notice(format!("Loaded skill '{}': {}", skill.name, skill.description)));
                                                app.is_busy = true;
                                                app.busy_started_at = Some(std::time::Instant::now());

                                                let model_name = app.model.clone();
                                                let session_id = app.session_id.clone();
                                                let mut msgs = app.messages.clone();
                                                // Replace or append prompt for AI execution
                                                msgs.push(ChatMessage::user(prompt_content));

                                                let tx = stream_tx.clone();
                                                let dispatcher = dispatcher.clone();
                                                let child_cancel = cancellation.child_token();
                                                current_stream_cancel = Some(child_cancel.clone());

                                                task = Some(tokio::spawn(async move {
                                                    dispatch_stream(&model_name, session_id, msgs, tx, dispatcher, child_cancel).await;
                                                }));
                                                continue;
                                            } else {
                                                app.messages.push(ChatMessage::ui_notice(format!(
                                                    "Unknown command '/{}'. Type /help or /skills to see available commands.",
                                                    skill_name
                                                )));
                                                continue;
                                            }
                                        }

                                        app.prompt_history.push(content.clone());
                                        app.history_index = None;
                                        app.temp_input.clear();
                                        app.messages.push(ChatMessage::user(content.clone()));
                                        app.is_busy = true;
                                        app.busy_started_at = Some(std::time::Instant::now());

                                        // Spawn streaming task
                                        let model_name = app.model.clone();
                                        let session_id = app.session_id.clone();
                                        let msgs = app.messages.clone();
                                        let tx = stream_tx.clone();
                                        let dispatcher = dispatcher.clone();
                                        let child_cancel = cancellation.child_token();
                                        current_stream_cancel = Some(child_cancel.clone());

                                        task = Some(tokio::spawn(async move {
                                            dispatch_stream(&model_name, session_id, msgs, tx, dispatcher, child_cancel).await;
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
                                    app.auto_scroll = false;
                                    app.scroll_offset = app.scroll_offset.saturating_sub(3);
                                }
                                MouseEventKind::ScrollDown => {
                                    app.scroll_offset = app.scroll_offset.saturating_add(3);
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(stream_event) = stream_rx.recv() => {
                if let ConversationEvent::OAuthComplete { provider, result: Ok(()) } = &stream_event {
                    if provider == "openai-codex" {
                        spawn_codex_catalog_refresh(stream_tx.clone());
                    }
                }
                app.receive(stream_event);
            }
            _ = ticker.tick() => {
                if app.is_busy {
                    anim_tick = anim_tick.wrapping_add(1);
                }
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
    mut session_id: String,
    mut messages: Vec<ChatMessage>,
    tx: mpsc::Sender<ConversationEvent>,
    dispatcher: Arc<Mutex<ToolDispatcher>>,
    cancellation: CancellationToken,
) {
    let mut dispatcher = dispatcher.lock().await;
    dispatcher.set_model(model);
    let result = dispatch_authenticated(
        model,
        &mut session_id,
        &mut messages,
        &tx,
        &mut dispatcher,
        &cancellation,
    )
    .await;
    dispatcher.cancel_all().await;
    let _ = tx
        .send(ConversationEvent::Finished {
            messages,
            error: result.as_ref().err().map(|error| format!("{error:#}")),
            session_id: Some(session_id),
        })
        .await;
}

struct PendingCompaction {
    before: Vec<ChatMessage>,
    after: Vec<ChatMessage>,
}

struct DispatchState {
    session_id: String,
    pending: Option<PendingCompaction>,
}

fn model_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut payload = vec![ChatMessage::system(
        dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT,
    )];
    payload.extend(messages.iter().filter(|message| {
        message.is_model_visible()
            && !(message.role == dume_provider::Role::System
                && message.content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT)
    }).cloned());
    payload
}

async fn dispatch_authenticated(
    model: &str,
    session_id: &mut String,
    messages: &mut Vec<ChatMessage>,
    tx: &mpsc::Sender<ConversationEvent>,
    dispatcher: &mut ToolDispatcher,
    cancellation: &CancellationToken,
) -> Result<String> {
    if !messages.iter().any(|m| m.role == dume_provider::Role::System && m.content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT) {
        messages.insert(0, ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT));
    }
    let cred_store =
        dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    let model = model.to_string();
    let tools = dispatcher.tool_definitions_for_session(session_id);
    let store = dispatcher.store().cloned();
    let session_state = Arc::new(std::sync::Mutex::new(DispatchState {
        session_id: session_id.clone(),
        pending: None,
    }));

    let result = run_conversation(
        messages,
        tx,
        dispatcher,
        cancellation,
        10,
        |raw_messages, tx| {
            let cred_store = cred_store.clone();
            let model = model.clone();
            let tools = tools.clone();
            let store = store.clone();
            let session_state = Arc::clone(&session_state);
            async move {
                let mut raw_messages = raw_messages;
                let before = raw_messages.clone();
                let mut session_id = session_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Dispatch state lock poisoned"))?
                    .session_id
                    .clone();
                let compacted = dume_worker::context::prepare_request_context(
                    dume_worker::context::RequestContext {
                        model: &model,
                        session_id: &session_id,
                        messages: &mut raw_messages,
                        tools: &tools,
                        store: store.as_deref(),
                    },
                )?;
                if compacted {
                    session_id = dume_provider::types::StreamRequestContext::new().session_id;
                    let mut state = session_state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Dispatch state lock poisoned"))?;
                    state.session_id = session_id.clone();
                    state.pending = Some(PendingCompaction {
                        before,
                        after: raw_messages.clone(),
                    });
                }
                let turn_ctx = dume_provider::types::StreamRequestContext::for_session(&session_id);
                let provider_msgs = model_messages(&raw_messages);
                let provider = resolve_provider(&model, &cred_store).await?;
                provider
                    .stream_with_context(&provider_msgs, &tools, Some(&turn_ctx), tx)
                    .await?;
                Ok(raw_messages)
            }
        },
    )
    .await;
    let mut state = session_state
        .lock()
        .map_err(|_| anyhow::anyhow!("Dispatch state lock poisoned"))?;
    if result.is_err() {
        if let Some(pending) = state.pending.take() {
            if *messages == pending.before {
                *messages = pending.after;
            }
        }
    }
    *session_id = state.session_id.clone();
    result?;
    Ok(session_id.clone())
}

#[derive(Default)]
struct StreamTurn {
    text: String,
    reasoning: Vec<serde_json::Value>,
    deepseek_reasoning: String,
    gemini_parts: Vec<serde_json::Value>,
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
            StreamEvent::DeepSeekReasoning(delta) => self.deepseek_reasoning.push_str(delta),
            StreamEvent::GeminiParts(parts) => self.gemini_parts.extend(parts.iter().cloned()),
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
            StreamEvent::Usage(_) => {}
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
        if !self.deepseek_reasoning.is_empty() || !calls.is_empty() {
            message.deepseek_reasoning = Some(self.deepseek_reasoning);
        }
        message.gemini_parts = self.gemini_parts;
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
    Fut: std::future::Future<Output = Result<Vec<ChatMessage>>>,
{
    for _ in 0..max_turns {
        let (sub_tx, mut sub_rx) = mpsc::channel(100);
        let collect = async {
            let mut turn = StreamTurn::default();
            while let Some(event) = sub_rx.recv().await {
                turn.push(&event)?;
                if matches!(
                    event,
                    StreamEvent::TextDelta(_)
                        | StreamEvent::ToolCallDelta { .. }
                        | StreamEvent::Usage(_)
                ) {
                    tx.send(ConversationEvent::Stream(event)).await?;
                }
            }
            turn.finish()
        };
        // Both futures are scoped: errors drop the other side, never detach a producer.
        let (prepared_messages, (assistant, tool_calls)) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("Conversation cancelled"),
            result = async { tokio::try_join!(stream(messages.clone(), sub_tx), collect) } => result?,
        };
        *messages = prepared_messages;
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
            let tool_res = dispatcher.execute(call, &messages).await?;
            messages.push(tool_res);
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
    fn credential_changes_rotate_session_identity_only_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("credentials.json"));
        let mut session_id = "before-login".to_string();

        assert!(save_credential_and_rotate_session(
            &store,
            "openai",
            "test-key",
            &mut session_id,
        )
        .is_ok());
        assert_ne!(session_id, "before-login");
        assert!(store.has_credential("openai"));

        let logged_in_session = session_id.clone();
        assert!(save_credential_and_rotate_session(
            &store,
            "openai-codex",
            "invalid-api-key",
            &mut session_id,
        )
        .is_err());
        assert_eq!(session_id, logged_in_session);

        assert!(delete_credential_and_rotate_session(
            &store,
            "openai",
            &mut session_id,
        )
        .is_ok());
        assert_ne!(session_id, logged_in_session);
        assert!(!store.has_credential("openai"));
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
                requests.push(history.clone());
                let (events, fail) = scripts.pop_front().expect("unexpected model request");
                async move {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    anyhow::ensure!(!fail, "provider request failed");
                    Ok(history)
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
            session_id: None,
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
            session_id: None,
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
                    std::future::pending::<Result<Vec<ChatMessage>>>().await
                }
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(messages.len(), 1);
        assert!(!dir.path().join("bad.txt").exists());
    }

    #[test]
    fn ui_notices_do_not_change_model_history() {
        let history = vec![
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
            ChatMessage::user("First request"),
            ChatMessage::ui_notice("Available Skills: /test"),
            ChatMessage::ui_notice("Session Token Usage: 100"),
            ChatMessage::user("Second request"),
        ];
        let payload = model_messages(&history);
        assert_eq!(payload.len(), 3);
        assert_eq!(payload[0].content, dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT);
        assert_eq!(payload[1].content, "First request");
        assert_eq!(payload[2].content, "Second request");
        assert_eq!(model_messages(&history[..2]), payload[..2]);
    }

    #[tokio::test]
    async fn provider_failure_after_compaction_keeps_new_session_and_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(dir.path().join("artifacts")).unwrap());
        let dispatcher = Arc::new(Mutex::new(
            ToolDispatcher::new(dir.path(), "unknown-model", CancellationToken::new())
                .with_store(store.clone()),
        ));
        let original_session = "before-compaction".to_string();
        let history = vec![
            ChatMessage::user("Preserve the public API."),
            ChatMessage::assistant("x".repeat(500_000)),
            ChatMessage::user("Continue."),
        ];
        let (tx, mut rx) = mpsc::channel(8);
        dispatch_stream(
            "unknown-model",
            original_session.clone(),
            history,
            tx,
            dispatcher,
            CancellationToken::new(),
        )
        .await;
        let Some(ConversationEvent::Finished {
            mut messages,
            error: Some(error),
            session_id: Some(session_id),
        }) = rx.recv().await else {
            panic!("expected a failed conversation with a recoverable session");
        };
        assert!(error.contains("Unknown") || error.contains("unknown"), "{error}");
        assert_ne!(session_id, original_session);
        assert!(messages.iter().any(|message| {
            message.content.contains("Conversation checkpoint")
                && message.content.contains("Preserve the public API")
        }));
        assert!(messages.iter().all(|message| !message.content.contains(&"x".repeat(100))));

        messages.push(ChatMessage::assistant("y".repeat(500_000)));
        messages.push(ChatMessage::user("Try again."));
        assert!(dume_worker::context::prepare_request_context(
            dume_worker::context::RequestContext {
                model: "unknown-model",
                session_id: &session_id,
                messages: &mut messages,
                tools: &[],
                store: Some(&store),
            },
        )
        .is_ok());
    }
}
