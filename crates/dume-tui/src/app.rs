use crate::component::{
    AutocompleteItem, LoginProviderChoice, ThinkingLevel, build_transcript_lines,
    calculate_transcript_height, render_api_key_modal, render_autocomplete_dropdown,
    render_btw_modal, render_input_bar, render_login_selector_modal, render_logout_selector_modal,
    render_model_selector_modal, render_oauth_waiting_modal, render_resume_picker_modal,
    render_routing_probe_confirm_modal, render_routing_settings_modal, render_status_bar,
    render_transcript, render_update_modal,
};
use crate::keybinding::{Action, KeyBindings};
use crate::program_status::{ProgramStatus, ProgramStatusReporter};
use crate::selection::{TextSelection, copy_to_clipboard};
use crate::terminal::{ScreenRecovery, TerminalSession, read_events};
use crate::theme::Theme;
use anyhow::{Context, Result};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use dume_core::skills::SkillRegistry;
use dume_core::updater::UpdateInfo;
use dume_provider::ModelInfo;
use dume_provider::runtime::resolve_provider;
use dume_provider::types::{ChatMessage, StreamEvent, ToolCall};
use dume_store::{BoundaryResult, InputKind, RuntimeScope};
use dume_store::{HarnessStore, RecentSessionSummary};
use dume_worker::agent_loop::ToolDispatcher;
use dume_worker::runtime::{RunSession, ToolStart};
use dume_worker::subagent_settings::SubagentSettings;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

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

fn select_login_provider(
    app: &mut App,
    selected_idx: usize,
    stream_tx: &tokio::sync::mpsc::Sender<ConversationEvent>,
) {
    let provider_ids = [
        "anthropic",
        "openai",
        "google",
        "deepseek",
        "openai-codex",
        "opencode",
        "opencode-go",
        "opencodex",
    ];
    let provider_names = [
        "Anthropic Claude",
        "OpenAI ChatGPT",
        "Google Gemini",
        "DeepSeek",
        "OpenAI Codex",
        "OpenCode Zen",
        "OpenCode Go",
        "OpenCodex Local",
    ];
    if selected_idx >= provider_ids.len() {
        return;
    }
    let p_id = provider_ids[selected_idx];
    let p_name = provider_names[selected_idx];

    if p_id == "openai-codex" {
        use dume_provider::oauth;
        match oauth::get_oauth_config("openai-codex") {
            Some(config) => match oauth::generate_pkce() {
                Ok(pkce) => {
                    let state = oauth::generate_state().unwrap_or_else(|_| "dum-e-login".to_string());
                    match oauth::build_authorization_url(&config, &state, &pkce.challenge) {
                        Ok(auth_url) => {
                            let _ = std::process::Command::new("open").arg(&auth_url).spawn();
                            let cancel_token = CancellationToken::new();
                            let tx = stream_tx.clone();
                            let cancel_child = cancel_token.clone();

                            let oauth_store = app.auth_store.clone();

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
                                    oauth_store.save("openai-codex", &cred)?;
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
            },
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

fn select_logout_provider(app: &mut App, selected_idx: usize) {
    let provider_ids = [
        "anthropic",
        "openai",
        "google",
        "deepseek",
        "openai-codex",
        "opencode",
        "opencode-go",
        "opencodex",
    ];
    let provider_names = [
        "Anthropic Claude",
        "OpenAI ChatGPT",
        "Google Gemini",
        "DeepSeek",
        "OpenAI Codex",
        "OpenCode Zen",
        "OpenCode Go",
        "OpenCodex Local",
    ];
    if selected_idx >= provider_ids.len() {
        return;
    }
    let p_id = provider_ids[selected_idx];
    let p_name = provider_names[selected_idx];

    let cred_store = app.auth_store.clone();
    match delete_credential_and_rotate_session(&cred_store, p_id, &mut app.session_id) {
        Ok(()) => {
            app.messages.push(ChatMessage::ui_notice(format!(
                "Logged out from provider '{}' ({})",
                p_id, p_name
            )));
        }
        Err(e) => {
            app.messages.push(ChatMessage::ui_notice(format!(
                "Failed to logout from '{}': {}",
                p_id, e
            )));
        }
    }
    app.modal = ModalState::None;
}

pub fn handle_logout_command(app: &mut App, args: &str) {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        app.modal = ModalState::LogoutSelector { selected_idx: 0 };
    } else if trimmed == "all" {
        let cred_store = app.auth_store.clone();
        let mut targets = std::collections::BTreeSet::new();
        for p in [
            "anthropic",
            "openai",
            "google",
            "deepseek",
            "openai-codex",
            "opencode",
            "opencode-go",
            "opencodex",
            "jev-local-system-one",
        ] {
            targets.insert(p.to_string());
        }
        match cred_store.load() {
            Ok(stored) => {
                for k in stored.keys() {
                    targets.insert(k.clone());
                }
            }
            Err(e) => {
                app.messages.push(ChatMessage::ui_notice(format!(
                    "Failed to load credentials for logout: {e}"
                )));
                return;
            }
        }

        let mut count = 0;
        let mut failures = Vec::new();
        for p in &targets {
            match delete_credential_and_rotate_session(&cred_store, p, &mut app.session_id) {
                Ok(()) => {
                    count += 1;
                }
                Err(e) => {
                    failures.push((p.clone(), e.to_string()));
                }
            }
        }

        if !failures.is_empty() {
            let failure_details = failures
                .iter()
                .map(|(p, err)| format!("{p}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            if count > 0 {
                app.messages.push(ChatMessage::ui_notice(format!(
                    "Logged out from {count} provider(s), but failed for: {failure_details}"
                )));
            } else {
                app.messages.push(ChatMessage::ui_notice(format!(
                    "Failed to log out from providers: {failure_details}"
                )));
            }
        } else {
            app.messages.push(ChatMessage::ui_notice(format!(
                "Logged out from all providers ({count} cleared)"
            )));
        }
    } else {
        let cred_store = app.auth_store.clone();
        match delete_credential_and_rotate_session(
            &cred_store,
            trimmed,
            &mut app.session_id,
        ) {
            Ok(()) => {
                app.messages.push(ChatMessage::ui_notice(format!(
                    "Logged out from provider '{}'",
                    trimmed
                )));
            }
            Err(e) => {
                app.messages.push(ChatMessage::ui_notice(format!(
                    "Failed to logout from '{}': {}",
                    trimmed, e
                )));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    None,
    Latest,
    Id(String),
    Picker { all_dirs: bool },
}

enum PersistEvent {
    Replace {
        conv_id: String,
        msgs: Vec<ChatMessage>,
    },
    Status {
        conv_id: String,
        status: String,
    },
}

pub struct ConversationLock {
    file: std::fs::File,
    path: std::path::PathBuf,
}

impl ConversationLock {
    pub fn try_acquire(conversation_id: &str) -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let locks_dir = std::path::PathBuf::from(home).join(".dume/agent/locks");
        std::fs::create_dir_all(&locks_dir)?;
        let path = locks_dir.join(format!("{}.lock", conversation_id));

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "Conversation '{}' is currently active in another session",
                    conversation_id
                );
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }

        Ok(Self { file, path })
    }
}

impl Drop for ConversationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn repair_conversation_history(messages: &mut Vec<ChatMessage>) -> (bool, Option<String>) {
    let mut repaired = false;
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == dume_provider::Role::Assistant {
            if let Some(tool_calls) = messages[i].tool_calls.clone() {
                if !tool_calls.is_empty() {
                    let mut j = i + 1;
                    let mut existing_tool_ids = std::collections::HashSet::new();
                    while j < messages.len() && messages[j].role == dume_provider::Role::Tool {
                        if let Some(ref tcid) = messages[j].tool_call_id {
                            existing_tool_ids.insert(tcid.clone());
                        }
                        j += 1;
                    }
                    for tc in &tool_calls {
                        if !existing_tool_ids.contains(&tc.id) {
                            let synth = ChatMessage::tool(
                                format!(
                                    "[Interrupted: session terminated before tool '{}' could complete]",
                                    tc.name
                                ),
                                tc.id.clone(),
                            );
                            messages.insert(j, synth);
                            existing_tool_ids.insert(tc.id.clone());
                            j += 1;
                            repaired = true;
                        }
                    }
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }

    let interrupted_user_prompt = if let Some(last) = messages.last() {
        if last.role == dume_provider::Role::User {
            let prompt = last.content.clone();
            messages.pop();
            repaired = true;
            Some(prompt)
        } else {
            None
        }
    } else {
        None
    };

    (repaired, interrupted_user_prompt)
}

enum ConversationEvent {
    Stream(StreamEvent),
    ToolStarted {
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolFinished {
        call_id: String,
        duration_ms: u64,
        is_error: bool,
        error: Option<String>,
        output: String,
    },
    RoutingDecision(dume_worker::routing::RoutingDecision),
    RoutingProbeFinished {
        probe_id: u64,
        message: String,
    },
    Compacted(Vec<ChatMessage>),
    Finished {
        run_id: Option<String>,
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
    BtwRetrying,
    BtwFinished(Result<String, String>),
    CatalogRefreshFinished(Result<Option<usize>, String>),
}

fn selectable_models() -> Vec<ModelInfo> {
    let store = dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    let authenticated: BTreeSet<_> = [
        "anthropic",
        "openai",
        "google",
        "deepseek",
        "openai-codex",
        "opencode",
        "opencode-go",
        "opencodex",
    ]
    .into_iter()
    .filter(|provider| *provider == "opencodex" || store.has_credential(provider))
    .collect();
    dume_provider::ModelCatalog::list_all_models()
        .unwrap_or_default()
        .into_iter()
        .filter(|model| {
            dume_provider::is_model_supported(model)
                && authenticated.contains(model.provider.as_str())
        })
        .collect()
}

fn subagent_command(command: &str, path: &std::path::Path) -> Result<String> {
    let mut settings = SubagentSettings::load_from(path)?;
    if command == "/subagents model inherit" {
        settings.model = None;
        settings.save_to(path)?;
        return Ok("Subagents now inherit the parent model.".into());
    }
    if let Some(selection) = command.strip_prefix("/subagents model ") {
        let model = dume_provider::resolve_model(selection.trim())?;
        let qualified = format!("{}/{}", model.provider, model.id);
        settings.model = Some(qualified.clone());
        settings.save_to(path)?;
        return Ok(format!("Subagent model set to {qualified}."));
    }
    anyhow::bail!("Usage: /subagents model <provider/model|inherit>")
}

fn spawn_codex_catalog_refresh(tx: mpsc::Sender<ConversationEvent>) {
    tokio::spawn(async move {
        let result = dume_provider::ModelCatalog::refresh_codex_cache_if_stale()
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = tx
            .send(ConversationEvent::CatalogRefreshFinished(result))
            .await;
    });
}

fn spawn_opencodex_catalog_refresh(tx: mpsc::Sender<ConversationEvent>, report_error: bool) {
    tokio::spawn(async move {
        let result = dume_provider::opencodex::refresh_cache_if_stale()
            .await
            .map_err(|error| format!("OpenCodex: {error:#}"));
        if result.is_ok() || report_error {
            let _ = tx
                .send(ConversationEvent::CatalogRefreshFinished(result))
                .await;
        }
    });
}

pub struct RoutingDraft {
    pub(crate) settings: dume_worker::RoutingSettings,
    pub(crate) selected: usize,
    pub(crate) cursor: usize,
    pub(crate) key_input: String,
    pub(crate) key_state: String,
    pub(crate) message: String,
    pub(crate) picker_role: Option<dume_worker::RoutingRole>,
    pub(crate) picker_models: Option<Vec<ModelInfo>>,
    pub(crate) picker_selected: usize,
    pub(crate) field_edit: bool,
    pub(crate) probe_confirmation: bool,
    probe_id: u64,
    probe_cancel: Option<CancellationToken>,
    probe_task: Option<tokio::task::AbortHandle>,
}

impl RoutingDraft {
    pub(crate) fn new(settings: dume_worker::RoutingSettings, key_state: String) -> Self {
        Self {
            settings,
            selected: 0,
            cursor: 0,
            key_input: String::new(),
            key_state,
            message: "↑/↓ 항목 · Enter 선택/편집 · Esc 닫기".into(),
            picker_role: None,
            picker_models: None,
            picker_selected: 0,
            field_edit: false,
            probe_confirmation: false,
            probe_id: 0,
            probe_cancel: None,
            probe_task: None,
        }
    }

    fn cancel_probe(&mut self) {
        if let Some(token) = self.probe_cancel.take() {
            token.cancel();
        }
        if let Some(task) = self.probe_task.take() {
            task.abort();
        }
        self.probe_id = NEXT_ROUTING_PROBE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn cancel_probe_for_config_edit(&mut self) {
        if self.probe_cancel.is_some() {
            self.cancel_probe();
            self.message = "설정이 바뀌어 연결 확인을 취소했습니다.".into();
        }
    }

    fn refresh_key_origin(&mut self) {
        let Ok(path) = dume_worker::RoutingSettings::global_path() else {
            self.key_state = "HOME 경로 오류 · 인증 정보 접근 불가".into();
            return;
        };
        let Some(config) = self.settings.classifier.as_ref() else {
            self.key_state = "없음".into();
            return;
        };
        let auth_path = path.with_file_name("auth.json");
        let env = config.backend == dume_provider::ClassifierBackend::OpenCodeZen
            && std::env::var("OPENCODE_API_KEY")
                .ok()
                .is_some_and(|key| !key.trim().is_empty());
        self.key_state = routing_key_origin(config.backend, &auth_path, env);
    }
}

fn routing_snapshot_for_run(
    snapshot: dume_worker::routing::RoutingSnapshot,
    run_status: Option<&str>,
) -> dume_worker::routing::RoutingSnapshot {
    let resuming = routing_run_status(run_status);
    snapshot.with_epoch_preservation(resuming)
}

fn routing_decision_notice(decision: &dume_worker::routing::RoutingDecision) -> Option<ChatMessage> {
    if decision.phase != dume_store::RoutingPhase::Fixed
        || matches!(
            decision.reason.as_str(),
            "disabled" | "all_roles_inherit_original" | "all_roles_match_original_model"
        )
    {
        return None;
    }
    let usage = decision
        .classifier_usage
        .as_ref()
        .map(|usage| {
            format!(
                " · 분류 토큰 {}+{} (가격 미확인)",
                usage.input_tokens, usage.output_tokens
            )
        })
        .unwrap_or_default();
    Some(ChatMessage::ui_notice(format!(
        "라우팅: {} → {} · {}{}",
        decision.original_model,
        decision.effective_model,
        decision.notice_text(),
        usage,
    )))
}

fn routing_run_status(status: Option<&str>) -> bool {
    matches!(status, Some("interrupted" | "needs_attention"))
}

const ROUTING_PROBE_PROMPT: &str = "두 수를 더하는 간단한 함수 작성 요청의 복잡도를 분류하세요.";

async fn routing_probe_request(
    config: dume_provider::ClassifierConfig,
    credentials: dume_provider::CredentialStore,
    cancellation: CancellationToken,
) -> String {
    let credential = tokio::select! { biased;
        _ = cancellation.cancelled() => return "연결 확인이 취소되었습니다.".into(),
        result = credentials.resolve_credential(config.backend.credential_provider()) => result.ok().flatten(),
    };
    let key = credential.and_then(|credential| credential.key.or(credential.access_token));
    let classifier = match dume_provider::SystemOneClassifier::new(&config, key) {
        Ok(classifier) => classifier,
        Err(error) => return routing_probe_error(error.reason_code()).into(),
    };
    let result = tokio::select! { biased;
        _ = cancellation.cancelled() => return "연결 확인이 취소되었습니다.".into(),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), classifier.classify(ROUTING_PROBE_PROMPT, &cancellation)) => result,
    };
    match result {
        Ok(Ok(value)) => format!(
            "연결 확인 완료 · 복잡도 {:.0}%{}",
            value.complex_probability * 100.0,
            value
                .usage
                .map(|usage| format!(
                    " · 토큰 {}+{} (가격 미확인)",
                    usage.input_tokens, usage.output_tokens
                ))
                .unwrap_or_default()
        ),
        Ok(Err(error)) => routing_probe_error(error.reason_code()).into(),
        Err(_) => "분류 요청 시간이 초과되었습니다.".into(),
    }
}

fn routing_probe_error(kind: &str) -> &'static str {
    match kind {
        "missing_credential" => "분류기 인증 정보가 없습니다.",
        "invalid_configuration" => "분류기 설정이 올바르지 않습니다.",
        "http_status" => "분류 서버 요청이 실패했습니다.",
        "invalid_response" => "분류 서버 응답을 확인할 수 없습니다.",
        "timeout" => "분류 요청 시간이 초과되었습니다.",
        "cancelled" => "연결 확인이 취소되었습니다.",
        _ => "분류 서버에 연결할 수 없습니다.",
    }
}

fn spawn_routing_probe(
    tasks: &mut tokio::task::JoinSet<(u64, String)>,
    draft: &mut RoutingDraft,
    config: dume_provider::ClassifierConfig,
    credentials: dume_provider::CredentialStore,
    cancellation: CancellationToken,
) {
    let probe_id = NEXT_ROUTING_PROBE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    draft.probe_id = probe_id;
    draft.probe_cancel = Some(cancellation.clone());
    draft.probe_task = Some(tasks.spawn(async move {
        (
            probe_id,
            routing_probe_request(config, credentials, cancellation).await,
        )
    }));
}

fn routing_key_origin(
    backend: dume_provider::ClassifierBackend,
    auth_path: &std::path::Path,
    env: bool,
) -> String {
    if env && backend == dume_provider::ClassifierBackend::OpenCodeZen {
        return "환경변수".into();
    }
    let provider = backend.credential_provider();
    let store = dume_provider::CredentialStore::new(auth_path);
    let saved = store
        .load()
        .ok()
        .and_then(|credentials| credentials.get(provider).cloned())
        .is_some_and(|credential| {
            credential
                .key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
        });
    if saved {
        "저장됨".into()
    } else {
        "없음".into()
    }
}

impl Drop for RoutingDraft {
    fn drop(&mut self) {
        self.cancel_probe();
    }
}

static NEXT_ROUTING_PROBE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn empty_routing_draft() -> RoutingDraft {
    RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into())
}

fn routing_selected_text(draft: &RoutingDraft) -> String {
    let config = draft.settings.classifier.as_ref();
    match draft.selected {
        2 => config.map(|c| c.base_url.clone()).unwrap_or_default(),
        3 => config.map(|c| c.model.clone()).unwrap_or_default(),
        4 => draft.settings.models.standard.clone().unwrap_or_default(),
        5 => draft.settings.models.complex.clone().unwrap_or_default(),
        6 => draft
            .settings
            .models
            .implementation
            .clone()
            .unwrap_or_default(),
        7 => draft.key_input.clone(),
        _ => String::new(),
    }
}

fn routing_edit_value(draft: &RoutingDraft) -> String {
    match draft.selected {
        2 => draft
            .settings
            .classifier
            .as_ref()
            .map(|c| c.base_url.clone())
            .unwrap_or_default(),
        3 => draft
            .settings
            .classifier
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn routing_begin_field_edit(draft: &mut RoutingDraft) {
    draft.cancel_probe_for_config_edit();
    if matches!(draft.selected, 2 | 3) && draft.settings.classifier.is_none() {
        draft.settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::OpenCodeZen,
            base_url: String::new(),
            model: String::new(),
        });
    }
    draft.cursor = routing_edit_value(draft).chars().count();
    draft.field_edit = true;
}

fn routing_insert_char(draft: &mut RoutingDraft, ch: char) {
    draft.cancel_probe_for_config_edit();
    if draft.selected == 7 {
        draft.key_input.insert(
            draft
                .key_input
                .char_indices()
                .nth(draft.cursor)
                .map_or(draft.key_input.len(), |(i, _)| i),
            ch,
        );
        draft.cursor += 1;
        return;
    }
    let Some(config) = draft.settings.classifier.as_mut() else {
        return;
    };
    let text = match draft.selected {
        2 => &mut config.base_url,
        3 => &mut config.model,
        _ => return,
    };
    let at = text
        .char_indices()
        .nth(draft.cursor)
        .map_or(text.len(), |(i, _)| i);
    text.insert(at, ch);
    draft.cursor += 1;
}

fn routing_delete_char(draft: &mut RoutingDraft) {
    draft.cancel_probe_for_config_edit();
    if draft.cursor == 0 {
        return;
    }
    if draft.selected == 7 {
        let mut chars = draft.key_input.chars().collect::<Vec<_>>();
        if draft.cursor <= chars.len() {
            chars.remove(draft.cursor - 1);
            draft.key_input = chars.into_iter().collect();
            draft.cursor -= 1;
        }
        return;
    }
    let Some(config) = draft.settings.classifier.as_mut() else {
        return;
    };
    let text = match draft.selected {
        2 => &mut config.base_url,
        3 => &mut config.model,
        _ => return,
    };
    let mut chars = text.chars().collect::<Vec<_>>();
    if draft.cursor <= chars.len() {
        chars.remove(draft.cursor - 1);
        *text = chars.into_iter().collect();
        draft.cursor -= 1;
    }
}

fn routing_delete_forward(draft: &mut RoutingDraft) {
    draft.cancel_probe_for_config_edit();
    let text = match draft.selected {
        2 => draft
            .settings
            .classifier
            .as_mut()
            .map(|config| &mut config.base_url),
        3 => draft
            .settings
            .classifier
            .as_mut()
            .map(|config| &mut config.model),
        7 => {
            delete_char_at(&mut draft.key_input, draft.cursor);
            return;
        }
        _ => None,
    };
    if let Some(text) = text {
        delete_char_at(text, draft.cursor);
    }
}

fn delete_char_at(text: &mut String, cursor: usize) {
    let Some((start, ch)) = text.char_indices().nth(cursor) else {
        return;
    };
    text.replace_range(start..start + ch.len_utf8(), "");
}

fn routing_delete_to_beginning(draft: &mut RoutingDraft) {
    draft.cancel_probe_for_config_edit();
    if draft.cursor == 0 {
        return;
    }
    if draft.selected == 7 {
        let mut chars = draft.key_input.chars().collect::<Vec<_>>();
        chars.drain(0..draft.cursor.min(chars.len()));
        draft.key_input = chars.into_iter().collect();
        draft.cursor = 0;
        return;
    }
    let Some(config) = draft.settings.classifier.as_mut() else {
        return;
    };
    let text = match draft.selected {
        2 => &mut config.base_url,
        3 => &mut config.model,
        _ => return,
    };
    let mut chars = text.chars().collect::<Vec<_>>();
    chars.drain(0..draft.cursor.min(chars.len()));
    *text = chars.into_iter().collect();
    draft.cursor = 0;
}

fn routing_edit_left(draft: &mut RoutingDraft) {
    draft.cursor = draft.cursor.saturating_sub(1);
}
fn routing_edit_right(draft: &mut RoutingDraft) {
    draft.cursor = (draft.cursor + 1).min(routing_selected_text(draft).chars().count());
}

fn routing_open_role_picker(draft: &mut RoutingDraft) {
    if dume_worker::RoutingSettings::global_path().is_err() {
        draft.message = "HOME 경로 오류 · 모델 인증 정보를 읽지 않았습니다.".into();
        return;
    }
    let models = selectable_models();
    routing_begin_role_picker(draft, models);
}

fn routing_begin_role_picker(draft: &mut RoutingDraft, models: Vec<ModelInfo>) {
    let role = match draft.selected {
        4 => dume_worker::RoutingRole::StandardPlanning,
        5 => dume_worker::RoutingRole::ComplexPlanning,
        _ => dume_worker::RoutingRole::Implementation,
    };
    draft.picker_role = Some(role);
    draft.picker_models = Some(models);
    draft.picker_selected = 0;
}

fn routing_set_role_model(
    draft: &mut RoutingDraft,
    role: dume_worker::RoutingRole,
    selected: Option<&ModelInfo>,
) {
    let value = selected.map(|model| format!("{}/{}", model.provider, model.id));
    match role {
        dume_worker::RoutingRole::StandardPlanning => draft.settings.models.standard = value,
        dume_worker::RoutingRole::ComplexPlanning => draft.settings.models.complex = value,
        dume_worker::RoutingRole::Implementation => draft.settings.models.implementation = value,
    }
}

pub enum ModelSelectorTarget {
    Conversation,
    Subagents {
        path: std::path::PathBuf,
        model: Option<String>,
    },
}

impl ModelSelectorTarget {
    pub fn model_offset(&self) -> usize {
        usize::from(matches!(self, Self::Subagents { .. }))
    }

    pub fn current_model<'a>(&'a self, parent_model: &'a str) -> &'a str {
        match self {
            Self::Conversation => parent_model,
            Self::Subagents { model, .. } => model.as_deref().unwrap_or_default(),
        }
    }
}

pub enum ModalState {
    None,
    RoutingSettings(RoutingDraft),
    RoutingProbeConfirm(RoutingDraft),
    ModelSelector {
        target: ModelSelectorTarget,
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
    LogoutSelector {
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
    ResumePicker {
        sessions: Vec<RecentSessionSummary>,
        selected_idx: usize,
    },
}

impl ModalState {
    fn model_selector(
        models: Vec<ModelInfo>,
        parent_model: &str,
        target: ModelSelectorTarget,
    ) -> Self {
        let current = target.current_model(parent_model);
        let selected_idx = models
            .iter()
            .position(|m| m.id == current || format!("{}/{}", m.provider, m.id) == current)
            .map(|index| index + target.model_offset())
            .unwrap_or(0);
        let mut provider_tabs = vec!["All".to_string()];
        for model in &models {
            if !provider_tabs
                .iter()
                .any(|tab| tab.eq_ignore_ascii_case(&model.provider))
            {
                provider_tabs.push(model.provider.clone());
            }
        }
        Self::ModelSelector {
            target,
            filtered: models.clone(),
            models,
            selected_idx,
            filter: String::new(),
            provider_tabs,
            active_tab_idx: 0,
        }
    }

    fn subagent_model_selector(models: Vec<ModelInfo>, path: std::path::PathBuf) -> Result<Self> {
        let settings = SubagentSettings::load_from(&path)?;
        Ok(Self::model_selector(
            models,
            "",
            ModelSelectorTarget::Subagents {
                path,
                model: settings.model,
            },
        ))
    }
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
    pub session_usage_incomplete: usize,
    pub current_turn_usage: dume_provider::types::TokenUsage,
    pub session_id: String,
    pub prompt_history: Vec<String>,
    pub history_index: Option<usize>,
    pub temp_input: String,
    pub busy_started_at: Option<std::time::Instant>,
    pub recent_sessions: Vec<RecentSessionSummary>,
    pub conversation_id: String,
    pub current_run_id: Option<String>,
    pub last_run_metrics: Option<dume_store::RunMetrics>,
    pub session_elapsed_ms: i64,
    pub session_timing_complete: bool,
    pub last_routing_decision: Option<dume_worker::routing::RoutingDecision>,
    pub active_tools: Vec<crate::tool_display::ActiveToolItem>,
    pub streaming_tools: Vec<crate::tool_display::StreamingToolCall>,
    pub selected_tool_id: Option<String>,
    pub expanded_tools: std::collections::HashSet<String>,
    pub auth_store: dume_provider::CredentialStore,
    active_request_model: Option<String>,
    active_thinking: Option<ThinkingLevel>,
    thinking_preferences: dume_provider::reasoning::ReasoningPreferences,
    thinking_path: std::path::PathBuf,
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

fn format_duration(milliseconds: i64) -> String {
    let seconds = milliseconds.max(0).saturating_add(500) / 1000;
    if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{}s", seconds)
    }
}

fn status_label(status: &str) -> &'static str {
    match status {
        "succeeded" => "DONE",
        "failed" | "interrupted" => "ERROR",
        "cancelled" => "CANCELLED",
        "needs_attention" => "ATTENTION",
        _ => "RUN",
    }
}

fn format_session_metrics(metrics: &dume_store::ConversationRuntimeMetrics) -> String {
    let mut text = format!("Session runs: {}", metrics.runs.len());
    for run in &metrics.runs {
        if !matches!(&run.scope, RuntimeScope::Conversation(_)) {
            continue;
        }
        text.push_str(&format!(
            "\n{} {}{}",
            status_label(&run.status),
            format_duration(run.elapsed_ms),
            if run.timing_complete {
                ""
            } else {
                " (lower bound)"
            }
        ));
        for attempt in metrics
            .model_attempts
            .iter()
            .filter(|attempt| attempt.run_id == run.run_id)
        {
            let duration = attempt
                .duration_ms
                .map(format_duration)
                .unwrap_or_else(|| "time missing".into());
            let usage = attempt
                .usage
                .as_ref()
                .map(|usage| {
                    let input = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_i64);
                    let output = usage
                        .get("output_tokens")
                        .and_then(serde_json::Value::as_i64);
                    match (input, output) {
                        (Some(input), Some(output)) => format!("usage in={input} out={output}"),
                        _ => "usage missing/incomplete".into(),
                    }
                })
                .unwrap_or_else(|| "usage missing".into());
            text.push_str(&format!(
                "\n  Model {}: {}{}; {}",
                attempt.model,
                duration,
                if attempt.state == "failed" {
                    " (failed/retry)"
                } else {
                    ""
                },
                usage
            ));
            if let Some(identity) = attempt
                .usage
                .as_ref()
                .and_then(|usage| usage.get("response_identity"))
            {
                text.push_str(&format!(
                    "; server={} response={}",
                    identity["model"].as_str().unwrap_or("unknown"),
                    identity["response_id"].as_str().unwrap_or("unknown")
                ));
            }
        }
    }
    for tool in &metrics.tool_executions {
        text.push_str(&format!("\nTool {}", tool.name));
    }
    for attempt in &metrics.tool_attempts {
        text.push_str(&format!(
            "\n  Tool attempt {} {}",
            attempt.state,
            attempt
                .duration_ms
                .map(format_duration)
                .unwrap_or_else(|| "time missing".into())
        ));
    }
    text
}

fn refresh_session_timing(app: &mut App, store: &HarnessStore) -> Result<()> {
    let metrics = store.conversation_runtime_metrics(&app.conversation_id)?;
    let primary: Vec<_> = metrics
        .runs
        .iter()
        .filter(|run| matches!(&run.scope, RuntimeScope::Conversation(_)))
        .collect();
    app.session_elapsed_ms = primary.iter().map(|run| run.elapsed_ms).sum();
    app.session_timing_complete =
        !primary.is_empty() && primary.iter().all(|run| run.timing_complete);
    app.last_run_metrics = primary.last().map(|run| dume_store::RunMetrics {
        run_id: run.run_id.clone(),
        status: run.status.clone(),
        elapsed_ms: run.elapsed_ms,
        timing_complete: run.timing_complete,
    });
    let mut usage = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut incomplete = 0;
    for attempt in &metrics.model_attempts {
        let Some(value) = attempt.usage.as_ref() else {
            incomplete += 1;
            continue;
        };
        let input = value
            .get("input_tokens")
            .and_then(serde_json::Value::as_i64);
        let output = value
            .get("output_tokens")
            .and_then(serde_json::Value::as_i64);
        if input.is_none() || output.is_none() {
            incomplete += 1;
        }
        let input = input.unwrap_or(0);
        let output = output.unwrap_or(0);
        usage.0 += input;
        usage.1 += output;
        usage.2 += value
            .get("total_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(input + output);
        usage.3 += value
            .get("cache_read_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        usage.4 += value
            .get("cache_write_tokens")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
    }
    app.session_usage = usage;
    app.session_usage_incomplete = incomplete;
    Ok(())
}

fn find_resumable_run(
    store: &HarnessStore,
    conversation_id: &str,
    requested: &str,
) -> Result<dume_store::RunRecord> {
    let metrics = store.conversation_runtime_metrics(conversation_id)?;
    metrics.runs.into_iter().rev().find(|run| {
        (requested.is_empty() || run.run_id == requested)
            && matches!(run.status.as_str(), "interrupted" | "needs_attention")
            && store.tool_executions_for_run(&run.run_id).is_ok_and(|tools| tools.iter().all(|tool| tool.state != "outcome_unknown"))
    }).ok_or_else(|| anyhow::anyhow!(if requested.is_empty() {
        "No interrupted or resolved-attention run is resumable in this conversation."
    } else {
        "Run is not resumable here, is not paused for recovery, or has unresolved unsafe tool outcomes."
    }))
}

pub fn persist_last_used_model(model: &str) -> Result<()> {
    persist_model_to(&dume_provider::reasoning::preferences_path(), model)
}

fn persist_model_to(path: &std::path::Path, model: &str) -> Result<()> {
    dume_provider::reasoning::update_settings(path, |settings| {
        settings.insert("last_used_model".into(), serde_json::json!(model));
        Ok(())
    })
}

fn load_recent_sessions_overview() -> Vec<RecentSessionSummary> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let db_path = std::path::PathBuf::from(home).join(".dume/agent/harness.db");
    let artifacts_dir = std::path::PathBuf::from(".dume/artifacts");
    if let Ok(store) = HarnessStore::open(&db_path, &artifacts_dir) {
        let current_dir = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string));
        let list = store
            .list_recent_conversations(current_dir.as_deref(), 5)
            .unwrap_or_default();
        if !list.is_empty() {
            return list;
        }
        store.list_recent_sessions(5).unwrap_or_default()
    } else {
        Vec::new()
    }
}

impl App {
    fn confirm_model_selection(&mut self) -> Result<()> {
        let ModalState::ModelSelector {
            target,
            filtered,
            selected_idx,
            ..
        } = &self.modal
        else {
            return Ok(());
        };
        let selected = selected_idx
            .checked_sub(target.model_offset())
            .and_then(|index| filtered.get(index))
            .map(|model| format!("{}/{}", model.provider, model.id));
        if selected.is_none() && (*selected_idx != 0 || target.model_offset() == 0) {
            self.modal = ModalState::None;
            return Ok(());
        }
        match target {
            ModelSelectorTarget::Conversation => {
                let new_model = selected.expect("conversation model selection");
                persist_model_to(&self.thinking_path, &new_model)?;
                self.messages.push(ChatMessage::ui_notice(format!(
                    "Switched model to '{new_model}'"
                )));
                self.model = new_model;
                self.restore_thinking();
                self.session_id = dume_provider::types::StreamRequestContext::new().session_id;
            }
            ModelSelectorTarget::Subagents { path, .. } => {
                SubagentSettings {
                    model: selected.clone(),
                }
                .save_to(path)?;
                self.messages.push(ChatMessage::ui_notice(match selected {
                    Some(model) => format!("Subagent model set to {model}."),
                    None => "Subagents now inherit the parent model.".into(),
                }));
            }
        }
        self.modal = ModalState::None;
        Ok(())
    }

    fn begin_run(&mut self, run_id: String) {
        self.current_run_id = Some(run_id);
        self.last_routing_decision = None;
        self.active_request_model = None;
        self.active_thinking = None;
    }

    fn status_model(&self) -> &str {
        if self.is_busy {
            self.active_request_model
                .as_deref()
                .or_else(|| {
                    self.last_routing_decision
                        .as_ref()
                        .map(|decision| decision.effective_model.as_str())
                })
                .unwrap_or(&self.model)
        } else {
            &self.model
        }
    }

    fn restore_thinking(&mut self) {
        self.thinking = dume_provider::resolve_model(&self.model)
            .ok()
            .map(|model| self.thinking_for(&model))
            .unwrap_or_default();
    }

    fn thinking_for(&self, model: &ModelInfo) -> ThinkingLevel {
        let effort = self
            .thinking_preferences
            .get(&model.qualified_id())
            .copied()
            .unwrap_or_default();
        if model.validate_effort(effort).is_ok() {
            effort
        } else {
            ThinkingLevel::Default
        }
    }

    fn cycle_thinking(&mut self, model: ModelInfo) {
        let levels = model.supported_efforts();
        if levels.len() <= 1 {
            self.messages.push(ChatMessage::ui_notice(format!(
                "Thinking adjustment is unavailable for {}.",
                model.qualified_id()
            )));
            return;
        }
        let current = self.thinking_for(&model);
        let index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        let next = levels[(index + 1) % levels.len()];
        match dume_provider::reasoning::save_effort(&self.thinking_path, &model, next) {
            Ok(()) => {
                self.thinking_preferences.insert(model.qualified_id(), next);
                self.restore_thinking();
            }
            Err(error) => self.messages.push(ChatMessage::ui_notice(format!(
                "Could not save thinking setting: {error:#}"
            ))),
        }
    }

    fn status_thinking(&self) -> ThinkingLevel {
        if self.is_busy {
            self.active_thinking.unwrap_or(self.thinking)
        } else {
            self.thinking
        }
    }

    fn picker_thinking(&self) -> ThinkingLevel {
        if let ModalState::ModelSelector {
            target,
            filtered,
            selected_idx,
            ..
        } = &self.modal
        {
            if let Some(model) = selected_idx
                .checked_sub(target.model_offset())
                .and_then(|index| filtered.get(index))
            {
                return self.thinking_for(model);
            }
        }
        self.thinking
    }

    pub fn new(model: impl Into<String>) -> Self {
        Self::with_auth_store(
            model,
            dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path()),
        )
    }

    pub fn with_auth_store(
        model: impl Into<String>,
        auth_store: dume_provider::CredentialStore,
    ) -> Self {
        let m = model.into();
        let effective_model = if m == "anthropic/claude-sonnet-4-5" || m.is_empty() {
            get_persisted_or_default_model(None)
        } else {
            get_persisted_or_default_model(Some(&m))
        };

        let recent_sessions = load_recent_sessions_overview();
        let thinking_path = dume_provider::reasoning::preferences_path();
        let preferences = dume_provider::reasoning::load_preferences(&thinking_path);
        let error = preferences
            .as_ref()
            .err()
            .map(|error| format!("Could not load thinking settings: {error:#}"));
        let thinking_preferences = preferences.unwrap_or_default();
        let thinking = dume_provider::resolve_model(&effective_model)
            .ok()
            .and_then(|model| {
                thinking_preferences
                    .get(&model.qualified_id())
                    .copied()
                    .filter(|effort| model.validate_effort(*effort).is_ok())
            })
            .unwrap_or_default();

        Self {
            messages: error.into_iter().map(ChatMessage::ui_notice).collect(),
            input_buffer: String::new(),
            cursor_pos: 0, // In characters, not bytes!
            streaming_text: String::new(),
            scroll_offset: 0,
            auto_scroll: true,
            is_busy: false,
            model: effective_model,
            thinking,
            thinking_preferences,
            thinking_path,
            active_thinking: None,
            theme: Theme::default(),
            skills: SkillRegistry::load_default(),
            autocomplete_index: 0,
            autocomplete_dismissed: false,
            modal: ModalState::None,
            last_ctrl_c: None,
            available_update: None,
            session_usage: (0, 0, 0, 0, 0),
            session_usage_incomplete: 0,
            current_turn_usage: dume_provider::types::TokenUsage::default(),
            session_id: dume_provider::types::StreamRequestContext::new().session_id,
            prompt_history: Vec::new(),
            history_index: None,
            temp_input: String::new(),
            busy_started_at: None,
            recent_sessions,
            conversation_id: dume_provider::types::StreamRequestContext::new().session_id,
            current_run_id: None,
            last_run_metrics: None,
            session_elapsed_ms: 0,
            session_timing_complete: false,
            last_routing_decision: None,
            active_request_model: None,
            active_tools: Vec::new(),
            streaming_tools: Vec::new(),
            selected_tool_id: None,
            expanded_tools: std::collections::HashSet::new(),
            auth_store,
        }
    }

    pub fn tool_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for msg in &self.messages {
            if msg.role == dume_provider::types::Role::Tool {
                if let Some(ref id) = msg.tool_call_id {
                    ids.push(id.clone());
                }
            }
        }
        for active in &self.active_tools {
            if !ids.iter().any(|id| id == &active.call_id) {
                ids.push(active.call_id.clone());
            }
        }
        for streaming in &self.streaming_tools {
            let id = if streaming.id.is_empty() {
                streaming.name.clone()
            } else {
                streaming.id.clone()
            };
            if !id.is_empty() && !ids.iter().any(|existing| existing == &id) {
                ids.push(id);
            }
        }
        ids
    }

    pub fn select_prev_tool(&mut self) {
        let ids = self.tool_ids();
        if ids.is_empty() {
            self.selected_tool_id = None;
            return;
        }
        let current_idx = self
            .selected_tool_id
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id));
        let next_idx = match current_idx {
            Some(idx) => idx.saturating_sub(1),
            None => ids.len().saturating_sub(1),
        };
        self.selected_tool_id = ids.get(next_idx).cloned();
    }

    pub fn select_next_tool(&mut self) {
        let ids = self.tool_ids();
        if ids.is_empty() {
            self.selected_tool_id = None;
            return;
        }
        let current_idx = self
            .selected_tool_id
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id));
        let next_idx = match current_idx {
            Some(idx) => (idx + 1).min(ids.len().saturating_sub(1)),
            None => 0,
        };
        self.selected_tool_id = ids.get(next_idx).cloned();
    }

    pub fn toggle_selected_tool_details(&mut self) {
        let ids = self.tool_ids();
        if ids.is_empty() {
            return;
        }
        if self.selected_tool_id.is_none() {
            self.selected_tool_id = ids.last().cloned();
        }
        if let Some(ref id) = self.selected_tool_id {
            if self.expanded_tools.contains(id) {
                self.expanded_tools.remove(id);
            } else {
                self.expanded_tools.insert(id.clone());
            }
        }
    }

    pub fn transcript_config(&self) -> crate::tool_display::TranscriptConfig<'_> {
        crate::tool_display::TranscriptConfig {
            expanded_tools: &self.expanded_tools,
            selected_tool_id: self.selected_tool_id.as_deref(),
            active_tools: &self.active_tools,
            streaming_tools: &self.streaming_tools,
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

        for skill in self.skills.list() {
            if query.is_empty() || skill.name.to_lowercase().starts_with(&query) {
                items.push(AutocompleteItem {
                    name: format!("/{}", skill.name),
                    description: skill.description.clone(),
                });
            }
        }

        // Built-in commands
        let builtins = [
            ("/routing", "라우팅 설정 열기"),
            (
                "/model",
                "Switch model (e.g. /model anthropic/claude-sonnet-4-5)",
            ),
            ("/resume", "Resume a previous conversation session"),
            ("/run", "Explicitly resume a paused durable run"),
            ("/subagents", "Select the model used by subagents"),
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
                "/session",
                "Display persisted run, model-attempt, and tool timing",
            ),
            ("/queue", "List, withdraw, or resume queued inputs"),
            ("/follow-up", "Queue a follow-up input"),
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

        items.sort_by_key(|item| item.name != "/model");
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

    pub fn delete_forward(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        if self.cursor_pos < chars.len() {
            chars.remove(self.cursor_pos);
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

    pub fn delete_to_beginning(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        if chars.is_empty() || self.cursor_pos == 0 {
            return;
        }
        let pos = self.cursor_pos.min(chars.len());
        let line_start = chars[..pos]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);

        if line_start == pos {
            if pos > 0 && chars[pos - 1] == '\n' {
                chars.remove(pos - 1);
                self.cursor_pos = pos - 1;
            }
        } else {
            chars.drain(line_start..pos);
            self.cursor_pos = line_start;
        }
        self.input_buffer = chars.into_iter().collect();
    }

    pub fn delete_to_end(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let pos = self.cursor_pos.min(chars.len());
        if pos >= chars.len() {
            return;
        }
        let line_end = chars[pos..]
            .iter()
            .position(|&c| c == '\n')
            .map(|idx| pos + idx)
            .unwrap_or(chars.len());

        if line_end == pos {
            chars.remove(pos);
        } else {
            chars.drain(pos..line_end);
        }
        self.input_buffer = chars.into_iter().collect();
    }

    pub fn delete_word_backward(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let pos = self.cursor_pos.min(chars.len());
        if pos == 0 {
            return;
        }
        let mut idx = pos;
        while idx > 0 && chars[idx - 1].is_whitespace() && chars[idx - 1] != '\n' {
            idx -= 1;
        }
        if idx > 0 {
            if chars[idx - 1].is_alphanumeric() || chars[idx - 1] == '_' {
                while idx > 0 && (chars[idx - 1].is_alphanumeric() || chars[idx - 1] == '_') {
                    idx -= 1;
                }
            } else if chars[idx - 1] != '\n' {
                while idx > 0 && !chars[idx - 1].is_alphanumeric() && !chars[idx - 1].is_whitespace() {
                    idx -= 1;
                }
            } else {
                idx -= 1;
            }
        }
        chars.drain(idx..pos);
        self.cursor_pos = idx;
        self.input_buffer = chars.into_iter().collect();
    }

    pub fn delete_word_forward(&mut self) {
        let mut chars: Vec<char> = self.input_buffer.chars().collect();
        let pos = self.cursor_pos.min(chars.len());
        if pos >= chars.len() {
            return;
        }
        let mut idx = pos;
        while idx < chars.len() && chars[idx].is_whitespace() && chars[idx] != '\n' {
            idx += 1;
        }
        if idx < chars.len() {
            if chars[idx].is_alphanumeric() || chars[idx] == '_' {
                while idx < chars.len() && (chars[idx].is_alphanumeric() || chars[idx] == '_') {
                    idx += 1;
                }
            } else if chars[idx] != '\n' {
                while idx < chars.len() && !chars[idx].is_alphanumeric() && !chars[idx].is_whitespace() {
                    idx += 1;
                }
            } else {
                idx += 1;
            }
        }
        chars.drain(pos..idx);
        self.input_buffer = chars.into_iter().collect();
    }

    pub fn cursor_word_left(&mut self) {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let mut pos = self.cursor_pos.min(chars.len());
        if pos == 0 {
            return;
        }
        while pos > 0 && chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        if pos > 0 {
            if chars[pos - 1].is_alphanumeric() || chars[pos - 1] == '_' {
                while pos > 0 && (chars[pos - 1].is_alphanumeric() || chars[pos - 1] == '_') {
                    pos -= 1;
                }
            } else {
                while pos > 0 && !chars[pos - 1].is_alphanumeric() && !chars[pos - 1].is_whitespace() {
                    pos -= 1;
                }
            }
        }
        self.cursor_pos = pos;
    }

    pub fn cursor_word_right(&mut self) {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let mut pos = self.cursor_pos.min(chars.len());
        if pos >= chars.len() {
            return;
        }
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }
        if pos < chars.len() {
            if chars[pos].is_alphanumeric() || chars[pos] == '_' {
                while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
                    pos += 1;
                }
            } else {
                while pos < chars.len() && !chars[pos].is_alphanumeric() && !chars[pos].is_whitespace() {
                    pos += 1;
                }
            }
        }
        self.cursor_pos = pos;
    }

    pub fn cursor_up(&mut self) -> bool {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let pos = self.cursor_pos.min(chars.len());
        let line_start = chars[..pos]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);

        if line_start == 0 {
            return false;
        }

        let col = pos - line_start;
        let prev_line_end = line_start - 1;
        let prev_line_start = chars[..prev_line_end]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let prev_line_len = prev_line_end - prev_line_start;

        self.cursor_pos = prev_line_start + col.min(prev_line_len);
        true
    }

    pub fn cursor_down(&mut self) -> bool {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let pos = self.cursor_pos.min(chars.len());
        let line_start = chars[..pos]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        let col = pos - line_start;

        let next_line_start = match chars[pos..].iter().position(|&c| c == '\n') {
            Some(idx) => pos + idx + 1,
            None => return false,
        };

        let next_line_end = chars[next_line_start..]
            .iter()
            .position(|&c| c == '\n')
            .map(|idx| next_line_start + idx)
            .unwrap_or(chars.len());
        let next_line_len = next_line_end - next_line_start;

        self.cursor_pos = next_line_start + col.min(next_line_len);
        true
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
            ConversationEvent::RoutingProbeFinished { probe_id, message } => {
                if let ModalState::RoutingSettings(draft) = &mut self.modal {
                    if draft.probe_id != probe_id {
                        return;
                    }
                    draft.message = message;
                    draft.probe_cancel = None;
                    draft.probe_task = None;
                }
            }
            ConversationEvent::RoutingDecision(decision) => {
                self.messages.retain(|message| {
                    message.is_model_visible() || !message.content.starts_with("라우팅: ")
                });
                if let Some(notice) = routing_decision_notice(&decision) {
                    self.messages.push(notice);
                }
                self.active_request_model = None;
                self.last_routing_decision = Some(decision);
            }
            ConversationEvent::ToolStarted {
                call_id,
                name,
                arguments,
            } => {
                if let Some(pos) = self.streaming_tools.iter().position(|s| {
                    (!s.id.is_empty() && s.id == call_id)
                        || (!s.name.is_empty() && s.name == name)
                }) {
                    self.streaming_tools.remove(pos);
                }
                self.active_tools.push(crate::tool_display::ActiveToolItem {
                    call_id,
                    name,
                    arguments,
                    status: crate::tool_display::ActiveToolStatus::Running {
                        started_at: std::time::Instant::now(),
                    },
                    output: None,
                });
            }
            ConversationEvent::ToolFinished {
                call_id,
                duration_ms,
                is_error,
                error,
                output,
            } => {
                if let Some(tool) = self.active_tools.iter_mut().find(|t| t.call_id == call_id) {
                    tool.output = Some(output);
                    tool.status = if is_error {
                        crate::tool_display::ActiveToolStatus::Failed {
                            duration_ms,
                            error: error.unwrap_or_default(),
                        }
                    } else {
                        crate::tool_display::ActiveToolStatus::Completed { duration_ms }
                    };
                }
            }
            ConversationEvent::Stream(StreamEvent::AttemptStarted { model, context }) => {
                self.active_request_model = Some(model);
                self.active_thinking = context.reasoning_effort;
            }
            ConversationEvent::Stream(StreamEvent::TextDelta(delta)) => {
                self.streaming_text.push_str(&delta);
            }
            ConversationEvent::Stream(StreamEvent::Retrying { .. }) => {
                self.streaming_text.clear();
                self.active_tools.clear();
                self.streaming_tools.clear();
                self.current_turn_usage = dume_provider::types::TokenUsage::default();
            }
            ConversationEvent::Stream(StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            }) => {
                while self.streaming_tools.len() <= index {
                    self.streaming_tools
                        .push(crate::tool_display::StreamingToolCall::default());
                }
                if let Some(tool_id) = id {
                    if !tool_id.is_empty() {
                        self.streaming_tools[index].id = tool_id;
                    }
                }
                if let Some(tool_name) = name {
                    if !tool_name.is_empty() {
                        self.streaming_tools[index].name = tool_name;
                    }
                }
                self.streaming_tools[index].arguments.push_str(&arguments_delta);
            }
            ConversationEvent::Stream(StreamEvent::Usage(usage)) => {
                self.current_turn_usage.merge_cumulative(&usage);
            }
            ConversationEvent::Stream(StreamEvent::Completed { .. }) => {
                self.current_turn_usage = dume_provider::types::TokenUsage::default();
            }
            ConversationEvent::Stream(_) => {}
            ConversationEvent::Compacted(_) => {}
            ConversationEvent::Finished {
                run_id,
                messages,
                error,
                session_id,
            } => {
                if run_id
                    .as_ref()
                    .is_some_and(|run_id| self.current_run_id.as_deref() != Some(run_id.as_str()))
                {
                    return;
                }
                let decision_notice = self
                    .last_routing_decision
                    .as_ref()
                    .and_then(routing_decision_notice);
                if let Some(session_id) = session_id {
                    self.session_id = session_id;
                }
                self.current_turn_usage = dume_provider::types::TokenUsage::default();
                self.messages = messages;
                self.messages.retain(|message| {
                    message.is_model_visible() || !message.content.starts_with("라우팅: ")
                });
                if let Some(notice) = decision_notice {
                    self.messages
                        .retain(|message| message.content != notice.content);
                    self.messages.push(notice);
                }

                if let Some(error) = error {
                    self.messages
                        .push(ChatMessage::ui_notice(format!("Error: {}", error)));
                }
                self.streaming_text.clear();
                self.active_tools.clear();
                self.streaming_tools.clear();
                self.is_busy = false;
                self.active_request_model = None;
                self.active_thinking = None;
                self.busy_started_at = None;
            }
            ConversationEvent::OAuthComplete { provider, result } => {
                match result {
                    Ok(()) => {
                        self.session_id =
                            dume_provider::types::StreamRequestContext::new().session_id;
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
            ConversationEvent::BtwRetrying => {
                if let ModalState::BtwChat {
                    streaming_reply, ..
                } = &mut self.modal
                {
                    streaming_reply.clear();
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
                    if let Ok(text) = res {
                        history.push(("assistant".to_string(), text));
                    }
                    streaming_reply.clear();
                }
            }
            ConversationEvent::CatalogRefreshFinished(result) => match result {
                Ok(Some(count)) => {
                    self.restore_thinking();
                    self.messages.push(ChatMessage::ui_notice(format!(
                        "Model catalog updated ({count} models)."
                    )));
                    if let ModalState::ModelSelector {
                        target,
                        models,
                        filtered,
                        selected_idx,
                        filter,
                        provider_tabs,
                        active_tab_idx,
                    } = &mut self.modal
                    {
                        let selected = selected_idx.checked_sub(target.model_offset())
                            .and_then(|index| filtered.get(index))
                            .map(|m| (m.provider.clone(), m.id.clone()));
                        let active_provider = provider_tabs
                            .get(*active_tab_idx)
                            .cloned()
                            .unwrap_or_default();
                        *models = selectable_models();
                        *provider_tabs = std::iter::once("All".to_string())
                            .chain(models.iter().map(|m| m.provider.clone()))
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        if let Some(index) =
                            provider_tabs.iter().position(|p| p == &active_provider)
                        {
                            *active_tab_idx = index;
                        } else {
                            *active_tab_idx = 0;
                        }
                        let tab = &provider_tabs[*active_tab_idx];
                        let query = filter.to_lowercase();
                        *filtered = models
                            .iter()
                            .filter(|m| {
                                (tab == "All" || &m.provider == tab)
                                    && (m.id.to_lowercase().contains(&query)
                                        || m.provider.to_lowercase().contains(&query))
                            })
                            .cloned()
                            .collect();
                        *selected_idx = selected
                            .and_then(|(provider, id)| {
                                filtered
                                    .iter()
                                    .position(|m| m.provider == provider && m.id == id)
                                    .map(|index| index + target.model_offset())
                            })
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
    run_tui_with_target(
        model,
        ResumeTarget::None,
        dume_worker::routing::RoutingSnapshot::load_default(false),
    )
    .await
}

pub async fn run_tui_with_target(
    model: &str,
    target: ResumeTarget,
    routing_snapshot: dume_worker::routing::RoutingSnapshot,
) -> Result<()> {
    let _session = TerminalSession::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    run_app(&mut terminal, model, target, routing_snapshot).await
}

#[cfg(unix)]
async fn wait_for_signal(
    sighup: &mut tokio::signal::unix::Signal,
    sigterm: &mut tokio::signal::unix::Signal,
) {
    tokio::select! {
        _ = sighup.recv() => (),
        _ = sigterm.recv() => (),
    }
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    model: &str,
    target: ResumeTarget,
    mut routing_snapshot: dume_worker::routing::RoutingSnapshot,
) -> Result<()> {
    let keybindings = KeyBindings::load_default()?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<io::Result<Event>>();
    let (stream_tx, mut stream_rx) = mpsc::channel::<ConversationEvent>(100);
    let cancellation = CancellationToken::new();

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let db_path = std::path::PathBuf::from(home).join(".dume/agent/harness.db");
    let artifacts_dir = std::path::PathBuf::from(".dume/artifacts");
    let harness_store = Arc::new(HarnessStore::open(&db_path, &artifacts_dir)?);
    let raw_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cwd = dume_store::resolve_project_root(&raw_cwd)
        .to_str()
        .unwrap_or(".")
        .to_string();

    let mut initial_conversation_id = dume_provider::types::StreamRequestContext::new().session_id;
    let mut initial_messages = Vec::new();
    let mut initial_model = model.to_string();
    let mut initial_provider_session_id =
        dume_provider::types::StreamRequestContext::new().session_id;
    let mut pending_interrupted_prompt = None;
    let mut initial_modal = ModalState::None;
    let mut resume_notice = None;

    match target {
        ResumeTarget::None => {
            let _ = harness_store.create_conversation(
                &initial_conversation_id,
                &cwd,
                &initial_model,
                &initial_provider_session_id,
            );
            resume_notice =
                detect_interrupted_session_notice(&harness_store, &cwd, &initial_conversation_id);
        }
        ResumeTarget::Latest => {
            if let Ok(Some(rec)) = harness_store.latest_conversation_for_cwd(&cwd) {
                match harness_store.load_conversation(&rec.id) {
                    Ok((loaded_rec, msg_recs)) => {
                        initial_conversation_id = loaded_rec.id.clone();
                        initial_model = loaded_rec.model.clone();
                        initial_provider_session_id = loaded_rec.provider_session_id.clone();
                        for mr in msg_recs {
                            if let Ok(chat_msg) =
                                serde_json::from_str::<ChatMessage>(&mr.message_json)
                            {
                                initial_messages.push(chat_msg);
                            }
                        }
                        let (was_repaired, prompt) =
                            repair_conversation_history(&mut initial_messages);
                        pending_interrupted_prompt = prompt;
                        let short_id = if loaded_rec.id.len() >= 8 {
                            &loaded_rec.id[..8]
                        } else {
                            &loaded_rec.id
                        };
                        resume_notice = Some(if was_repaired {
                            format!(
                                "Resumed conversation {} (recovered from interrupted state).",
                                short_id
                            )
                        } else {
                            format!("Resumed conversation {}.", short_id)
                        });
                    }
                    Err(e) => {
                        let _ = harness_store.create_conversation(
                            &initial_conversation_id,
                            &cwd,
                            &initial_model,
                            &initial_provider_session_id,
                        );
                        resume_notice = Some(format!(
                            "Failed to load previous conversation: {}. Started new session.",
                            e
                        ));
                    }
                }
            } else {
                let _ = harness_store.create_conversation(
                    &initial_conversation_id,
                    &cwd,
                    &initial_model,
                    &initial_provider_session_id,
                );
                resume_notice = Some(
                    "No previous conversation found in this directory. Started new session."
                        .to_string(),
                );
            }
        }
        ResumeTarget::Id(target_id) => {
            let found = harness_store
                .load_conversation(&target_id)
                .ok()
                .or_else(|| {
                    let list = harness_store.list_conversations(None, 50).ok()?;
                    let matched = list.into_iter().find(|c| c.id.starts_with(&target_id))?;
                    harness_store.load_conversation(&matched.id).ok()
                });
            match found {
                Some((loaded_rec, msg_recs)) => {
                    initial_conversation_id = loaded_rec.id.clone();
                    initial_model = loaded_rec.model.clone();
                    initial_provider_session_id = loaded_rec.provider_session_id.clone();
                    for mr in msg_recs {
                        if let Ok(chat_msg) = serde_json::from_str::<ChatMessage>(&mr.message_json)
                        {
                            initial_messages.push(chat_msg);
                        }
                    }
                    let (was_repaired, prompt) = repair_conversation_history(&mut initial_messages);
                    pending_interrupted_prompt = prompt;
                    let short_id = if loaded_rec.id.len() >= 8 {
                        &loaded_rec.id[..8]
                    } else {
                        &loaded_rec.id
                    };
                    resume_notice = Some(if was_repaired {
                        format!(
                            "Resumed conversation {} (recovered from interrupted state).",
                            short_id
                        )
                    } else {
                        format!("Resumed conversation {}.", short_id)
                    });
                }
                None => {
                    let _ = harness_store.create_conversation(
                        &initial_conversation_id,
                        &cwd,
                        &initial_model,
                        &initial_provider_session_id,
                    );
                    resume_notice = Some(format!(
                        "Conversation '{}' not found. Started new session.",
                        target_id
                    ));
                }
            }
        }
        ResumeTarget::Picker { all_dirs } => {
            let _ = harness_store.create_conversation(
                &initial_conversation_id,
                &cwd,
                &initial_model,
                &initial_provider_session_id,
            );
            let query_cwd = if all_dirs { None } else { Some(cwd.as_str()) };
            let sessions = harness_store
                .list_recent_conversations(query_cwd, 20)
                .unwrap_or_default();
            initial_modal = ModalState::ResumePicker {
                sessions,
                selected_idx: 0,
            };
        }
    }

    let mut conversation_lock = match ConversationLock::try_acquire(&initial_conversation_id) {
        Ok(l) => l,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to acquire lock for conversation '{}': {}",
                initial_conversation_id,
                e
            ));
        }
    };

    let mut app = App::new(&initial_model);
    app.conversation_id = initial_conversation_id.clone();
    app.session_id = initial_provider_session_id;
    app.messages = initial_messages;
    match harness_store.conversation_runtime_metrics(&app.conversation_id) {
        Ok(metrics) => {
            let primary: Vec<_> = metrics
                .runs
                .iter()
                .filter(|run| matches!(&run.scope, RuntimeScope::Conversation(_)))
                .collect();
            app.session_elapsed_ms = primary.iter().map(|run| run.elapsed_ms).sum();
            app.session_timing_complete =
                !primary.is_empty() && primary.iter().all(|run| run.timing_complete);
            app.last_run_metrics = primary.last().map(|run| dume_store::RunMetrics {
                run_id: run.run_id.clone(),
                status: run.status.clone(),
                elapsed_ms: run.elapsed_ms,
                timing_complete: run.timing_complete,
            });
        }
        Err(error) => app.messages.push(ChatMessage::ui_notice(format!(
            "Could not load session timing: {error}"
        ))),
    }
    let _ = refresh_session_timing(&mut app, &harness_store);
    app.modal = initial_modal;
    if let Some(prompt) = pending_interrupted_prompt {
        app.input_buffer = prompt;
        app.cursor_pos = app.input_buffer.chars().count();
    }
    if let Some(notice) = resume_notice {
        app.messages.push(ChatMessage::ui_notice(notice));
    }

    let (persist_tx, mut persist_rx) = mpsc::unbounded_channel::<PersistEvent>();
    let store_for_persist = harness_store.clone();
    let persist_handle = tokio::spawn(async move {
        while let Some(event) = persist_rx.recv().await {
            match event {
                PersistEvent::Replace { conv_id, msgs } => {
                    let json_vec: Vec<String> = msgs
                        .iter()
                        .filter_map(|m| serde_json::to_string(m).ok())
                        .collect();
                    let _ = store_for_persist.replace_conversation_messages(&conv_id, &json_vec);
                }
                PersistEvent::Status { conv_id, status } => {
                    let _ = store_for_persist.set_conversation_status(&conv_id, &status);
                }
            }
        }
    });

    let mut raw_dispatcher =
        ToolDispatcher::new(std::env::current_dir()?, &app.model, cancellation.clone());
    raw_dispatcher = raw_dispatcher.with_store(harness_store.clone());
    let dispatcher = Arc::new(Mutex::new(raw_dispatcher));
    let mut task = None;
    let mut routing_probe_tasks = tokio::task::JoinSet::new();
    let mut btw_tasks: Vec<(CancellationToken, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut current_stream_cancel: Option<CancellationToken> = None;
    let mut anim_tick: usize = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut screen_recovery = ScreenRecovery::new();
    let mut status_reporter = ProgramStatusReporter::new(io::stdout().is_terminal());
    let mut selection = TextSelection::default();

    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

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
        let _ = catalog_tx
            .send(ConversationEvent::CatalogRefreshFinished(result))
            .await;
    });
    spawn_codex_catalog_refresh(stream_tx.clone());
    spawn_opencodex_catalog_refresh(stream_tx.clone(), app.model.starts_with("opencodex/"));

    let reader_task = tokio::spawn(read_events(
        crossterm::event::EventStream::new(),
        event_tx,
        cancellation.child_token(),
    ));

    let result: Result<()> = async {
      'app_loop: loop {
        screen_recovery.draw(terminal, |f| {
            let line_count = app.input_buffer.split('\n').count().max(1);
            let input_height = (line_count as u16 + 2)
                .min(10)
                .min(f.area().height.saturating_sub(4).max(3));
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(0),           // Transcript area
                    Constraint::Length(input_height), // Input bar
                    Constraint::Length(1),        // Status bar
                ])
                .split(f.area());

            let transcript_area = chunks[0];
            let total_lines = calculate_transcript_height(
                transcript_area.width,
                &app.messages,
                &app.streaming_text,
                app.recent_sessions.len(),
                Some(&app.transcript_config()),
            );
            let max_scroll = total_lines.saturating_sub(transcript_area.height);
            if app.auto_scroll {
                app.scroll_offset = max_scroll;
            } else if app.scroll_offset >= max_scroll {
                app.scroll_offset = max_scroll;
                app.auto_scroll = !selection.is_dragging();
            }

            render_transcript(
                f,
                transcript_area,
                &app.messages,
                &app.streaming_text,
                app.scroll_offset,
                &app.recent_sessions,
                &app.theme,
                Some(&app.transcript_config()),
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
            let last_run = app.last_run_metrics.as_ref().map(|m| {
                let d = format_duration(m.elapsed_ms);
                let outcome = status_label(&m.status);
                if m.timing_complete { format!("{outcome} {d}") } else { format!("{outcome} 최소 {d}") }
            });
            let session_elapsed = if app.session_timing_complete {
                Some(format_duration(app.session_elapsed_ms))
            } else if app.session_elapsed_ms > 0 {
                Some(format!("최소 {}", format_duration(app.session_elapsed_ms)))
            } else { None };
            let session_usage = format!("TOKENS {}{}", app.session_usage.2, if app.session_usage_incomplete > 0 { format!(" +{} incomplete", app.session_usage_incomplete) } else { String::new() });
            render_status_bar(
                f,
                chunks[2],
                app.status_model(),
                app.status_thinking(),
                app.is_busy,
                elapsed_secs,
                last_run.as_deref(),
                session_elapsed.as_deref(),
                Some(&session_usage),
                anim_tick,
                update_str,
                &app.theme,
            );

            // Render active modal on top
            match &app.modal {
                ModalState::None => {}
                ModalState::RoutingSettings(draft) => {
                    render_routing_settings_modal(f, f.area(), draft, &app.theme);
                }
                ModalState::RoutingProbeConfirm(draft) => {
                    render_routing_probe_confirm_modal(f, f.area(), draft.probe_confirmation, &app.theme);
                }
                ModalState::ModelSelector {
                    target,
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
                        app.picker_thinking(),
                        &app.model,
                        target,
                        &app.theme,
                    );
                }
                ModalState::LoginSelector { selected_idx } => {
                    let cred_store = &app.auth_store;

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
                        LoginProviderChoice {
                            id: "opencodex",
                            name: "OpenCodex Local",
                            auth_type: "API Key (optional)",
                            is_authenticated: cred_store.has_credential("opencodex"),
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
                ModalState::LogoutSelector { selected_idx } => {
                    let cred_store = &app.auth_store;

                    let choices = [
                        LoginProviderChoice {
                            id: "anthropic",
                            name: "Anthropic Claude",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("anthropic"),
                        },
                        LoginProviderChoice {
                            id: "openai",
                            name: "OpenAI ChatGPT",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("openai"),
                        },
                        LoginProviderChoice {
                            id: "google",
                            name: "Google Gemini",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("google"),
                        },
                        LoginProviderChoice {
                            id: "deepseek",
                            name: "DeepSeek",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("deepseek"),
                        },
                        LoginProviderChoice {
                            id: "openai-codex",
                            name: "OpenAI Codex",
                            auth_type: "OAuth Browser",
                            is_authenticated: cred_store.has_credential("openai-codex"),
                        },
                        LoginProviderChoice {
                            id: "opencode",
                            name: "OpenCode Zen",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("opencode"),
                        },
                        LoginProviderChoice {
                            id: "opencode-go",
                            name: "OpenCode Go",
                            auth_type: "API Key",
                            is_authenticated: cred_store.has_credential("opencode-go"),
                        },
                        LoginProviderChoice {
                            id: "opencodex",
                            name: "OpenCodex Local",
                            auth_type: "API Key (optional)",
                            is_authenticated: cred_store.has_credential("opencodex"),
                        },
                    ];

                    render_logout_selector_modal(
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
                ModalState::ResumePicker { sessions, selected_idx } => {
                    render_resume_picker_modal(
                        f,
                        f.area(),
                        sessions,
                        *selected_idx,
                        &app.theme,
                    );
                }
            }
            if matches!(app.modal, ModalState::None) && autocomplete_items.is_empty() {
                selection.update_viewport(
                    f.buffer_mut(),
                    transcript_area,
                    chunks[1].inner(ratatui::layout::Margin::new(1, 1)),
                    app.scroll_offset,
                );
                if selection.selects_transcript() {
                    let transcript_cfg = app.transcript_config();
                    let (lines, _) = build_transcript_lines(
                        &app.messages,
                        &app.streaming_text,
                        &app.recent_sessions,
                        transcript_area.width as usize,
                        &app.theme,
                        Some(&transcript_cfg),
                    );
                    selection.refresh_transcript(lines);
                }
                selection.highlight(
                    f.buffer_mut(),
                    ratatui::style::Style::default()
                        .bg(app.theme.accent)
                        .fg(ratatui::style::Color::Black),
                );
            } else {
                selection.clear();
                selection.update_viewport(f.buffer_mut(), Default::default(), Default::default(), 0);
            }
        })?;

        tokio::select! {
            completed_probe = routing_probe_tasks.join_next(), if !routing_probe_tasks.is_empty() => {
                if let Some(Ok((probe_id, message))) = completed_probe {
                    app.receive(ConversationEvent::RoutingProbeFinished { probe_id, message });
                }
            }
            event = event_rx.recv() => {
                let event = event.context("terminal input reader stopped")?;
                let mut events = vec![event];
                while let Ok(next) = event_rx.try_recv() {
                    events.push(next);
                }
                for event in events {
                    let event = event.context("reading terminal input")?;
                    match event {
                    Event::Key(key) => {
                            if key.kind == crossterm::event::KeyEventKind::Release {
                                continue;
                            }
                            selection.clear();
                            let mut selected_login = None;
                            let mut selected_logout = None;
                            // 1. Handle Modal Events if modal is active
                            match &mut app.modal {
                                ModalState::ModelSelector {
                                    target,
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
                                        _ if matches!(keybindings.action(key), Action::Tab) => {
                                            if let Some(model) = selected_idx.checked_sub(target.model_offset()).and_then(|index| filtered.get(index)).cloned() {
                                                app.cycle_thinking(model);
                                            }
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
                                            let count = filtered.len() + target.model_offset();
                                            if count > 0 {
                                                if *selected_idx == 0 {
                                                    *selected_idx = count - 1;
                                                } else {
                                                    *selected_idx -= 1;
                                                }
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            let count = filtered.len() + target.model_offset();
                                            if count > 0 {
                                                if *selected_idx + 1 >= count {
                                                    *selected_idx = 0;
                                                } else {
                                                    *selected_idx += 1;
                                                }
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            if let Err(error) = app.confirm_model_selection() {
                                                app.messages.push(ChatMessage::ui_notice(format!("Subagent settings error: {error:#}")));
                                            }
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
                                        crossterm::event::KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Char('u') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                            filter.clear();
                                            let tab = &provider_tabs[*active_tab_idx];
                                            *filtered = models
                                                .iter()
                                                .filter(|m| {
                                                    tab == "All" || m.provider.eq_ignore_ascii_case(tab)
                                                })
                                                .cloned()
                                                .collect();
                                            *selected_idx = 0;
                                        }
                                        crossterm::event::KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT => {
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
                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            if *selected_idx == 0 {
                                                *selected_idx = 7;
                                            } else {
                                                *selected_idx -= 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            if *selected_idx + 1 >= 8 {
                                                *selected_idx = 0;
                                            } else {
                                                *selected_idx += 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            selected_login = Some(*selected_idx);
                                        }
                                        _ => {}
                                    }
                                    if selected_login.is_none() {
                                        continue;
                                    }
                                }
                                ModalState::LogoutSelector { selected_idx } => {
                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            if *selected_idx == 0 {
                                                *selected_idx = 7;
                                            } else {
                                                *selected_idx -= 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            if *selected_idx + 1 >= 8 {
                                                *selected_idx = 0;
                                            } else {
                                                *selected_idx += 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            selected_logout = Some(*selected_idx);
                                        }
                                        _ => {}
                                    }
                                    if selected_logout.is_none() {
                                        continue;
                                    }
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
                                                let cred_store = app.auth_store.clone();
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
                                                app.modal = ModalState::None;
                                            } else {
                                                app.messages.push(ChatMessage::ui_notice(format!(
                                                    "API key cannot be empty for {}. Please enter or paste a valid API key.",
                                                    provider_name
                                                )));
                                            }
                                        }
                                        crossterm::event::KeyCode::Char('v') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                            if let Some(clip) = crate::selection::get_clipboard_text() {
                                                let clean = normalize_pasted_text(&clip);
                                                input.push_str(clean.trim());
                                            }
                                        }
                                        crossterm::event::KeyCode::Char('u') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                            input.clear();
                                        }
                                        crossterm::event::KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT => {
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
                                            let store = harness_store.clone();
                                            let parent_id = app.conversation_id.clone();
                                            let cwd = cwd.clone();
                                            let cancellation = cancellation.child_token();
                                            let task_cancel = cancellation.clone();

                                            let handle = tokio::spawn(async move {
                                                let result = run_btw_request(
                                                    &model_name, &cwd, &parent_id, btw_history, store,
                                                    cancellation, tx.clone(),
                                                    |model, messages, context, events| async move {
                                                        let provider = dume_provider::runtime::resolve_provider(
                                                            &model,
                                                            &dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path()),
                                                        ).await?;
                                                        provider.stream_with_context(&messages, &[], Some(&context), events).await
                                                    },
                                                ).await;
                                                let _ = tx.send(ConversationEvent::BtwFinished(result)).await;
                                            });
                                            btw_tasks.push((task_cancel, handle));
                                        }
                                    } else if key.code == crossterm::event::KeyCode::Char('u') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
                                        input.clear();
                                    } else if key.code == crossterm::event::KeyCode::Char('c') && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
                                        app.modal = ModalState::None;
                                    } else if let crossterm::event::KeyCode::Char(c) = key.code {
                                        if key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT {
                                            input.push(c);
                                        }
                                    }
                                    continue;
                                }
                                ModalState::ResumePicker { sessions, selected_idx } => {
                                    match key.code {
                                        crossterm::event::KeyCode::Esc => {
                                            app.modal = ModalState::None;
                                        }
                                        crossterm::event::KeyCode::Up => {
                                            if *selected_idx == 0 {
                                                if !sessions.is_empty() {
                                                    *selected_idx = sessions.len() - 1;
                                                }
                                            } else {
                                                *selected_idx -= 1;
                                            }
                                        }
                                        crossterm::event::KeyCode::Down => {
                                            if !sessions.is_empty() {
                                                if *selected_idx + 1 >= sessions.len() {
                                                    *selected_idx = 0;
                                                } else {
                                                    *selected_idx += 1;
                                                }
                                            }
                                        }
                                        crossterm::event::KeyCode::Enter => {
                                            if app.is_busy {
                                                app.messages.push(ChatMessage::ui_notice("Cannot switch conversations while a run is active; the picker remains open."));
                                                continue;
                                            }
                                            let target_id = sessions.get(*selected_idx).map(|s| s.session_id.clone());
                                            app.modal = ModalState::None;
                                            if let Some(session_id) = target_id {
                                                match ConversationLock::try_acquire(&session_id) {
                                                    Ok(new_lock) => {
                                                        match harness_store.load_conversation(&session_id) {
                                                            Ok((loaded_rec, msg_recs)) => {
                                                                conversation_lock = new_lock;
                                                                app.conversation_id = loaded_rec.id.clone();
                                                                let _ = refresh_session_timing(&mut app, &harness_store);
                                                                app.model = loaded_rec.model.clone();
                                                                app.restore_thinking();
                                                                app.session_id = loaded_rec.provider_session_id.clone();
                                                                app.messages.clear();
                                                                for mr in msg_recs {
                                                                    if let Ok(chat_msg) = serde_json::from_str::<ChatMessage>(&mr.message_json) {
                                                                        app.messages.push(chat_msg);
                                                                    }
                                                                }
                                                                let (was_repaired, prompt) = repair_conversation_history(&mut app.messages);
                                                                if let Some(p) = prompt {
                                                                    app.input_buffer = p;
                                                                    app.cursor_pos = app.input_buffer.chars().count();
                                                                }
                                                                let short_id = if loaded_rec.id.len() >= 8 { &loaded_rec.id[..8] } else { &loaded_rec.id };
                                                                app.messages.push(ChatMessage::ui_notice(if was_repaired {
                                                                    format!("Switched to conversation {} (repaired interrupted state).", short_id)
                                                                } else {
                                                                    format!("Switched to conversation {}.", short_id)
                                                                }));
                                                            }
                                                            Err(e) => {
                                                                app.messages.push(ChatMessage::ui_notice(format!("Failed to load conversation: {}", e)));
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!("Cannot resume conversation: {}", e)));
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }
                                ModalState::RoutingSettings(draft) => {
                                    if matches!(keybindings.action(key), Action::Escape | Action::Quit) {
                                        draft.cancel_probe();
                                        app.modal = ModalState::None;
                                        continue;
                                    }
                                    if draft.picker_role.is_some() {
                                        match keybindings.action(key) {
                                            Action::Escape | Action::Quit => {
                                                draft.picker_role = None;
                                                draft.picker_models = None;
                                            }
                                            Action::ScrollUp => draft.picker_selected = draft.picker_selected.saturating_sub(1),
                                            Action::ScrollDown => draft.picker_selected = draft.picker_selected.saturating_add(1).min(draft.picker_models.as_ref().map_or(0, Vec::len)),
                                            Action::SubmitInput => {
                                                if let (Some(role), Some(models)) = (draft.picker_role.take(), draft.picker_models.take()) {
                                                    let selected = draft.picker_selected.checked_sub(1).and_then(|index| models.get(index));
                                                    routing_set_role_model(draft, role, selected);
                                                }
                                            }
                                            _ => {}
                                        }
                                        continue;
                                    }
                                    match keybindings.action(key) {
                                        Action::ScrollUp => { draft.field_edit = false; draft.selected = draft.selected.saturating_sub(1); }
                                        Action::ScrollDown => { draft.field_edit = false; draft.selected = (draft.selected + 1).min(12); }
                                        Action::CursorLeft if draft.field_edit => routing_edit_left(draft),
                                        Action::CursorRight if draft.field_edit => routing_edit_right(draft),
                                        Action::CursorHome if draft.field_edit => draft.cursor = 0,
                                        Action::CursorEnd if draft.field_edit => draft.cursor = routing_selected_text(draft).chars().count(),
                                        Action::DeleteChar if draft.field_edit => routing_delete_char(draft),
                                        Action::DeleteForward if draft.field_edit => routing_delete_forward(draft),
                                        Action::DeleteToBeginning if draft.field_edit => routing_delete_to_beginning(draft),
                                        Action::InsertChar(c) if draft.field_edit => routing_insert_char(draft, c),
                                        Action::Tab => { draft.field_edit = false; draft.selected = (draft.selected + 1) % 13; }
                                        Action::SubmitInput => {
                                            match draft.selected {
                                                0 => draft.settings.enabled = !draft.settings.enabled,
                                                1 => {
                                                    draft.cancel_probe_for_config_edit();
                                                    let current = draft.settings.classifier.clone().unwrap_or(dume_provider::ClassifierConfig { backend: dume_provider::ClassifierBackend::OpenCodeZen, base_url: String::new(), model: String::new() });
                                                    let backend = if current.backend == dume_provider::ClassifierBackend::OpenCodeZen { dume_provider::ClassifierBackend::LocalSystemOne } else { dume_provider::ClassifierBackend::OpenCodeZen };
                                                    draft.settings.classifier = Some(dume_provider::ClassifierConfig { backend, ..current });
                                                    draft.key_input.clear();
                                                    draft.cursor = 0;
                                                    draft.refresh_key_origin();
                                                }
                                                2 | 3 => {
                                                    routing_begin_field_edit(draft);
                                                }
                                                4..=6 => routing_open_role_picker(draft),
                                                7 => { draft.cursor = draft.key_input.chars().count(); draft.field_edit = true; }
                                                8 => {
                                                    draft.cancel_probe_for_config_edit();
                                                    let provider = draft.settings.classifier.as_ref().map(|c| c.backend.credential_provider());
                                                    if let (Some(provider), Ok(path)) = (provider, dume_worker::RoutingSettings::global_path()) {
                                                        let store = dume_provider::CredentialStore::new(path.with_file_name("auth.json"));
                                                        match store.save_credential(provider, &draft.key_input) {
                                                            Ok(()) => { draft.key_input.clear(); draft.refresh_key_origin(); draft.message = "분류기 인증 키를 저장했습니다.".into(); }
                                                            Err(_) => draft.message = "분류기 인증 키를 저장할 수 없습니다.".into(),
                                                        }
                                                    } else { draft.message = "전역 인증 경로를 확인할 수 없습니다.".into(); }
                                                }
                                                9 => {
                                                    draft.cancel_probe_for_config_edit();
                                                    let provider = draft.settings.classifier.as_ref().map(|c| c.backend.credential_provider());
                                                    if let (Some(provider), Ok(path)) = (provider, dume_worker::RoutingSettings::global_path()) {
                                                        let store = dume_provider::CredentialStore::new(path.with_file_name("auth.json"));
                                                        match store.delete(provider) {
                                                            Ok(()) => draft.refresh_key_origin(),
                                                            Err(_) => draft.message = "저장된 분류기 키를 삭제할 수 없습니다.".into(),
                                                        }
                                                    } else { draft.message = "전역 인증 경로를 확인할 수 없습니다.".into(); }
                                                }
                                                10 => {
                                                    match draft.settings.validate() {
                                                        Err(_) => draft.message = "설정 값을 확인하세요: 주소나 분류 모델이 올바르지 않습니다.".into(),
                                                        Ok(()) => match draft.settings.save_default() {
                                                            Ok(()) => draft.message = "설정을 저장했습니다.".into(),
                                                            Err(_) => draft.message = "전역 설정 경로가 안전하지 않거나 저장할 수 없습니다.".into(),
                                                        },
                                                    }
                                                }
                                                11 => {
                                                    if draft.probe_task.is_some() { draft.message = "연결 확인이 이미 진행 중입니다.".into(); }
                                                    else {
                                                        draft.probe_confirmation = false;
                                                        app.modal = ModalState::RoutingProbeConfirm(std::mem::replace(draft, empty_routing_draft()));
                                                    }
                                                }
                                                12 => { draft.cancel_probe(); app.modal = ModalState::None; },
                                                _ => {}
                                            }
                                        }
                                        Action::SubmitFollowUp => {
                                            if draft.selected == 4 { draft.settings.models.standard = None; }
                                            else if draft.selected == 5 { draft.settings.models.complex = None; }
                                            else if draft.selected == 6 { draft.settings.models.implementation = None; }
                                            else if draft.selected == 9 {
                                                draft.cancel_probe_for_config_edit();
                                                let provider = draft.settings.classifier.as_ref().map(|c| c.backend.credential_provider());
                                                if let (Some(provider), Ok(path)) = (provider, dume_worker::RoutingSettings::global_path()) {
                                                    let store = dume_provider::CredentialStore::new(path.parent().unwrap_or_else(|| std::path::Path::new(".")).join("auth.json"));
                                                    match store.delete(provider) {
                                                        Ok(()) => draft.refresh_key_origin(),
                                                        Err(_) => draft.message = "저장 키를 삭제할 수 없습니다.".into(),
                                                    }
                                                } else { draft.message = "전역 인증 경로를 확인할 수 없습니다.".into(); }
                                            }
                                        }
                                        Action::Clear => if draft.selected == 7 { draft.key_input.clear(); draft.cursor = 0; },
                                        Action::PageUp => { draft.field_edit = false; draft.selected = 0; },
                                        Action::PageDown => { draft.field_edit = false; draft.selected = 12; },
                                        Action::Escape => { draft.cancel_probe(); app.modal = ModalState::None; },
                                        _ => {}
                                    }
                                    continue;
                                }
                                ModalState::RoutingProbeConfirm(draft) => {
                                    match keybindings.action(key) {
                                    Action::CursorLeft | Action::ScrollUp => draft.probe_confirmation = false,
                                    Action::CursorRight | Action::ScrollDown | Action::Tab => draft.probe_confirmation = true,
                                    Action::SubmitInput if !draft.probe_confirmation => {
                                        draft.probe_confirmation = false;
                                        app.modal = ModalState::RoutingSettings(std::mem::replace(draft, empty_routing_draft()));
                                    }
                                    Action::SubmitInput => {
                                        draft.message = "확인 중입니다…".into();
                                        let token = CancellationToken::new();
                                        draft.probe_cancel = Some(token.clone());
                                        let config = draft.settings.classifier.clone();
                                        let global_auth_path = dume_worker::RoutingSettings::global_path().ok().map(|path| path.with_file_name("auth.json"));
                                        if let (Some(config), Some(auth_path)) = (config, global_auth_path) {
                                            let credentials = dume_provider::CredentialStore::new(auth_path);
                                            spawn_routing_probe(&mut routing_probe_tasks, draft, config, credentials, token);
                                        } else {
                                            draft.message = "HOME 경로 오류 · 인증 정보를 읽지 않았습니다.".into();
                                        }
                                        app.modal = ModalState::RoutingSettings(std::mem::replace(draft, empty_routing_draft()));
                                    }
                                    Action::Escape | Action::Quit => {
                                        draft.cancel_probe();
                                        app.modal = ModalState::RoutingSettings(std::mem::replace(draft, empty_routing_draft()));
                                    }
                                    _ => {}
                                    }
                                    continue;
                                }
                                ModalState::None => {}
                            }

                            if let Some(idx) = selected_login {
                                select_login_provider(&mut app, idx, &stream_tx);
                                continue;
                            }
                            if let Some(idx) = selected_logout {
                                select_logout_provider(&mut app, idx);
                                continue;
                            }

                            // 2. Normal Editor and Transcript Handling
                            let autocomplete_items = app.get_autocomplete_items();
                            let is_autocomplete_active = !autocomplete_items.is_empty();

                            match keybindings.action(key) {
                                Action::OpenRoutingSettings => {
                                    let (settings, message) = match dume_worker::RoutingSettings::load_default() {
                                        Ok(settings) => (settings, "↑/↓ 항목 · Enter 선택/편집 · Esc 닫기".to_string()),
                                        Err(_) => (dume_worker::RoutingSettings::default(), "설정을 읽지 못해 기본값으로 열었습니다.".to_string()),
                                    };
                                    let mut draft = RoutingDraft::new(settings, "인증 정보 없음".into());
                                    draft.message = message;
                                    draft.refresh_key_origin();
                                    app.modal = ModalState::RoutingSettings(draft);
                                }
                                Action::Quit => {
                                    // 1. If currently busy streaming, first Ctrl+C cancels the active turn
                                    if app.is_busy {
                                        if let Some(cancel) = current_stream_cancel.take() {
                                            cancel.cancel();
                                        }
                                        app.messages.push(ChatMessage::ui_notice("Streaming interrupted by user."));
                                        app.is_busy = false;
                                        app.streaming_text.clear();
                                        app.active_tools.clear();
                                        app.streaming_tools.clear();
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
                                            break 'app_loop;
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
                                        app.active_tools.clear();
                                        app.streaming_tools.clear();
                                    }
                                    app.last_ctrl_c = None;
                                }
                                Action::Clear if !app.is_busy => {
                                    app.messages.clear();
                                    app.streaming_text.clear();
                                    app.active_tools.clear();
                                    app.streaming_tools.clear();
                                    app.selected_tool_id = None;
                                    app.expanded_tools.clear();
                                    app.auto_scroll = true;
                                    app.scroll_offset = 0;
                                    app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                }
                                Action::Clear => {}
                                Action::ToggleToolDetails => {
                                    app.toggle_selected_tool_details();
                                }
                                Action::SelectPrevTool => {
                                    app.select_prev_tool();
                                }
                                Action::SelectNextTool => {
                                    app.select_next_tool();
                                }
                                Action::InsertChar(c) => {
                                    app.autocomplete_dismissed = false;
                                    app.insert_char(c);
                                    app.autocomplete_index = 0;
                                }
                                Action::InsertNewline => {
                                    app.autocomplete_dismissed = false;
                                    app.insert_char('\n');
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteChar => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_char();
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteForward => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_forward();
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteToBeginning => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_to_beginning();
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteToEnd => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_to_end();
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteWordBackward => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_word_backward();
                                    app.autocomplete_index = 0;
                                }
                                Action::DeleteWordForward => {
                                    app.autocomplete_dismissed = false;
                                    app.delete_word_forward();
                                    app.autocomplete_index = 0;
                                }
                                Action::CursorLeft => app.cursor_left(),
                                Action::CursorRight => app.cursor_right(),
                                Action::CursorWordLeft => app.cursor_word_left(),
                                Action::CursorWordRight => app.cursor_word_right(),
                                Action::CursorHome => app.cursor_pos = 0,
                                Action::CursorEnd => app.cursor_pos = app.input_buffer.chars().count(),
                                Action::ScrollUp => {
                                    if is_autocomplete_active {
                                        if app.autocomplete_index == 0 {
                                            app.autocomplete_index = autocomplete_items.len().saturating_sub(1);
                                        } else {
                                            app.autocomplete_index -= 1;
                                        }
                                    } else if app.cursor_up() {
                                        // Moved cursor up in multiline input buffer
                                    } else if !app.prompt_history.is_empty() {
                                        app.navigate_history_up();
                                    }
                                }
                                Action::ScrollDown => {
                                    if is_autocomplete_active {
                                        if app.autocomplete_index + 1 >= autocomplete_items.len() {
                                            app.autocomplete_index = 0;
                                        } else {
                                            app.autocomplete_index += 1;
                                        }
                                    } else if app.cursor_down() {
                                        // Moved cursor down in multiline input buffer
                                    } else if app.history_index.is_some() {
                                        app.navigate_history_down();
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
                                        match dume_provider::resolve_model(&app.model) {
                                            Ok(model) => app.cycle_thinking(model),
                                            Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Could not change thinking: {error:#}"))),
                                        }
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

                                    if app.is_busy && !app.input_buffer.trim().is_empty() {
                                        let draft = app.input_buffer.trim();
                                        let (kind, content) = draft.strip_prefix("/follow-up ")
                                            .map(|content| (InputKind::FollowUp, content.trim()))
                                            .unwrap_or((InputKind::Steer, draft));
                                        if content.is_empty() { continue; }
                                        let request_id = dume_provider::types::StreamRequestContext::new().request_id;
                                        match harness_store.admit_input(
                                            RuntimeScope::Conversation(app.conversation_id.clone()),
                                            &request_id,
                                            kind,
                                            &content,
                                        ) {
                                            Ok(_) => {
                                                app.input_buffer.clear();
                                                app.cursor_pos = 0;
                                            }
                                            Err(error) => app.messages.push(ChatMessage::ui_notice(
                                                format!("Could not queue steer: {error}"),
                                            )),
                                        }
                                        continue;
                                    }

                                    if !app.input_buffer.trim().is_empty() && !app.is_busy {
                                        let content = std::mem::take(&mut app.input_buffer);
                                        app.cursor_pos = 0;
                                        app.auto_scroll = true;
                                        app.autocomplete_dismissed = false;
                                        app.autocomplete_index = 0;

                                        let trimmed = content.trim();
                                        if trimmed == "/follow-up" || trimmed.starts_with("/follow-up ") {
                                            let follow_up = trimmed.strip_prefix("/follow-up").unwrap_or("").trim();
                                            if follow_up.is_empty() {
                                                app.input_buffer = content;
                                                app.cursor_pos = app.input_buffer.chars().count();
                                                continue;
                                            }
                                            let request_id = dume_provider::types::StreamRequestContext::new().request_id;
                                            match harness_store.admit_input(
                                                RuntimeScope::Conversation(app.conversation_id.clone()),
                                                &request_id,
                                                InputKind::FollowUp,
                                                follow_up,
                                            ) {
                                                Ok(_) => app.messages.push(ChatMessage::ui_notice("Follow-up queued.")),
                                                Err(error) => {
                                                    app.input_buffer = content;
                                                    app.cursor_pos = app.input_buffer.chars().count();
                                                    app.messages.push(ChatMessage::ui_notice(format!("Could not queue follow-up: {error}")));
                                                }
                                            }
                                            continue;
                                        } else if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
                                            let operation = trimmed.strip_prefix("/mcp").unwrap_or("").trim();
                                            let mut active_dispatcher = dispatcher.lock().await;
                                            let result = match operation {
                                                "" | "list" | "status" => {
                                                    let status = active_dispatcher.mcp_status();
                                                    if status.is_empty() { "MCP is not initialized for an admitted conversation run.".to_string() } else { status }
                                                }
                                                "refresh" => match active_dispatcher.refresh_mcp().await {
                                                    Ok(()) => active_dispatcher.mcp_status(),
                                                    Err(error) => format!("MCP refresh failed: {error:#}"),
                                                },
                                                _ => "Usage: /mcp [list|status|refresh]".to_string(),
                                            };
                                            app.messages.push(ChatMessage::ui_notice(result));
                                            continue;
                                        } else if trimmed == "/run resume" || trimmed.starts_with("/run resume ") {
                                            let run_id = trimmed.strip_prefix("/run resume").unwrap_or("").trim();
                                            let result = (|| -> Result<String> {
                                                if app.is_busy { anyhow::bail!("A conversation run is already active."); }
                                                let candidate = find_resumable_run(&harness_store, &app.conversation_id, run_id)?;
                                                let run_id = candidate.run_id;
                                                let history = harness_store.conversation_history(&app.conversation_id)?;
                                                let messages = history.iter().filter_map(|row| serde_json::from_str(&row.message_json).ok()).collect();
                                                app.begin_run(run_id.clone());
                                                app.is_busy = true;
                                                app.busy_started_at = Some(std::time::Instant::now());
                                                let model_name = app.model.clone();
                                                let session_id = app.session_id.clone();
                                                let run_cancel = cancellation.child_token();
                                                current_stream_cancel = Some(run_cancel.clone());
                                                task = Some(spawn_admitted_run(
                                                    model_name, session_id, messages, stream_tx.clone(), dispatcher.clone(),
                                                    run_cancel, harness_store.clone(), app.conversation_id.clone(), run_id.clone(), routing_snapshot.clone(),
                                                ));
                                                Ok(format!("Resuming paused run {run_id} without submitting new input."))
                                            })();
                                            app.messages.push(ChatMessage::ui_notice(match result {
                                                Ok(message) => message,
                                                Err(error) => format!("Cannot resume run: {error:#}"),
                                            }));
                                            continue;
                                        } else if trimmed.starts_with("/queue") {
                                            let args = trimmed.strip_prefix("/queue").unwrap_or("").trim();
                                            let scope = RuntimeScope::Conversation(app.conversation_id.clone());
                                            let mut resumed_receipt = None;
                                            let result = if args.is_empty() || args == "list" {
                                                harness_store.inbox(&scope, &["pending", "paused"])
                                                    .map(|rows| {
                                                        if rows.is_empty() { "Queue is empty.".to_string() }
                                                        else { rows.into_iter().map(|row| format!("#{} {:?} [{}] {}", row.input_id, row.kind, row.state, row.payload)).collect::<Vec<_>>().join("\n") }
                                                    })
                                            } else if let Some(id) = args.strip_prefix("withdraw ") {
                                                id.parse::<i64>().map_err(|_| dume_store::StoreError::InvalidSessionState("Usage: /queue withdraw <input-id>".into()))
                                                    .and_then(|id| harness_store.withdraw_input(&scope, id).map(|removed| if removed { format!("Withdrew queued input #{id}.") } else { format!("Input #{id} is no longer withdrawable.") }))
                                            } else if let Some(id) = args.strip_prefix("resume ") {
                                                id.parse::<i64>().map_err(|_| dume_store::StoreError::InvalidSessionState("Usage: /queue resume <input-id>".into()))
                                                    .and_then(|id| harness_store.reactivate_paused_input(&scope, id).map(|receipt| {
                                                        let message = format!("Reactivated input #{id} ({}) as {:?}.", receipt.disposition, receipt.run_id);
                                                        resumed_receipt = Some(receipt);
                                                        message
                                                    }))
                                            } else {
                                                Err(dume_store::StoreError::InvalidSessionState("Usage: /queue [list|withdraw <input-id>|resume <input-id>]".into()))
                                            };
                                            app.messages.push(ChatMessage::ui_notice(match result {
                                                Ok(message) => message,
                                                Err(error) => {
                                                    app.input_buffer = content.clone();
                                                    app.cursor_pos = app.input_buffer.chars().count();
                                                    format!("Queue operation failed: {error}")
                                                },
                                            }));
                                            if !app.is_busy {
                                                if let Some(run_id) = resumed_receipt.and_then(|receipt| receipt.run_id) {
                                                    let history = harness_store.conversation_history(&app.conversation_id)?;
                                                    let messages = history.iter().filter_map(|row| serde_json::from_str(&row.message_json).ok()).collect();
                                                    app.begin_run(run_id.clone());
                                                    app.is_busy = true;
                                                    app.busy_started_at = Some(std::time::Instant::now());
                                                    let model_name = app.model.clone();
                                                    let session_id = app.session_id.clone();
                                                    let tx = stream_tx.clone();
                                                    let run_cancel = cancellation.child_token();
                                                    current_stream_cancel = Some(run_cancel.clone());
                                                    let task_dispatcher = dispatcher.clone();
                                                    let store = harness_store.clone();
                                                    let conversation_id = app.conversation_id.clone();
                                                    task = Some(spawn_admitted_run(model_name, session_id, messages, tx, task_dispatcher, run_cancel, store, conversation_id, run_id, routing_snapshot.clone()));
                                                }
                                            }
                                            continue;
                                        } else if trimmed == "/routing" {
                                            let (settings, message) = match dume_worker::RoutingSettings::load_default() {
                                                Ok(settings) => (settings, "↑/↓ 항목 · Enter 선택/편집 · Esc 닫기".to_string()),
                                                Err(_) => (dume_worker::RoutingSettings::default(), "설정을 읽지 못해 기본값으로 열었습니다.".to_string()),
                                            };
                                            let mut draft = RoutingDraft::new(settings, "인증 정보 없음".into());
                                            draft.message = message;
                                            draft.refresh_key_origin();
                                            app.modal = ModalState::RoutingSettings(draft);
                                            continue;
                                        } else if trimmed == "/model" || trimmed == "/model " {
                                            let models = selectable_models();

                                            if models.is_empty() {
                                                app.messages.push(ChatMessage::ui_notice(
                                                    "No available provider models found. Start OpenCodex locally or use /login to authenticate another provider."
                                                ));
                                                continue;
                                            }

                                            app.modal = ModalState::model_selector(models, &app.model, ModelSelectorTarget::Conversation);
                                            continue;
                                        } else if trimmed.starts_with("/model ") {
                                            let new_model = trimmed[7..].trim().to_string();
                                            if let Err(error) = persist_model_to(&app.thinking_path, &new_model) {
                                                app.messages.push(ChatMessage::ui_notice(format!("Could not save model selection: {error:#}")));
                                                continue;
                                            }
                                            app.messages.push(ChatMessage::ui_notice(format!("Switched model to '{}'", new_model)));
                                            app.model = new_model;
                                            app.restore_thinking();
                                            app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                            continue;
                                        } else if trimmed == "/subagents" {
                                            match SubagentSettings::path().and_then(|path| ModalState::subagent_model_selector(selectable_models(), path)) {
                                                Ok(modal) => app.modal = modal,
                                                Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Subagent settings error: {error:#}"))),
                                            }
                                            continue;
                                        } else if trimmed.starts_with("/subagents ") {
                                            let result = SubagentSettings::path()
                                                .and_then(|path| subagent_command(trimmed, &path));
                                            let message = match result {
                                                Ok(message) => message,
                                                Err(error) => format!("Subagent settings error: {error:#}"),
                                            };
                                            app.messages.push(ChatMessage::ui_notice(message));
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
                                                let cred_store = app.auth_store.clone();
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
                                        } else if trimmed == "/logout" {
                                            handle_logout_command(&mut app, "");
                                            continue;
                                        } else if trimmed.starts_with("/logout ") {
                                            let rest = trimmed[8..].trim();
                                            handle_logout_command(&mut app, rest);
                                            continue;
                                        } else if trimmed == "/usage" {
                                            match harness_store.conversation_runtime_metrics(&app.conversation_id) {
                                                Ok(metrics) => {
                                                    let mut input = 0i64;
                                                    let mut output = 0i64;
                                                    let mut missing = 0usize;
                                                    for attempt in &metrics.model_attempts {
                                                        if let Some(usage) = &attempt.usage {
                                                            match (usage.get("input_tokens").and_then(serde_json::Value::as_i64), usage.get("output_tokens").and_then(serde_json::Value::as_i64)) {
                                                                (Some(i), Some(o)) => { input += i; output += o; }
                                                                _ => missing += 1,
                                                            }
                                                        } else { missing += 1; }
                                                    }
                                                    app.messages.push(ChatMessage::ui_notice(format!("Persisted model-attempt usage: input={input}, output={output}, total={}, missing attempts={missing}", input + output)));
                                                }
                                                Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Could not load usage metrics: {error}"))),
                                            }
                                            continue;
                                        } else if trimmed == "/session" {
                                            match harness_store.conversation_runtime_metrics(&app.conversation_id) {
                                                Ok(metrics) => {
                                                    let mut report = format_session_metrics(&metrics);
                                                    let auxiliary_id = format!("btw:{}", app.conversation_id);
                                                    if let Ok(auxiliary) = harness_store.conversation_runtime_metrics(&auxiliary_id) {
                                                        report.push_str("\n\nAuxiliary /btw usage (separate from primary session):");
                                                        report.push_str(&format_session_metrics(&auxiliary));
                                                    }
                                                    app.messages.push(ChatMessage::ui_notice(report));
                                                }
                                                Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Could not load session metrics: {error}"))),
                                            }
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
                                            let scope = RuntimeScope::Conversation(app.conversation_id.clone());
                                            let active = harness_store.active_run(&scope)?;
                                            let needs_attention = harness_store.conversation_runtime_metrics(&app.conversation_id)?
                                                .runs.iter().any(|run| run.status == "needs_attention");
                                            if app.is_busy || active.is_some() || needs_attention {
                                                app.input_buffer = content;
                                                app.cursor_pos = app.input_buffer.chars().count();
                                                app.messages.push(ChatMessage::ui_notice("Cannot clear while a run is active or needs attention."));
                                                continue;
                                            }
                                            if let Err(error) = harness_store.replace_conversation_messages(&app.conversation_id, &[]) {
                                                app.input_buffer = content;
                                                app.cursor_pos = app.input_buffer.chars().count();
                                                app.messages.push(ChatMessage::ui_notice(format!("Could not clear conversation: {error}")));
                                                continue;
                                            }
                                            app.session_id = dume_provider::types::StreamRequestContext::new().session_id;
                                            app.messages.clear();
                                            app.streaming_text.clear();
                                            app.auto_scroll = true;
                                            app.scroll_offset = 0;
                                            continue;
                                        } else if trimmed == "/resume" || trimmed.starts_with("/resume ") {
                                            let arg = trimmed.strip_prefix("/resume").unwrap_or("").trim();
                                            if arg.is_empty() || arg == "all" {
                                                let all_dirs = arg == "all";
                                                let query_cwd = if all_dirs { None } else { Some(cwd.as_str()) };
                                                let sessions = harness_store.list_recent_conversations(query_cwd, 20).unwrap_or_default();
                                                app.modal = ModalState::ResumePicker {
                                                    sessions,
                                                    selected_idx: 0,
                                                };
                                            } else {
                                                let target_id = arg;
                                                let found = harness_store.load_conversation(target_id).ok().or_else(|| {
                                                    let list = harness_store.list_conversations(None, 50).ok()?;
                                                    let matched = list.into_iter().find(|c| c.id.starts_with(target_id))?;
                                                    harness_store.load_conversation(&matched.id).ok()
                                                });
                                                match found {
                                                    Some((loaded_rec, msg_recs)) => {
                                                        match ConversationLock::try_acquire(&loaded_rec.id) {
                                                            Ok(new_lock) => {
                                                                conversation_lock = new_lock;
                                                                app.conversation_id = loaded_rec.id.clone();
                                                                let _ = refresh_session_timing(&mut app, &harness_store);
                                                                app.model = loaded_rec.model.clone();
                                                                app.restore_thinking();
                                                                app.session_id = loaded_rec.provider_session_id.clone();
                                                                app.messages.clear();
                                                                for mr in msg_recs {
                                                                    if let Ok(chat_msg) = serde_json::from_str::<ChatMessage>(&mr.message_json) {
                                                                        app.messages.push(chat_msg);
                                                                    }
                                                                }
                                                                let (was_repaired, prompt) = repair_conversation_history(&mut app.messages);
                                                                if let Some(p) = prompt {
                                                                    app.input_buffer = p;
                                                                    app.cursor_pos = app.input_buffer.chars().count();
                                                                }
                                                                let short_id = if loaded_rec.id.len() >= 8 { &loaded_rec.id[..8] } else { &loaded_rec.id };
                                                                app.messages.push(ChatMessage::ui_notice(if was_repaired {
                                                                    format!("Switched to conversation {} (repaired interrupted state).", short_id)
                                                                } else {
                                                                    format!("Switched to conversation {}.", short_id)
                                                                }));
                                                            }
                                                            Err(e) => {
                                                                app.messages.push(ChatMessage::ui_notice(format!("Cannot resume conversation: {}", e)));
                                                            }
                                                        }
                                                    }
                                                    None => {
                                                        app.messages.push(ChatMessage::ui_notice(format!("Conversation '{}' not found.", target_id)));
                                                    }
                                                }
                                            }
                                            continue;
                                        } else if trimmed == "/skills" {
                                            app.skills.reload();
                                            let skills = app.skills.list();
                                            let mut text = String::from("Available Skills:\n");
                                            if skills.is_empty() {
                                                text.push_str("  (No skills available)\n");
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
                                                                run_id: None,
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
                                                                run_id: None,
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
                                                "DUM-E Commands:\n  /model <provider/model>    - Switch model (e.g. anthropic/claude-sonnet-4-5, openai/gpt-4o)\n  /resume [all|<id>]        - Resume a previous conversation session\n  /run resume [run-id]      - Explicitly resume a paused run in this conversation\n  /queue resume <input-id> - Reactivate a paused queued input\n  /subagents                - Open the subagent model selector\n  /subagents model <model|inherit> - Set subagent model\n  /login <provider> <key>    - Save API key directly in TUI\n  /logout <provider>         - Clear saved credentials\n  /update                    - Check and install latest version in-place\n  /clear                     - Clear conversation transcript\n  /skills                    - List available skills\n  /help                      - Show this help\n\nShortcuts:\n  Ctrl+C / Ctrl+D - Exit\n  Ctrl+L          - Clear screen\n  PageUp/Down     - Scroll transcript\n  Mouse Wheel     - Scroll up/down\n"
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
                                                    format!("Execute skill '{}':\n\n{}", skill.name, skill.content)
                                                } else {
                                                    format!("Execute skill '{}' with instructions: {}\n\n{}", skill.name, rest_args, skill.content)
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
                                                let run_input = format!("/{}", cmd);
                                                let client_id = dume_provider::types::StreamRequestContext::new().request_id;
                                                let run_id = match harness_store.admit_input(RuntimeScope::Conversation(app.conversation_id.clone()), &client_id, InputKind::Submit, &run_input) {
                                                    Ok(receipt) => receipt.run_id.expect("submission starts run"),
                                                    Err(error) => {
                                                        app.messages.push(ChatMessage::ui_notice(format!("Could not admit skill run: {error}")));
                                                        app.is_busy = false;
                                                        continue;
                                                    }
                                                };
                                                app.begin_run(run_id.clone());
                                                let store = harness_store.clone();
                                                let conversation_id = app.conversation_id.clone();
                                                routing_snapshot.reload_for_new_input();
                                                let run_routing = routing_snapshot.clone();

                                                task = Some(tokio::spawn(async move {
                                                    dispatch_stream(&model_name, session_id, msgs, tx, dispatcher, child_cancel, store, conversation_id, run_id, run_routing).await;
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
                                        let client_id = dume_provider::types::StreamRequestContext::new().request_id;
                                        let receipt = match harness_store.admit_input(
                                            RuntimeScope::Conversation(app.conversation_id.clone()),
                                            &client_id,
                                            InputKind::Submit,
                                            &content,
                                        ) {
                                            Ok(receipt) => receipt,
                                            Err(error) => {
                                                app.messages.push(ChatMessage::ui_notice(format!("Could not admit input: {error}")));
                                                app.input_buffer = content;
                                                app.cursor_pos = app.input_buffer.chars().count();
                                                continue;
                                            }
                                        };
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
                                        let run_id = receipt.run_id.expect("admitted submission creates a run");
                                        app.begin_run(run_id.clone());
                                        let store = harness_store.clone();
                                        let conversation_id = app.conversation_id.clone();
                                        routing_snapshot.reload_for_new_input();
                                        let run_routing = routing_snapshot.clone();

                                        task = Some(tokio::spawn(async move {
                                            dispatch_stream(&model_name, session_id, msgs, tx, dispatcher, child_cancel, store, conversation_id, run_id, run_routing).await;
                                        }));
                                    }
                                }
                                Action::SubmitFollowUp => {
                                    if app.input_buffer.trim().is_empty() { continue; }
                                    let content = app.input_buffer.trim().to_owned();
                                    let request_id = dume_provider::types::StreamRequestContext::new().request_id;
                                    match harness_store.admit_input(
                                        RuntimeScope::Conversation(app.conversation_id.clone()),
                                        &request_id,
                                        InputKind::FollowUp,
                                        &content,
                                    ) {
                                        Ok(receipt) => {
                                            app.input_buffer.clear();
                                            app.cursor_pos = 0;
                                            app.messages.push(ChatMessage::ui_notice(format!("Follow-up queued ({})", receipt.disposition)));
                                            if !app.is_busy {
                                                if let Some(run_id) = receipt.run_id {
                                                    let history = harness_store.conversation_history(&app.conversation_id)?;
                                                    let messages = history.iter().filter_map(|row| serde_json::from_str(&row.message_json).ok()).collect();
                                                    app.begin_run(run_id.clone());
                                                    app.is_busy = true;
                                                    app.busy_started_at = Some(std::time::Instant::now());
                                                    let model_name = app.model.clone();
                                                    let session_id = app.session_id.clone();
                                                    let tx = stream_tx.clone();
                                                    let run_cancel = cancellation.child_token();
                                                    current_stream_cancel = Some(run_cancel.clone());
                                                    let task_dispatcher = dispatcher.clone();
                                                    let store = harness_store.clone();
                                                    let conversation_id = app.conversation_id.clone();
                                                    routing_snapshot.reload_for_new_input();
                                                    let run_routing = routing_snapshot.clone();
                                                    task = Some(tokio::spawn(async move {
                                                        dispatch_stream(&model_name, session_id, messages, tx, task_dispatcher, run_cancel, store, conversation_id, run_id, run_routing).await;
                                                    }));
                                                }
                                            }
                                        }
                                        Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Could not queue follow-up: {error}"))),
                                    }
                                }
                                Action::None => {}
                            }
                        }
                        Event::Mouse(mouse) => {
                            match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    let mut click_login = None;
                                    let mut click_logout = None;
                                    match &app.modal {
                                        ModalState::None if app.get_autocomplete_items().is_empty() => {
                                            let transcript_cfg = app.transcript_config();
                                            let (lines, _) = build_transcript_lines(
                                                &app.messages,
                                                &app.streaming_text,
                                                &app.recent_sessions,
                                                terminal.size()?.width as usize,
                                                &app.theme,
                                                Some(&transcript_cfg),
                                            );
                                            if selection.start(mouse, lines) {
                                                app.auto_scroll = false;
                                            }
                                        }
                                        ModalState::LoginSelector { .. } => {
                                            let size = terminal.size()?;
                                            let width = 72.min(size.width.saturating_sub(6));
                                            let height = 12.min(size.height.saturating_sub(4));
                                            let x = (size.width.saturating_sub(width)) / 2;
                                            let y = (size.height.saturating_sub(height)) / 2;
                                            if mouse.column >= x
                                                && mouse.column < x + width
                                                && mouse.row >= y + 3
                                                && mouse.row < y + 3 + 8
                                            {
                                                click_login = Some((mouse.row - (y + 3)) as usize);
                                            }
                                        }
                                        ModalState::LogoutSelector { .. } => {
                                            let size = terminal.size()?;
                                            let width = 72.min(size.width.saturating_sub(6));
                                            let height = 12.min(size.height.saturating_sub(4));
                                            let x = (size.width.saturating_sub(width)) / 2;
                                            let y = (size.height.saturating_sub(height)) / 2;
                                            if mouse.column >= x
                                                && mouse.column < x + width
                                                && mouse.row >= y + 3
                                                && mouse.row < y + 3 + 8
                                            {
                                                click_logout = Some((mouse.row - (y + 3)) as usize);
                                            }
                                        }
                                        _ => {}
                                    }
                                    if let Some(idx) = click_login {
                                        select_login_provider(&mut app, idx, &stream_tx);
                                    }
                                    if let Some(idx) = click_logout {
                                        select_logout_provider(&mut app, idx);
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => selection.drag(mouse),
                                MouseEventKind::Up(MouseButton::Left) => {
                                    if let Some(text) = selection.release(mouse) {
                                        if let Err(error) = copy_to_clipboard(text).await {
                                            app.messages.push(ChatMessage::ui_notice(format!("Could not copy selection: {error}")));
                                        }
                                    }
                                }
                                MouseEventKind::ScrollUp => {
                                    match &mut app.modal {
                                        ModalState::ResumePicker { sessions, selected_idx } => {
                                            if !sessions.is_empty() {
                                                if *selected_idx == 0 {
                                                    *selected_idx = sessions.len() - 1;
                                                } else {
                                                    *selected_idx -= 1;
                                                }
                                            }
                                        }
                                        ModalState::ModelSelector { target, filtered, selected_idx, .. } => {
                                            let count = filtered.len() + target.model_offset();
                                            if count > 0 {
                                                if *selected_idx == 0 {
                                                    *selected_idx = count - 1;
                                                } else {
                                                    *selected_idx -= 1;
                                                }
                                            }
                                        }
                                        ModalState::LoginSelector { selected_idx }
                                        | ModalState::LogoutSelector { selected_idx } => {
                                            if *selected_idx == 0 {
                                                *selected_idx = 7;
                                            } else {
                                                *selected_idx -= 1;
                                            }
                                        }
                                        ModalState::RoutingSettings(draft) => {
                                            if draft.picker_models.is_some() {
                                                draft.picker_selected = draft.picker_selected.saturating_sub(1);
                                            } else {
                                                draft.field_edit = false;
                                                draft.selected = draft.selected.saturating_sub(1);
                                            }
                                        }
                                        ModalState::None => {
                                            app.auto_scroll = false;
                                            app.scroll_offset = app.scroll_offset.saturating_sub(2);
                                        }
                                        _ => {}
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    match &mut app.modal {
                                        ModalState::ResumePicker { sessions, selected_idx } => {
                                            if !sessions.is_empty() {
                                                if *selected_idx + 1 >= sessions.len() {
                                                    *selected_idx = 0;
                                                } else {
                                                    *selected_idx += 1;
                                                }
                                            }
                                        }
                                        ModalState::ModelSelector { target, filtered, selected_idx, .. } => {
                                            let count = filtered.len() + target.model_offset();
                                            if count > 0 {
                                                if *selected_idx + 1 >= count {
                                                    *selected_idx = 0;
                                                } else {
                                                    *selected_idx += 1;
                                                }
                                            }
                                        }
                                        ModalState::LoginSelector { selected_idx }
                                        | ModalState::LogoutSelector { selected_idx } => {
                                            if *selected_idx + 1 >= 8 {
                                                *selected_idx = 0;
                                            } else {
                                                *selected_idx += 1;
                                            }
                                        }
                                        ModalState::RoutingSettings(draft) => {
                                            if draft.picker_models.is_some() {
                                                let max_len = draft.picker_models.as_ref().map_or(0, Vec::len);
                                                draft.picker_selected = (draft.picker_selected + 1).min(max_len);
                                            } else {
                                                draft.field_edit = false;
                                                draft.selected = (draft.selected + 1).min(12);
                                            }
                                        }
                                        ModalState::None => {
                                            app.scroll_offset = app.scroll_offset.saturating_add(2);
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                        Event::Paste(text) => {
                            selection.clear();
                            let text = normalize_pasted_text(&text);
                            match &mut app.modal {
                                ModalState::None => {
                                    app.autocomplete_dismissed = true;
                                    for c in text.chars() {
                                        app.insert_char(c);
                                    }
                                }
                                ModalState::ApiKeyInput { input, .. } => {
                                    input.push_str(text.trim());
                                }
                                ModalState::BtwChat { input, .. } => {
                                    input.push_str(&text);
                                }
                                ModalState::ModelSelector { filter, models, filtered, selected_idx, provider_tabs, active_tab_idx, .. } => {
                                    filter.push_str(text.trim());
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
                        }
                        Event::Resize(_, _) => {
                            selection.clear();
                            screen_recovery.request_repaint();
                        }
                        Event::FocusGained => screen_recovery.request_repaint(),
                        _ => {}
                    }
                }
            }
            Some(stream_event) = stream_rx.recv() => {
                if matches!(
                    &stream_event,
                    ConversationEvent::Compacted(_)
                        | ConversationEvent::Stream(StreamEvent::Retrying { .. })
                ) {
                    selection.clear();
                }
                let finished_run_id = match &stream_event {
                    ConversationEvent::Finished { run_id: Some(run_id), .. }
                        if app.current_run_id.as_deref() == Some(run_id.as_str()) => Some(run_id.clone()),
                    _ => None,
                };
                if let ConversationEvent::OAuthComplete { provider, result: Ok(()) } = &stream_event {
                    if provider == "openai-codex" {
                        spawn_codex_catalog_refresh(stream_tx.clone());
                    }
                }
                match &stream_event {
                    ConversationEvent::Compacted(msgs) => {
                        let _ = persist_tx.send(PersistEvent::Replace {
                            conv_id: app.conversation_id.clone(),
                            msgs: msgs.clone(),
                        });
                    }
                    _ => {}
                }
                app.receive(stream_event);
                if let Some(finished_run_id) = finished_run_id {
                    if app.current_run_id.as_deref() == Some(finished_run_id.as_str()) {
                        let _ = refresh_session_timing(&mut app, &harness_store);
                        match harness_store.run_metrics(&finished_run_id) {
                            Ok(metric) => {
                                app.last_run_metrics = Some(metric.clone());
                                match harness_store.conversation_runtime_metrics(&app.conversation_id) {
                                    Ok(metrics) => {
                                        let primary: Vec<_> = metrics.runs.iter().filter(|run| matches!(&run.scope, RuntimeScope::Conversation(_))).collect();
                                        app.session_elapsed_ms = primary.iter().map(|run| run.elapsed_ms).sum();
                                        app.session_timing_complete = !primary.is_empty() && primary.iter().all(|run| run.timing_complete);
                                    }
                                    Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Could not refresh session timing: {error}"))),
                                }
                                let duration = format_duration(metric.elapsed_ms);
                                let duration = if metric.timing_complete { duration } else { format!("최소 {duration}") };
                                app.messages.push(ChatMessage::ui_notice(format!("Run {} {} ({})", status_label(&metric.status), duration, metric.run_id)));
                            }
                            Err(error) => app.messages.push(ChatMessage::ui_notice(format!("Run finished, but timing could not be loaded: {error}"))),
                        }
                    }
                    let scope = RuntimeScope::Conversation(app.conversation_id.clone());
                    if let Some(next_run) = harness_store.active_run(&scope)?
                        .filter(|run| run.run_id != finished_run_id && run.status == "queued")
                    {
                        let history = harness_store.conversation_history(&app.conversation_id)?;
                        let messages = history.iter()
                            .filter_map(|row| serde_json::from_str(&row.message_json).ok())
                            .collect();
                        let run_id = next_run.run_id;
                        app.begin_run(run_id.clone());
                        app.is_busy = true;
                        app.busy_started_at = Some(std::time::Instant::now());
                        let model_name = app.model.clone();
                        let session_id = app.session_id.clone();
                        let tx = stream_tx.clone();
                        let run_cancel = cancellation.child_token();
                        current_stream_cancel = Some(run_cancel.clone());
                        let task_dispatcher = dispatcher.clone();
                        let store = harness_store.clone();
                        let conversation_id = app.conversation_id.clone();
                        routing_snapshot.reload_for_new_input();
                        let run_routing = routing_snapshot.clone();
                        task = Some(tokio::spawn(async move {
                            dispatch_stream(&model_name, session_id, messages, tx, task_dispatcher, run_cancel, store, conversation_id, run_id, run_routing).await;
                        }));
                    }
                }
            }
            _ = async {
                #[cfg(unix)]
                wait_for_signal(&mut sighup, &mut sigterm).await;
                #[cfg(not(unix))]
                std::future::pending::<()>().await;
            } => {
                let _ = persist_tx.send(PersistEvent::Status {
                    conv_id: app.conversation_id.clone(),
                    status: "interrupted".to_string(),
                });
                break 'app_loop;
            }
            _ = ticker.tick() => {
                if app.is_busy {
                    anim_tick = anim_tick.wrapping_add(1);
                }
            }
        }

        let state = if cancellation.is_cancelled() {
            ProgramStatus::Idle
        } else if matches!(&app.modal, ModalState::LoginSelector { .. } | ModalState::LogoutSelector { .. } | ModalState::ApiKeyInput { .. } | ModalState::CodexOAuthWaiting { .. }) {
            ProgramStatus::BlockedAuth
        } else if app.is_busy || btw_tasks.iter().any(|(_, handle)| !handle.is_finished()) {
            ProgramStatus::Working
        } else {
            ProgramStatus::Idle
        };
        let mut stdout = io::stdout().lock();
        status_reporter.emit(&mut stdout, state)?;
        stdout.flush()?;

      }
      Ok(())
    }.await;

    let _ = persist_tx.send(PersistEvent::Status {
        conv_id: app.conversation_id.clone(),
        status: "closed".to_string(),
    });
    drop(persist_tx);
    let _ = persist_handle.await;

    cancellation.cancel();
    let _ = reader_task.await;
    if let ModalState::RoutingSettings(draft) | ModalState::RoutingProbeConfirm(draft) =
        &mut app.modal
    {
        draft.cancel_probe();
    }
    routing_probe_tasks.abort_all();
    while routing_probe_tasks.join_next().await.is_some() {}
    for (token, _) in &btw_tasks {
        token.cancel();
    }
    stream_rx.close();
    if let Some(task) = task {
        let _ = task.await;
    }
    for (_, task) in btw_tasks {
        let _ = task.await;
    }
    dispatcher.lock().await.cancel_all().await;
    result
}

fn spawn_admitted_run(
    model: String,
    session_id: String,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<ConversationEvent>,
    dispatcher: Arc<Mutex<ToolDispatcher>>,
    cancellation: CancellationToken,
    store: Arc<HarnessStore>,
    conversation_id: String,
    run_id: String,
    routing_snapshot: dume_worker::routing::RoutingSnapshot,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        dispatch_stream(
            &model,
            session_id,
            messages,
            tx,
            dispatcher,
            cancellation,
            store,
            conversation_id,
            run_id,
            routing_snapshot,
        )
        .await;
    })
}

async fn dispatch_stream(
    model: &str,
    mut session_id: String,
    mut messages: Vec<ChatMessage>,
    tx: mpsc::Sender<ConversationEvent>,
    dispatcher: Arc<Mutex<ToolDispatcher>>,
    cancellation: CancellationToken,
    store: Arc<HarnessStore>,
    conversation_id: String,
    run_id: String,
    routing_snapshot: dume_worker::routing::RoutingSnapshot,
) {
    let previous_status = store.get_run(&run_id).ok().map(|record| record.status);
    let resuming = routing_run_status(previous_status.as_deref());
    let session_result = match previous_status.as_deref() {
        Some("interrupted" | "needs_attention") => {
            RunSession::resume(store.clone(), &run_id, &conversation_id)
        }
        _ => RunSession::start(
            store.clone(),
            RuntimeScope::Conversation(conversation_id.clone()),
            &run_id,
            &conversation_id,
        ),
    };
    let run_session = match session_result {
        Ok(session) => session,
        Err(error) => {
            let _ = tx
                .send(ConversationEvent::Finished {
                    run_id: Some(run_id.clone()),
                    messages,
                    error: Some(format!("Could not start durable run: {error:#}")),
                    session_id: Some(session_id),
                })
                .await;
            return;
        }
    };
    let mut dispatcher = dispatcher.lock().await;
    dispatcher.set_model(model);
    if let Err(error) = run_session
        .with_checkpoints(&cancellation, dispatcher.initialize_mcp(&conversation_id))
        .await
    {
        let _ = run_session.settle(
            "failed",
            Some(&format!("MCP initialization failed: {error:#}")),
        );
        drop(dispatcher);
        drop(run_session);
        let _ = tx
            .send(ConversationEvent::Finished {
                run_id: Some(run_id),
                messages,
                error: Some(format!("MCP initialization failed: {error:#}")),
                session_id: Some(session_id),
            })
            .await;
        return;
    }
    match run_session
        .with_checkpoints(&cancellation, dispatcher.replay_pending_tools(&run_session))
        .await
    {
        Ok(replayed) => {
            messages.extend(replayed);
        }
        Err(error) => {
            let _ = run_session.settle(
                "needs_attention",
                Some(&format!(
                    "Tool recovery requires operator review: {error:#}"
                )),
            );
            drop(dispatcher);
            drop(run_session);
            let _ = tx
                .send(ConversationEvent::Finished {
                    run_id: Some(run_id),
                    messages,
                    error: Some(format!("Tool recovery requires operator review: {error:#}")),
                    session_id: Some(session_id),
                })
                .await;
            return;
        }
    }
    let mut routing_snapshot =
        routing_snapshot_for_run(routing_snapshot, previous_status.as_deref());
    if !resuming {
        routing_snapshot.reload_for_new_input();
    }
    let result = run_session
        .with_checkpoints(
            &cancellation,
            dispatch_authenticated(
                model,
                &mut session_id,
                &mut messages,
                &tx,
                &mut dispatcher,
                &cancellation,
                &run_session,
                routing_snapshot,
            ),
        )
        .await;
    if cancellation.is_cancelled() {
        let _ = run_session.store().cancel_run(run_session.token());
    }
    dispatcher.cancel_all().await;
    let unknown_outcome = run_session
        .store()
        .tool_executions_for_run(&run_session.token().run_id)
        .map(|items| items.iter().any(|item| item.state == "outcome_unknown"))
        .unwrap_or(false);
    let terminal = if cancellation.is_cancelled() {
        "cancelled"
    } else if unknown_outcome {
        "needs_attention"
    } else if result.is_ok() {
        "succeeded"
    } else {
        "failed"
    };
    let reason = result.as_ref().err().map(|error| {
        let rendered = format!("{error:#}");
        if rendered.contains("turn limit") {
            format!("turn_limit: {rendered}")
        } else {
            rendered
        }
    });
    let _ = run_session.settle(terminal, reason.as_deref());
    drop(dispatcher);
    drop(run_session);
    let _ = tx
        .send(ConversationEvent::Finished {
            run_id: Some(run_id),
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

struct PreparedRequest {
    messages: Vec<ChatMessage>,
    effective_model: String,
}

struct DispatchState {
    session_id: String,
    pending: Option<PendingCompaction>,
}

fn model_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut payload = vec![ChatMessage::system(
        dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT,
    )];
    payload.extend(
        messages
            .iter()
            .filter(|message| {
                message.is_model_visible()
                    && !(message.role == dume_provider::Role::System
                        && message.content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT)
            })
            .cloned(),
    );
    payload
}

async fn dispatch_authenticated(
    model: &str,
    session_id: &mut String,
    messages: &mut Vec<ChatMessage>,
    tx: &mpsc::Sender<ConversationEvent>,
    dispatcher: &mut ToolDispatcher,
    cancellation: &CancellationToken,
    run_session: &RunSession,
    mut routing_snapshot: dume_worker::routing::RoutingSnapshot,
) -> Result<String> {
    if !messages.iter().any(|m| {
        m.role == dume_provider::Role::System
            && m.content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT
    }) {
        messages.insert(
            0,
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
        );
    }
    if cancellation.is_cancelled() {
        anyhow::bail!("Conversation cancelled");
    }
    let cred_store =
        dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
    let model = model.to_string();
    let store = dispatcher.store().cloned();
    let session_state = Arc::new(std::sync::Mutex::new(DispatchState {
        session_id: session_id.clone(),
        pending: None,
    }));

    let result = run_conversation(
        messages,
        tx,
        dispatcher,
        session_id,
        cancellation,
        Some(run_session),
        None,
        Some(&mut routing_snapshot),
        &model,
        |raw_messages, tools, request_session_id, effective_model, tx| {
            let cred_store = cred_store.clone();
            let model = effective_model;
            let store = store.clone();
            let session_state = Arc::clone(&session_state);
            async move {
                let mut raw_messages = raw_messages;
                let before = raw_messages.clone();
                let mut session_id = request_session_id;
                session_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Dispatch state lock poisoned"))?
                    .session_id = session_id.clone();
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
                Ok(PreparedRequest {
                    messages: raw_messages,
                    effective_model: model,
                })
            }
        },
    )
    .await;
    let (had_compaction, new_session_id) = {
        let mut state = session_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Dispatch state lock poisoned"))?;
        let had_compaction = state.pending.is_some();
        if result.is_err() {
            if let Some(pending) = state.pending.take() {
                if *messages == pending.before {
                    *messages = pending.after;
                }
            }
        }
        *session_id = state.session_id.clone();
        (had_compaction, state.session_id.clone())
    };
    result?;
    if had_compaction {
        let _ = tx
            .send(ConversationEvent::Compacted(messages.clone()))
            .await;
    }
    Ok(new_session_id)
}

#[derive(Default)]
struct StreamTurn {
    text: String,
    reasoning: Vec<serde_json::Value>,
    deepseek_reasoning: String,
    gemini_parts: Vec<serde_json::Value>,
    anthropic_thinking: Vec<serde_json::Value>,
    calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
}

impl StreamTurn {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn push(&mut self, event: &StreamEvent) -> Result<()> {
        if let StreamEvent::Error(error) = event {
            anyhow::bail!("Model streaming error: {}", error);
        }
        if let StreamEvent::ProviderError { message, .. } = event {
            anyhow::bail!("Provider error: {}", message);
        }
        if matches!(event, StreamEvent::Retrying { .. }) {
            self.reset();
            return Ok(());
        }
        anyhow::ensure!(
            self.finish_reason.is_none()
                || matches!(
                    event,
                    StreamEvent::AttemptStarted { .. } | StreamEvent::AttemptFinished { .. }
                ),
            "Stream event after completion"
        );
        match event {
            StreamEvent::TextDelta(delta) => self.text.push_str(delta),
            StreamEvent::CodexReasoning(items) => self.reasoning.extend(items.iter().cloned()),
            StreamEvent::DeepSeekReasoning(delta) => self.deepseek_reasoning.push_str(delta),
            StreamEvent::GeminiParts(parts) => self.gemini_parts.extend(parts.iter().cloned()),
            StreamEvent::AnthropicThinking(parts) => {
                self.anthropic_thinking.extend(parts.iter().cloned())
            }
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
            StreamEvent::AttemptStarted { .. }
            | StreamEvent::ResponseIdentity(_)
            | StreamEvent::AttemptFinished { .. }
            | StreamEvent::ProviderError { .. }
            | StreamEvent::Retrying { .. } => {}
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
        message.anthropic_thinking = self.anthropic_thinking;
        Ok((message, calls))
    }
}

async fn run_btw_request<F, Fut>(
    model: &str,
    cwd: &str,
    primary_id: &str,
    history: Vec<(String, String)>,
    store: Arc<HarnessStore>,
    cancellation: CancellationToken,
    tx: mpsc::Sender<ConversationEvent>,
    stream: F,
) -> Result<String, String>
where
    F: FnOnce(
        String,
        Vec<ChatMessage>,
        dume_provider::types::StreamRequestContext,
        mpsc::Sender<StreamEvent>,
    ) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let result: anyhow::Result<ChatMessage> = async {
        let conversation_id = format!("btw:{primary_id}");
        if store.load_conversation(&conversation_id).is_err() {
            store.create_conversation(
                &conversation_id, cwd, model,
                &dume_provider::types::StreamRequestContext::new().session_id,
            )?;
        }
        let scope = RuntimeScope::Conversation(conversation_id.clone());
        let prompt = history.last().filter(|(role, _)| role == "user")
            .map(|(_, text)| text.as_str()).unwrap_or("");
        let context = dume_provider::types::StreamRequestContext::new();
        let receipt = store.admit_input(scope, &context.request_id, InputKind::Submit, prompt)?;
        let run_id = receipt.run_id.ok_or_else(|| anyhow::anyhow!("auxiliary admission produced no run"))?;
        let session = RunSession::start(store.clone(), RuntimeScope::Conversation(conversation_id.clone()), &run_id, &conversation_id)?;
        let mut messages = vec![ChatMessage::system(
            "You are a helpful coding assistant answering a quick side-question without tools. Keep your answer direct and concise.",
        )];
        for (role, content) in history.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
            if role == "user" { messages.push(ChatMessage::user(content)); }
            else { messages.push(ChatMessage::assistant(content)); }
        }
        let (event_tx, mut event_rx) = mpsc::channel(50);
        let collect = async {
            let mut turn = StreamTurn::default();
            while let Some(event) = event_rx.recv().await {
                session.record_model_event(&event)?;
                if matches!(event, StreamEvent::AttemptFinished { .. }) { session.checkpoint()?; }
                if matches!(event, StreamEvent::Retrying { .. }) {
                    turn.reset();
                    tx.send(ConversationEvent::BtwRetrying).await?;
                }
                if let StreamEvent::TextDelta(text) = &event {
                    tx.send(ConversationEvent::BtwStreamDelta(text.clone())).await?;
                }
                turn.push(&event)?;
            }
            Ok::<_, anyhow::Error>(turn.finish().and_then(|(assistant, calls)| {
                anyhow::ensure!(calls.is_empty(), "tool calls are disabled for /btw");
                Ok(assistant)
            }))
        };
        let operation = async {
            let (result, message) = tokio::try_join!(async { Ok::<_, anyhow::Error>(stream(model.to_string(), messages, context, event_tx).await) }, collect)?;
            result?;
            message
        };
        let result = session.with_checkpoints(&cancellation, async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => anyhow::bail!("Auxiliary request cancelled"),
                result = operation => result,
            }
        }).await;
        let result = result.and_then(|assistant| {
            session.record_assistant(&assistant)?;
            Ok(assistant)
        });
        let terminal = if cancellation.is_cancelled() { "cancelled" } else if result.is_ok() { "succeeded" } else { "failed" };
        let reason = result.as_ref().err().map(|error| format!("{error:#}"));
        session.settle(terminal, reason.as_deref())?;
        result
    }.await;
    result
        .map(|message| message.content)
        .map_err(|error| format!("{error:#}"))
}

async fn run_conversation<F, Fut>(
    messages: &mut Vec<ChatMessage>,
    tx: &mpsc::Sender<ConversationEvent>,
    dispatcher: &mut ToolDispatcher,
    session_id: &mut String,
    cancellation: &CancellationToken,
    run_session: Option<&RunSession>,
    max_turns: Option<usize>,
    mut routing_snapshot: Option<&mut dume_worker::routing::RoutingSnapshot>,
    original_model: &str,
    mut stream: F,
) -> Result<()>
where
    F: FnMut(
        Vec<ChatMessage>,
        Vec<dume_provider::types::ToolDefinition>,
        String,
        String,
        mpsc::Sender<StreamEvent>,
    ) -> Fut,
    Fut: std::future::Future<Output = Result<PreparedRequest>>,
{
    let mut turn_count = 0;
    loop {
        if let Some(limit) = max_turns {
            if turn_count >= limit {
                anyhow::bail!("Conversation turn limit reached ({})", limit);
            }
        }
        turn_count += 1;
        dispatcher.refresh_mcp().await?;
        let tools = dispatcher.tool_definitions_for_session(session_id);
        let (sub_tx, mut sub_rx) = mpsc::channel(100);
        let collect = async {
            let mut turn = StreamTurn::default();
            let mut actual_model = None;
            while let Some(event) = sub_rx.recv().await {
                if let StreamEvent::AttemptFinished {
                    model,
                    outcome: dume_provider::AttemptOutcome::Succeeded,
                    ..
                } = &event
                {
                    actual_model = Some(model.clone());
                }
                if let Some(session) = run_session {
                    session.record_model_event(&event)?;
                    if matches!(event, StreamEvent::AttemptFinished { .. }) {
                        session.checkpoint()?;
                    }
                }
                if matches!(event, StreamEvent::Retrying { .. }) {
                    turn.reset();
                }
                turn.push(&event)?;
                if matches!(
                    event,
                    StreamEvent::TextDelta(_)
                        | StreamEvent::ToolCallDelta { .. }
                        | StreamEvent::Usage(_)
                        | StreamEvent::AttemptStarted { .. }
                        | StreamEvent::AttemptFinished { .. }
                        | StreamEvent::ResponseIdentity(_)
                        | StreamEvent::Retrying { .. }
                ) {
                    tx.send(ConversationEvent::Stream(event)).await?;
                }
            }
            Ok((turn.finish(), actual_model))
        };
        // Both futures are scoped: errors drop the other side, never detach a producer.
        let effective_model = if let (Some(snapshot), Some(session)) =
            (routing_snapshot.as_deref_mut(), run_session)
        {
            let credentials =
                dume_provider::CredentialStore::new(dume_provider::CredentialStore::default_path());
            let decision = tokio::select! {
                biased;
                _ = cancellation.cancelled() => anyhow::bail!("Conversation cancelled"),
                result = dume_worker::routing::select_model(snapshot, session, original_model, messages, &credentials, cancellation) => result?,
            };
            let _ = tx
                .send(ConversationEvent::RoutingDecision(decision.clone()))
                .await;
            decision.effective_model
        } else {
            original_model.to_string()
        };
        let (prepared, (turn, actual_model)) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("Conversation cancelled"),
            // Drain queued attempt evidence when the producer fails; collector errors still cancel it.
            result = async { tokio::try_join!(async { Ok::<_, anyhow::Error>(stream(messages.clone(), tools, session_id.clone(), effective_model, sub_tx).await) }, collect) } => result?,
        };
        let prepared = prepared?;
        let (assistant, tool_calls) = turn?;
        dispatcher.set_model(actual_model.as_deref().unwrap_or(&prepared.effective_model));
        *messages = prepared.messages;
        messages.push(assistant.clone());
        let executions = if let Some(session) = run_session {
            Some(session.prepare_tools(&assistant)?)
        } else {
            None
        };
        if tool_calls.is_empty() {
            dispatcher.cancel_all().await;
            anyhow::ensure!(!cancellation.is_cancelled(), "Conversation cancelled");
            if let Some(session) = run_session {
                match session.boundary()? {
                    BoundaryResult::Continue(inputs) => {
                        if !inputs.is_empty() {
                            if let Some(snapshot) = routing_snapshot.as_deref_mut() {
                                snapshot.reload_for_new_input();
                            }
                        }
                        for input in inputs {
                            messages.push(ChatMessage::user(input.payload));
                        }
                        dispatcher.initialize_mcp(session.conversation_id()).await?;
                        continue;
                    }
                    BoundaryResult::Terminal | BoundaryResult::NextRun(_) => return Ok(()),
                }
            }
            return Ok(());
        }
        for (index, call) in tool_calls.into_iter().enumerate() {
            let name = call.name.clone();
            let call_id = call.id.clone();
            let arguments = call.arguments.clone();
            let _ = tx
                .send(ConversationEvent::ToolStarted {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                })
                .await;
            let started =
                if let (Some(session), Some(executions)) = (run_session, executions.as_ref()) {
                    Some((session, session.begin_tool(&executions[index])?))
                } else {
                    None
                };
            let started_at = std::time::Instant::now();
            let mut is_failure = false;
            let tool_res = match started {
                Some((_, ToolStart::Cached { result })) => {
                    serde_json::from_str::<ChatMessage>(&result)?
                }
                Some((session, ToolStart::Execute { attempt_id })) => {
                    match dispatcher.execute(call, &messages).await {
                        Ok(tool_res) => {
                            let elapsed =
                                started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
                            let mut tool_res = tool_res;
                            tool_res.duration_ms = Some(elapsed as u64);
                            if tool_res.is_error.is_none() {
                                tool_res.is_error = Some(false);
                            }
                            session.finish_tool(
                                &executions.as_ref().expect("durable tool list")[index],
                                &attempt_id,
                                &tool_res,
                                elapsed,
                            )?;
                            session.checkpoint()?;
                            tool_res
                        }
                        Err(error) => {
                            let execution = &executions.as_ref().expect("durable tool list")[index];
                            let elapsed =
                                started_at.elapsed().as_millis().min(i64::MAX as u128) as i64;
                            if let Some(failure) =
                                error.downcast_ref::<dume_worker::tools::KnownToolFailure>()
                            {
                                is_failure = true;
                                let mut tool_res =
                                    ChatMessage::tool(failure.output.clone(), call_id.clone());
                                tool_res.duration_ms = Some(elapsed as u64);
                                tool_res.is_error = Some(true);
                                session.finish_tool_error(
                                    execution,
                                    &attempt_id,
                                    &tool_res,
                                    elapsed,
                                )?;
                                session.checkpoint()?;
                                tool_res
                            } else {
                                session.mark_tool_unknown(
                                    execution,
                                    &attempt_id,
                                    &format!("{error:#}"),
                                    elapsed,
                                )?;
                                dispatcher.cancel_all().await;
                                return Err(error);
                            }
                        }
                    }
                }
                None => match dispatcher.execute(call, &messages).await {
                    Ok(mut tool_res) => {
                        let elapsed =
                            started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                        tool_res.duration_ms = Some(elapsed);
                        if tool_res.is_error.is_none() {
                            tool_res.is_error = Some(false);
                        }
                        tool_res
                    }
                    Err(error) => {
                        if let Some(failure) =
                            error.downcast_ref::<dume_worker::tools::KnownToolFailure>()
                        {
                            is_failure = true;
                            let elapsed =
                                started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            let mut tool_res =
                                ChatMessage::tool(failure.output.clone(), call_id.clone());
                            tool_res.duration_ms = Some(elapsed);
                            tool_res.is_error = Some(true);
                            tool_res
                        } else {
                            return Err(error);
                        }
                    }
                },
            };
            let elapsed_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let mut tool_res = tool_res;
            if tool_res.duration_ms.is_none() {
                tool_res.duration_ms = Some(elapsed_ms);
            }
            if is_failure {
                tool_res.is_error = Some(true);
            } else if tool_res.is_error.is_none() {
                tool_res.is_error = Some(false);
            }
            let is_err = tool_res.is_error.unwrap_or(false);
            let err_content = if is_err {
                Some(tool_res.content.clone())
            } else {
                None
            };
            let _ = tx
                .send(ConversationEvent::ToolFinished {
                    call_id: call_id.clone(),
                    duration_ms: elapsed_ms,
                    is_error: is_err,
                    error: err_content,
                    output: tool_res.content.clone(),
                })
                .await;
            messages.push(tool_res);
        }
        if let Some(session) = run_session {
            let inputs = session.consume_steer()?;
            if !inputs.is_empty() {
                if let Some(snapshot) = routing_snapshot.as_deref_mut() {
                    snapshot.reload_for_new_input();
                }
            }
            for input in inputs {
                messages.push(ChatMessage::user(input.payload));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_picker_settings_restore_per_model_and_skip_unsupported_levels() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::new("openai-codex/gpt-5.4");
        app.thinking_path = directory.path().join("models-store.json");
        app.thinking_preferences.clear();
        let mut first = dume_provider::resolve_model("openai-codex/gpt-5.4").unwrap();
        first.reasoning_levels = Some(vec![ThinkingLevel::Low, ThinkingLevel::High]);
        app.cycle_thinking(first.clone());
        assert_eq!(app.thinking_for(&first), ThinkingLevel::Low);
        app.cycle_thinking(first.clone());
        assert_eq!(app.thinking_for(&first), ThinkingLevel::High);
        let mut second = first.clone();
        second.provider = "openai".into();
        assert_eq!(app.thinking_for(&second), ThinkingLevel::Default);
        app.cycle_thinking(second.clone());
        assert_eq!(app.thinking_for(&second), ThinkingLevel::Low);
        assert_eq!(app.thinking_for(&first), ThinkingLevel::High);
        app.thinking_preferences = dume_provider::reasoning::load_preferences(&app.thinking_path).unwrap();
        assert_eq!(app.thinking_for(&first), ThinkingLevel::High);
        let mut unsupported = first.clone();
        unsupported.reasoning_levels = None;
        assert_eq!(app.thinking_for(&unsupported), ThinkingLevel::Default);
        app.cycle_thinking(unsupported);
        assert!(app.messages.last().unwrap().content.contains("unavailable"));
        app.modal = ModalState::model_selector(vec![first], &app.model, ModelSelectorTarget::Conversation);
        assert_eq!(app.picker_thinking(), ThinkingLevel::High);
    }

    #[test]
    fn status_keeps_requested_model_and_provider_errors_leave_input_available() {
        let mut app = App::new("openai-codex/gpt-5.4");
        app.is_busy = true;
        let mut context = dume_provider::types::StreamRequestContext::new();
        context.reasoning_effort = Some(ThinkingLevel::High);
        app.receive(ConversationEvent::Stream(StreamEvent::AttemptStarted {
            context: context.clone(), model: app.model.clone(),
        }));
        assert_eq!(app.status_thinking(), ThinkingLevel::High);
        for identity in [
            dume_provider::types::ResponseIdentity::default(),
            dume_provider::types::ResponseIdentity {
                model: Some("provider-alias".into()), response_id: Some("response-id".into()),
            },
        ] {
            app.receive(ConversationEvent::Stream(StreamEvent::ResponseIdentity(identity.clone())));
            app.receive(ConversationEvent::Stream(StreamEvent::AttemptFinished {
                context: context.clone(), model: app.model.clone(),
                outcome: dume_provider::AttemptOutcome::Succeeded, duration_ms: 1, usage: None, error: None, identity,
            }));
            assert_eq!(app.status_model(), app.model);
        }
        let error = "Provider: model_not_found: requested model is unavailable";
        app.receive(ConversationEvent::Finished {
            run_id: None, messages: Vec::new(), error: Some(error.into()), session_id: None,
        });
        assert!(!app.is_busy);
        assert_eq!(app.status_model(), app.model);
        assert_eq!(app.messages.last().unwrap().content, format!("Error: {error}"));
    }
    use dume_provider::types::Role;
    use std::collections::VecDeque;

    #[test]
    fn pasted_line_endings_are_normalized_before_rendering_or_submission() {
        for pasted in [
            "first\rsecond\r안녕",
            "first\r\nsecond\r\n안녕",
            "first\nsecond\n안녕",
        ] {
            let normalized = normalize_pasted_text(pasted);
            assert_eq!(normalized, "first\nsecond\n안녕");
            assert_eq!(normalized.lines().count(), 3);
            assert!(!normalized.contains('\r'));
        }
        assert_eq!(
            normalize_pasted_text("first\r\n\r\nlast\r"),
            "first\n\nlast\n"
        );
    }

    #[test]
    fn routing_field_cursor_edits_korean_by_unicode_scalar_and_roles_do_not_change_parent_model() {
        let mut draft = RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into());
        draft.selected = 2;
        draft.settings.classifier = Some(dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::OpenCodeZen,
            base_url: "가나".into(),
            model: "분류".into(),
        });
        draft.cursor = 1;
        routing_insert_char(&mut draft, '다');
        assert_eq!(
            draft.settings.classifier.as_ref().unwrap().base_url,
            "가다나"
        );
        routing_delete_char(&mut draft);
        assert_eq!(draft.settings.classifier.as_ref().unwrap().base_url, "가나");
        draft.selected = 5;
        draft.picker_models = Some(Vec::new());
        routing_set_role_model(&mut draft, dume_worker::RoutingRole::ComplexPlanning, None);
        assert_eq!(draft.settings.models.complex, None);
        let app = App::new("parent/model");
        let model = ModelInfo {
            reasoning_levels: None,
            id: "selected".into(),
            name: "선택".into(),
            provider: "fixture".into(),
            api: String::new(),
            base_url: None,
            reasoning: false,
            input: Vec::new(),
            context_window: None,
            max_tokens: None,
            cost: None,
        };
        for role in [
            dume_worker::RoutingRole::StandardPlanning,
            dume_worker::RoutingRole::ComplexPlanning,
            dume_worker::RoutingRole::Implementation,
        ] {
            routing_set_role_model(&mut draft, role, Some(&model));
        }
        assert_eq!(
            draft.settings.models.standard.as_deref(),
            Some("fixture/selected")
        );
        assert_eq!(
            draft.settings.models.complex.as_deref(),
            Some("fixture/selected")
        );
        assert_eq!(
            draft.settings.models.implementation.as_deref(),
            Some("fixture/selected")
        );
        routing_set_role_model(&mut draft, dume_worker::RoutingRole::Implementation, None);
        assert_eq!(draft.settings.models.implementation, None);
        assert_eq!(app.model, "parent/model");
    }

    #[test]
    fn routing_role_picker_with_empty_catalog_still_offers_inheritance() {
        let mut draft = RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into());
        draft.selected = 4;
        routing_begin_role_picker(&mut draft, Vec::new());
        assert_eq!(draft.picker_selected, 0);
        assert_eq!(draft.picker_models.as_ref().unwrap().len(), 0);
        let role = draft.picker_role.take().unwrap();
        let models = draft.picker_models.take().unwrap();
        let model = draft
            .picker_selected
            .checked_sub(1)
            .and_then(|index| models.get(index));
        routing_set_role_model(&mut draft, role, model);
        assert_eq!(draft.settings.models.standard, None);
    }

    #[test]
    fn blank_routing_draft_fields_can_be_edited_and_validated_without_api_key() {
        let mut draft = RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into());
        draft.selected = 2;
        routing_begin_field_edit(&mut draft);
        for ch in "http://127.0.0.1:8080".chars() {
            routing_insert_char(&mut draft, ch);
        }
        draft.selected = 3;
        routing_begin_field_edit(&mut draft);
        for ch in "classifier-test".chars() {
            routing_insert_char(&mut draft, ch);
        }
        assert!(draft.settings.validate().is_ok());
        assert_eq!(
            draft.settings.classifier.as_ref().unwrap().base_url,
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            draft.settings.classifier.as_ref().unwrap().model,
            "classifier-test"
        );
        let public_json = serde_json::to_string(&draft.settings).unwrap();
        assert!(!public_json.contains("key_input"));
        assert!(!public_json.contains("classifier-test-secret"));
    }

    #[test]
    fn main_editor_forward_delete_removes_one_korean_scalar_and_is_noop_at_end() {
        let mut app = App::new("test/model");
        app.input_buffer = "가나다".into();
        app.cursor_pos = 1;
        app.delete_forward();
        assert_eq!(app.input_buffer, "가다");
        assert_eq!(app.cursor_pos, 1);
        app.cursor_pos = app.input_buffer.chars().count();
        app.delete_forward();
        assert_eq!(app.input_buffer, "가다");
    }

    #[test]
    fn routing_key_origin_uses_backend_store_and_never_returns_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = dume_provider::CredentialStore::new(&path);
        store.save_credential("opencode", "fixture-secret").unwrap();
        store
            .save_credential("jev-local-system-one", "local-secret")
            .unwrap();
        assert_eq!(
            routing_key_origin(dume_provider::ClassifierBackend::OpenCodeZen, &path, true),
            "환경변수"
        );
        assert_eq!(
            routing_key_origin(dume_provider::ClassifierBackend::OpenCodeZen, &path, false),
            "저장됨"
        );
        assert_eq!(
            routing_key_origin(
                dume_provider::ClassifierBackend::LocalSystemOne,
                &path,
                true
            ),
            "저장됨"
        );
        let origin = routing_key_origin(
            dume_provider::ClassifierBackend::LocalSystemOne,
            &path,
            false,
        );
        assert_eq!(origin, "저장됨");
        assert!(!origin.contains("local-secret"));
    }

    #[test]
    fn stale_routing_probe_completion_does_not_modify_a_new_draft() {
        let mut app = App::new("test/model");
        let mut draft = RoutingDraft::new(dume_worker::RoutingSettings::default(), "없음".into());
        draft.probe_id = 2;
        app.modal = ModalState::RoutingSettings(draft);
        app.receive(ConversationEvent::RoutingProbeFinished {
            probe_id: 1,
            message: "오래된 결과".into(),
        });
        let ModalState::RoutingSettings(draft) = &app.modal else {
            panic!("라우팅 편집기가 유지되어야 합니다")
        };
        assert_ne!(draft.message, "오래된 결과");
    }

    async fn routing_probe_fixture(
        body: &'static str,
        status: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            let body_start = loop {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(count > 0, "client closed before request completed");
                request.extend_from_slice(&chunk[..count]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..index]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= index + 4 + length {
                        break index + 4;
                    }
                }
            };
            let captured = String::from_utf8_lossy(&request).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            assert!(body_start <= request.len());
            captured
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn routing_probe_helper_sends_only_fixed_prompt_with_local_scoped_saved_credential() {
        let (url, task) = routing_probe_fixture(r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":0.8,"complex":0.2},"confidence":0.8}},"usage":{"input_tokens":3,"output_tokens":1}}"#, "200 OK").await;
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        store
            .save_credential("jev-local-system-one", "fixture-local-key")
            .unwrap();
        store.save_credential("opencode", "different-key").unwrap();
        let config = dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: url,
            model: "fixture-classifier".into(),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(7),
            routing_probe_request(config, store, CancellationToken::new()),
        )
        .await
        .unwrap();
        let request = task.await.unwrap();
        assert!(result.contains("토큰 3+1"));
        assert!(request.contains("Bearer fixture-local-key"));
        assert!(!request.contains("different-key"));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["state"]["prompt"], ROUTING_PROBE_PROMPT);
        assert_eq!(
            ROUTING_PROBE_PROMPT,
            "두 수를 더하는 간단한 함수 작성 요청의 복잡도를 분류하세요."
        );
    }

    #[tokio::test]
    async fn routing_probe_reuses_saved_opencode_zen_login() {
        let (url, task) = routing_probe_fixture(r#"{"answers":{"complexity":{"type":"choice","choice":"standard","probabilities":{"standard":0.8,"complex":0.2},"confidence":0.8}},"usage":{"input_tokens":3,"output_tokens":1}}"#, "200 OK").await;
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        store.save_credential("opencode", "saved-zen-key").unwrap();
        store
            .save_credential("jev-local-system-one", "local-key")
            .unwrap();
        let config = dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::OpenCodeZen,
            base_url: url,
            model: "jev-1.13-free".into(),
        };
        let result = routing_probe_request(config, store, CancellationToken::new()).await;
        let request = task.await.unwrap();
        assert!(result.contains("연결 확인 완료"));
        assert!(request.contains("Bearer saved-zen-key"));
        assert!(!request.contains("Bearer local-key"));
        assert!(request.contains("jev-1.13-free"));
    }

    #[tokio::test]
    async fn routing_probe_helper_hides_http_and_malformed_response_bodies() {
        for (body, status) in [
            (r#"{"secret":"fake-secret"}"#, "500 Internal Server Error"),
            ("not-json", "200 OK"),
            (
                r#"{"answers":{"complexity":{"type":"choice","choice":"standard"}}}"#,
                "200 OK",
            ),
        ] {
            let (url, task) = routing_probe_fixture(body, status).await;
            let dir = tempfile::tempdir().unwrap();
            let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
            let config = dume_provider::ClassifierConfig {
                backend: dume_provider::ClassifierBackend::LocalSystemOne,
                base_url: url,
                model: "fixture".into(),
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(7),
                routing_probe_request(config, store, CancellationToken::new()),
            )
            .await
            .unwrap();
            let request = task.await.unwrap();
            assert!(!result.contains("fake-secret"));
            assert!(!result.contains("not-json"));
            assert!(result.contains("실패") || result.contains("응답"));
            assert!(!request.contains("Authorization: Bearer"));
        }
    }

    #[tokio::test]
    async fn routing_probe_helper_precancel_performs_no_loopback_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        });
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        let config = dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: format!("http://{address}"),
            model: "fixture".into(),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            routing_probe_request(config, store, cancellation)
                .await
                .contains("취소")
        );
        assert!(accept.await.unwrap());
    }

    #[tokio::test]
    async fn routing_probe_draft_drop_cancels_abort_joins_owner_and_ignores_stale_result() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if let Some(boundary) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..boundary]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= boundary + 4 + length {
                        accepted_tx.send(request).unwrap();
                        break;
                    }
                }
            }
            // Keep the connection open until the probe task is aborted, then observe client EOF.
            let mut eof = false;
            while !eof {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                eof = count == 0;
            }
            assert!(eof);
            let extra_accept =
                tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                    .await;
            assert!(
                extra_accept.is_err(),
                "no second request should be accepted"
            );
        });
        let dir = tempfile::tempdir().unwrap();
        let credentials = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        credentials
            .save_credential("jev-local-system-one", "saved-local")
            .unwrap();
        let config = dume_provider::ClassifierConfig {
            backend: dume_provider::ClassifierBackend::LocalSystemOne,
            base_url: format!("http://{address}"),
            model: "fixture".into(),
        };
        let mut tasks = tokio::task::JoinSet::new();
        let mut draft = RoutingDraft::new(
            dume_worker::RoutingSettings {
                classifier: Some(config.clone()),
                ..Default::default()
            },
            "저장됨".into(),
        );
        let cancellation = CancellationToken::new();
        spawn_routing_probe(
            &mut tasks,
            &mut draft,
            config.clone(),
            credentials,
            cancellation,
        );
        assert_eq!(tasks.len(), 1);
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&request).contains("Bearer saved-local"));
        let old_id = draft.probe_id;
        drop(draft);
        assert_eq!(tasks.len(), 1);
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.join_next())
            .await
            .unwrap()
            .unwrap();
        assert!(joined.unwrap_err().is_cancelled());
        assert_eq!(tasks.len(), 0);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        let mut app = App::new("original/model");
        let mut reopened = RoutingDraft::new(
            dume_worker::RoutingSettings {
                classifier: Some(config),
                ..Default::default()
            },
            "저장됨".into(),
        );
        reopened.probe_id =
            NEXT_ROUTING_PROBE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let original_message = reopened.message.clone();
        app.modal = ModalState::RoutingSettings(reopened);
        app.receive(ConversationEvent::RoutingProbeFinished {
            probe_id: old_id,
            message: "오래된 결과".into(),
        });
        let ModalState::RoutingSettings(reopened) = &app.modal else {
            panic!("새 편집기가 유지되어야 합니다")
        };
        assert_eq!(reopened.message, original_message);
        assert_ne!(reopened.message, "오래된 결과");
        assert_eq!(reopened.key_state, "저장됨");
        assert_eq!(
            reopened.settings.classifier.as_ref().unwrap().backend,
            dume_provider::ClassifierBackend::LocalSystemOne
        );
    }

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
    fn routing_updates_status_without_notices_or_changing_model_history() {
        let mut app = App::new("original/model");
        app.is_busy = true;
        app.messages.push(ChatMessage::user("실제 요청"));
        let history_before = model_messages(&app.messages);
        app.receive(ConversationEvent::RoutingDecision(
            dume_worker::routing::RoutingDecision {
                original_model: "original/model".into(),
                effective_model: "effective/model".into(),
                phase: dume_store::RoutingPhase::Planning,
                epoch: Some("epoch".into()),
                reason: "complexity_complex".into(),
                classifier_usage: Some(dume_provider::classifier::ClassifierUsage {
                    input_tokens: 7,
                    output_tokens: 2,
                }),
            },
        ));
        assert_eq!(model_messages(&app.messages), history_before);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.status_model(), "effective/model");
        assert_eq!(app.model, "original/model");
        assert_eq!(
            app.last_routing_decision
                .as_ref()
                .unwrap()
                .classifier_usage
                .as_ref()
                .unwrap()
                .input_tokens,
            7
        );
        assert_eq!(
            app.last_routing_decision.as_ref().unwrap().effective_model,
            "effective/model"
        );
    }

    #[test]
    fn status_follows_request_attempts_and_returns_to_selected_model() {
        let mut app = App::new("original/model");
        let user = ChatMessage::user("라우팅: 사용자 요청");
        app.messages.push(user.clone());
        for reason in [
            "disabled",
            "all_roles_inherit_original",
            "all_roles_match_original_model",
        ] {
            app.receive(ConversationEvent::RoutingDecision(
                dume_worker::routing::RoutingDecision {
                    original_model: app.model.clone(),
                    effective_model: app.model.clone(),
                    phase: dume_store::RoutingPhase::Fixed,
                    epoch: None,
                    reason: reason.into(),
                    classifier_usage: None,
                },
            ));
            assert_eq!(app.messages, vec![user.clone()]);
        }
        app.begin_run("run".into());
        app.is_busy = true;
        assert_eq!(app.status_model(), "original/model");
        for (phase, model) in [
            (dume_store::RoutingPhase::Planning, "planning/model"),
            (
                dume_store::RoutingPhase::Implementation,
                "implementation/model",
            ),
        ] {
            app.receive(ConversationEvent::RoutingDecision(
                dume_worker::routing::RoutingDecision {
                    original_model: "original/model".into(),
                    effective_model: model.into(),
                    phase,
                    epoch: Some("epoch".into()),
                    reason: "complexity_complex".into(),
                    classifier_usage: None,
                },
            ));
            assert_eq!(app.status_model(), model);
            app.receive(ConversationEvent::Stream(StreamEvent::AttemptStarted {
                context: dume_provider::types::StreamRequestContext::new(),
                model: format!("{model}-actual"),
            }));
            assert_eq!(app.status_model(), format!("{model}-actual"));
            assert_eq!(app.model, "original/model");
        }
        // A new selection during a run must not hide the model already executing.
        app.model = "selected/next".into();
        assert_eq!(app.status_model(), "implementation/model-actual");
        app.receive(ConversationEvent::Finished {
            run_id: Some("run".into()),
            messages: vec![
                user.clone(),
                ChatMessage::ui_notice("라우팅: 오래된 정상 안내"),
            ],
            error: None,
            session_id: None,
        });
        assert_eq!(app.status_model(), "selected/next");
        assert_eq!(app.messages, vec![user]);
        app.begin_run("next-run".into());
        app.is_busy = true;
        assert_eq!(app.status_model(), "selected/next");
    }

    #[test]
    fn routing_snapshot_preserves_epochs_only_for_actual_resumable_run_statuses() {
        let settings = dume_worker::RoutingSettings::default();
        for status in [Some("interrupted"), Some("needs_attention")] {
            let snapshot =
                dume_worker::routing::RoutingSnapshot::from_settings(settings.clone(), false);
            let snapshot = routing_snapshot_for_run(snapshot, status);
            assert!(format!("{snapshot:?}").contains("preserve_saved_epoch: true"));
        }
        for status in [
            None,
            Some("queued"),
            Some("running"),
            Some("succeeded"),
            Some("failed"),
        ] {
            let snapshot =
                dume_worker::routing::RoutingSnapshot::from_settings(settings.clone(), true);
            let snapshot = routing_snapshot_for_run(snapshot, status);
            assert!(format!("{snapshot:?}").contains("preserve_saved_epoch: false"));
        }
    }

    #[test]
    fn finished_keeps_routing_failure_warning_without_changing_model_history() {
        let mut app = App::new("original/model");
        app.current_run_id = Some("run-routing".into());
        app.is_busy = true;
        let user = ChatMessage::user("request");
        let history_before = model_messages(std::slice::from_ref(&user));
        app.receive(ConversationEvent::RoutingDecision(
            dume_worker::routing::RoutingDecision {
                original_model: "original/model".into(),
                effective_model: "original/model".into(),
                phase: dume_store::RoutingPhase::Fixed,
                epoch: Some("epoch-current".into()),
                reason: "transport".into(),
                classifier_usage: Some(dume_provider::classifier::ClassifierUsage {
                    input_tokens: 4,
                    output_tokens: 2,
                }),
            },
        ));
        for error in [None, Some("provider failed".to_string())] {
            app.receive(ConversationEvent::Finished {
                run_id: Some("run-routing".into()),
                messages: vec![user.clone()],
                error: error.clone(),
                session_id: Some("session".into()),
            });
            assert!(
                app.messages
                    .iter()
                    .any(|message| message.content.contains("분류기 서버에 연결할 수 없어"))
            );
            assert_eq!(
                app.messages
                    .iter()
                    .filter(|message| message.content.contains("분류기 서버에 연결할 수 없어"))
                    .count(),
                1
            );
            assert_eq!(model_messages(&app.messages), history_before);
            assert_eq!(app.status_model(), "original/model");
            if error.is_some() {
                assert!(
                    app.messages
                        .iter()
                        .any(|message| message.content.contains("Error: provider failed"))
                );
            }
        }
    }

    #[test]
    fn normal_routing_clears_old_warning_and_stale_finish_cannot_reverse_it() {
        let mut app = App::new("original/model");
        app.current_run_id = Some("run-current".into());
        app.is_busy = true;
        let decision =
            |effective: &str, epoch: Option<&str>, reason: &str| dume_worker::routing::RoutingDecision {
                original_model: "original/model".into(),
                effective_model: effective.into(),
                phase: dume_store::RoutingPhase::Fixed,
                epoch: epoch.map(str::to_string),
                reason: reason.into(),
                classifier_usage: None,
            };
        app.receive(ConversationEvent::RoutingDecision(decision(
            "old/model",
            Some("old"),
            "transport",
        )));
        assert_eq!(app.messages.len(), 1);
        app.receive(ConversationEvent::RoutingDecision(decision(
            "original/model",
            None,
            "disabled",
        )));
        app.receive(ConversationEvent::Finished {
            run_id: Some("run-stale".into()),
            messages: vec![ChatMessage::user("stale")],
            error: None,
            session_id: None,
        });
        assert!(app.messages.is_empty());
        assert!(app.is_busy);
        assert_eq!(app.status_model(), "original/model");
        assert_eq!(app.last_routing_decision.as_ref().unwrap().epoch, None);
    }

    #[tokio::test]
    async fn btw_retry_keeps_failed_usage_but_only_returns_successful_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            HarnessStore::open(dir.path().join("harness.db"), &dir.path().join("artifacts"))
                .unwrap(),
        );
        store
            .create_conversation("primary", "/tmp", "fake/model", "primary-provider-session")
            .unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let reply = run_btw_request(
            "fake/model",
            "/tmp",
            "primary",
            vec![("user".into(), "question".into())],
            store.clone(),
            CancellationToken::new(),
            tx,
            |_, _, context, events| async move {
                let send = |event| events.send(event);
                send(StreamEvent::AttemptStarted {
                    context: context.clone(),
                    model: "fake/model".into(),
                })
                .await
                .unwrap();
                send(StreamEvent::TextDelta("provisional".into()))
                    .await
                    .unwrap();
                send(StreamEvent::Usage(dume_provider::types::TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    total_tokens: 5,
                    ..Default::default()
                }))
                .await
                .unwrap();
                send(StreamEvent::AttemptFinished {
                    identity: Default::default(),
                    context: context.clone(),
                    model: "fake/model".into(),
                    outcome: dume_provider::types::AttemptOutcome::Failed,
                    duration_ms: 10,
                    usage: Some(dume_provider::types::TokenUsage {
                        input_tokens: 3,
                        output_tokens: 2,
                        total_tokens: 5,
                        ..Default::default()
                    }),
                    error: Some("retry".into()),
                })
                .await
                .unwrap();
                send(StreamEvent::Retrying {
                    next_attempt: 2,
                    delay_ms: 0,
                    reason: "retry".into(),
                })
                .await
                .unwrap();
                let retry_context = dume_provider::types::StreamRequestContext::new();
                send(StreamEvent::AttemptStarted {
                    context: retry_context.clone(),
                    model: "fake/model".into(),
                })
                .await
                .unwrap();
                send(StreamEvent::TextDelta("answer".into())).await.unwrap();
                send(completed("stop")).await.unwrap();
                send(StreamEvent::AttemptFinished {
                    identity: Default::default(),
                    context: retry_context,
                    model: "fake/model".into(),
                    outcome: dume_provider::types::AttemptOutcome::Succeeded,
                    duration_ms: 10,
                    usage: None,
                    error: None,
                })
                .await
                .unwrap();
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(reply, "answer");
        while rx.try_recv().is_ok() {}
        let metrics = store.conversation_runtime_metrics("btw:primary").unwrap();
        assert_eq!(metrics.model_attempts.len(), 2);
        assert_eq!(metrics.model_attempts[0].state, "failed");
        assert!(metrics.model_attempts[0].usage.is_some());
        assert_eq!(metrics.runs[0].status, "succeeded");
        assert!(
            store
                .conversation_runtime_metrics("primary")
                .unwrap()
                .runs
                .is_empty()
        );
        let saved = store.conversation_history("btw:primary").unwrap();
        assert_eq!(saved.len(), 2);
        let user: ChatMessage = serde_json::from_str(&saved[0].message_json).unwrap();
        assert_eq!(user, ChatMessage::user("question"));
        let assistant: ChatMessage = serde_json::from_str(&saved[1].message_json).unwrap();
        assert_eq!(assistant.content, "answer");
    }

    #[tokio::test]
    async fn btw_cancellation_is_joined_and_retains_checkpointed_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            HarnessStore::open(dir.path().join("harness.db"), &dir.path().join("artifacts"))
                .unwrap(),
        );
        store
            .create_conversation("primary", "/tmp", "fake/model", "primary-provider-session")
            .unwrap();
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task_store = store.clone();
        let task = tokio::spawn(async move {
            let (tx, _rx) = mpsc::channel(32);
            run_btw_request(
                "fake/model",
                "/tmp",
                "primary",
                vec![("user".into(), "question".into())],
                task_store,
                task_cancel,
                tx,
                |_, _, context, events| async move {
                    events
                        .send(StreamEvent::AttemptStarted {
                            context,
                            model: "fake/model".into(),
                        })
                        .await
                        .unwrap();
                    events
                        .send(StreamEvent::Usage(dume_provider::types::TokenUsage {
                            input_tokens: 7,
                            output_tokens: 4,
                            total_tokens: 11,
                            ..Default::default()
                        }))
                        .await
                        .unwrap();
                    std::future::pending::<Result<()>>().await
                },
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancellation.cancel();
        assert!(task.await.unwrap().is_err());
        let metrics = store.conversation_runtime_metrics("btw:primary").unwrap();
        assert_eq!(metrics.runs[0].status, "cancelled");
        assert_eq!(metrics.model_attempts.len(), 1);
        let usage = metrics.model_attempts[0].usage.as_ref().unwrap();
        assert_eq!(usage["input_tokens"].as_i64(), Some(7));
        assert_eq!(usage["output_tokens"].as_i64(), Some(4));
        assert!(
            store
                .conversation_runtime_metrics("primary")
                .unwrap()
                .runs
                .is_empty()
        );
    }

    #[test]
    fn credential_changes_rotate_session_identity_only_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("credentials.json"));
        let mut session_id = "before-login".to_string();

        assert!(
            save_credential_and_rotate_session(&store, "openai", "test-key", &mut session_id,)
                .is_ok()
        );
        assert_ne!(session_id, "before-login");
        assert!(store.has_credential("openai"));

        let logged_in_session = session_id.clone();
        assert!(
            save_credential_and_rotate_session(
                &store,
                "openai-codex",
                "invalid-api-key",
                &mut session_id,
            )
            .is_err()
        );
        assert_eq!(session_id, logged_in_session);

        assert!(delete_credential_and_rotate_session(&store, "openai", &mut session_id,).is_ok());
        assert_ne!(session_id, logged_in_session);
        assert!(!store.has_credential("openai"));
    }

    #[test]
    fn subagent_picker_saves_and_reopens_without_changing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent/subagents.json");
        let models = vec![dume_provider::resolve_model("openai/gpt-5.4").unwrap()];
        let mut app = App::new("parent/model");
        app.thinking = ThinkingLevel::High;
        let session_id = app.session_id.clone();
        app.modal = ModalState::subagent_model_selector(models.clone(), path.clone()).unwrap();
        let ModalState::ModelSelector { selected_idx, .. } = &mut app.modal else {
            panic!("expected model picker");
        };
        assert_eq!(*selected_idx, 0);
        *selected_idx = 1;
        app.confirm_model_selection().unwrap();
        assert!(matches!(app.modal, ModalState::None));
        assert_eq!(
            SubagentSettings::load_from(&path).unwrap().model.as_deref(),
            Some("openai/gpt-5.4")
        );
        assert_eq!(app.model, "parent/model");
        assert_eq!(app.session_id, session_id);
        assert_eq!(app.thinking, ThinkingLevel::High);

        app.modal = ModalState::subagent_model_selector(models, path.clone()).unwrap();
        let ModalState::ModelSelector { selected_idx, .. } = &app.modal else {
            panic!("expected model picker");
        };
        assert_eq!(*selected_idx, 1);
        app.modal = ModalState::None;
        assert_eq!(
            SubagentSettings::load_from(&path).unwrap().model.as_deref(),
            Some("openai/gpt-5.4")
        );

        app.modal = ModalState::subagent_model_selector(Vec::new(), path.clone()).unwrap();
        app.confirm_model_selection().unwrap();
        assert!(SubagentSettings::load_from(&path).unwrap().model.is_none());
        assert_eq!(app.model, "parent/model");
        assert_eq!(app.session_id, session_id);
    }

    #[test]
    fn subagent_picker_preserves_settings_and_stays_open_on_save_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagents.json");
        let original = b"{\"model\":\"openai/gpt-5.4\"}";
        std::fs::write(&path, original).unwrap();
        let mut app = App::new("parent/model");
        app.modal = ModalState::subagent_model_selector(Vec::new(), path.clone()).unwrap();
        std::fs::create_dir(path.with_extension(format!("tmp-{}", std::process::id()))).unwrap();
        assert!(app.confirm_model_selection().is_err());
        assert!(matches!(app.modal, ModalState::ModelSelector { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        std::fs::write(&path, b"invalid JSON").unwrap();
        assert!(ModalState::subagent_model_selector(Vec::new(), path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid JSON");
    }

    #[test]
    fn subagent_command_persists_model_and_rejects_invalid_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagents.json");

        subagent_command("/subagents model openai/gpt-5.4", &path).unwrap();
        assert_eq!(
            SubagentSettings::load_from(&path).unwrap().model.as_deref(),
            Some("openai/gpt-5.4")
        );
        assert!(subagent_command("/subagents model no-such-model", &path).is_err());
        assert_eq!(
            SubagentSettings::load_from(&path).unwrap().model.as_deref(),
            Some("openai/gpt-5.4")
        );

        subagent_command("/subagents model inherit", &path).unwrap();
        assert!(SubagentSettings::load_from(&path).unwrap().model.is_none());
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

    #[tokio::test]
    async fn durable_cached_and_failed_tools_reach_the_next_model_payload() {
        for cached in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store =
                Arc::new(HarnessStore::in_memory(directory.path().join("artifacts")).unwrap());
            store
                .create_conversation(
                    "durable",
                    directory.path().to_str().unwrap(),
                    "test",
                    "session",
                )
                .unwrap();
            let scope = RuntimeScope::Conversation("durable".into());
            let receipt = store
                .admit_input(scope.clone(), "request", InputKind::Submit, "read")
                .unwrap();
            let session = RunSession::start(
                store.clone(),
                scope,
                receipt.run_id.as_deref().unwrap(),
                "durable",
            )
            .unwrap();
            let arguments = r#"{"path":"missing.txt"}"#;
            if cached {
                let mut turn = StreamTurn::default();
                turn.push(&delta(0, Some("read-once"), Some("read_file"), arguments))
                    .unwrap();
                turn.push(&completed("tool_calls")).unwrap();
                let (assistant, _) = turn.finish().unwrap();
                let execution = session.prepare_tools(&assistant).unwrap().remove(0);
                let ToolStart::Execute { attempt_id } = session.begin_tool(&execution).unwrap()
                else {
                    panic!("new execution expected")
                };
                session
                    .finish_tool(
                        &execution,
                        &attempt_id,
                        &ChatMessage::tool("saved reply", "read-once"),
                        0,
                    )
                    .unwrap();
            }
            let mut dispatcher =
                ToolDispatcher::new(directory.path(), "test", CancellationToken::new())
                    .with_store(store.clone());
            let mut messages = vec![ChatMessage::user("read")];
            let mut requests = Vec::new();
            let mut provider_session = "session".to_string();
            let (tx, _rx) = mpsc::channel(100);
            run_conversation(
                &mut messages,
                &tx,
                &mut dispatcher,
                &mut provider_session,
                &CancellationToken::new(),
                Some(&session),
                Some(3),
                None,
                "test",
                |history, _, context, effective_model, events| {
                    requests.push(history.clone());
                    let first = requests.len() == 1;
                    let context = dume_provider::types::StreamRequestContext::for_session(context);
                    async move {
                        events
                            .send(StreamEvent::AttemptStarted {
                                context: context.clone(),
                                model: "test".into(),
                            })
                            .await
                            .unwrap();
                        if first {
                            events
                                .send(delta(0, Some("read-once"), Some("read_file"), arguments))
                                .await
                                .unwrap();
                            events.send(completed("tool_calls")).await.unwrap();
                        } else {
                            events
                                .send(StreamEvent::TextDelta("finished".into()))
                                .await
                                .unwrap();
                            events.send(completed("stop")).await.unwrap();
                        }
                        events
                            .send(StreamEvent::AttemptFinished {
                                identity: Default::default(),
                                context,
                                model: "test".into(),
                                outcome: dume_provider::AttemptOutcome::Succeeded,
                                duration_ms: 0,
                                usage: None,
                                error: None,
                            })
                            .await
                            .unwrap();
                        Ok(PreparedRequest {
                            messages: history,
                            effective_model,
                        })
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(requests.len(), 2);
            let reply = &requests[1][2];
            assert_eq!(reply.role, Role::Tool);
            assert_eq!(reply.tool_call_id.as_deref(), Some("read-once"));
            if cached {
                assert_eq!(reply.content, "saved reply");
            } else {
                assert!(reply.content.contains("Tool error:"));
            }
            let executions = store
                .tool_executions_for_run(&session.token().run_id)
                .unwrap();
            assert_eq!(executions.len(), 1);
            assert_eq!(
                executions[0].state,
                if cached { "succeeded" } else { "failed" }
            );
            assert_eq!(
                store.get_run(&session.token().run_id).unwrap().status,
                "succeeded"
            );
            let saved = store
                .conversation_history("durable")
                .unwrap()
                .into_iter()
                .map(|row| serde_json::from_str::<ChatMessage>(&row.message_json).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(saved, messages);
            assert!(!directory.path().join("missing.txt").exists());
        }
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
        let mut session_id = String::from("test-session");
        let result = run_conversation(
            messages,
            &tx,
            dispatcher,
            &mut session_id,
            &CancellationToken::new(),
            None,
            Some(max_turns),
            None,
            "test",
            |history, _, _, effective_model, tx| {
                requests.push(history.clone());
                let (events, fail) = scripts.pop_front().expect("unexpected model request");
                async move {
                    for event in events {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    anyhow::ensure!(!fail, "provider request failed");
                    Ok(PreparedRequest {
                        messages: history,
                        effective_model,
                    })
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
    async fn producer_error_drains_final_attempt_identity_before_ending_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(directory.path().join("artifacts")).unwrap());
        store
            .create_conversation("identity-test", "/tmp", "test", "session")
            .unwrap();
        let scope = RuntimeScope::Conversation("identity-test".into());
        let receipt = store
            .admit_input(scope.clone(), "input", InputKind::Submit, "fixture")
            .unwrap();
        let session = RunSession::start(
            store.clone(),
            scope,
            receipt.run_id.as_deref().unwrap(),
            "identity-test",
        )
        .unwrap();
        let mut dispatcher =
            ToolDispatcher::new(directory.path(), "test", CancellationToken::new())
                .with_store(store.clone());
        let (tx, _rx) = mpsc::channel(100);
        let mut messages = vec![ChatMessage::user("fixture")];
        let result = run_conversation(
            &mut messages,
            &tx,
            &mut dispatcher,
            &mut "session".to_string(),
            &CancellationToken::new(),
            Some(&session),
            Some(1),
            None,
            "test",
            |_, _, _, _, events| async move {
                let context = dume_provider::types::StreamRequestContext::from_ids(
                    "session",
                    "failed-identity",
                );
                events
                    .send(StreamEvent::AttemptStarted {
                        context: context.clone(),
                        model: "test".into(),
                    })
                    .await
                    .unwrap();
                events
                    .send(delta(
                        0,
                        Some("write"),
                        Some("write_file"),
                        r#"{"path":"blocked.txt","content":"bad"}"#,
                    ))
                    .await
                    .unwrap();
                events
                    .send(StreamEvent::AttemptFinished {
                        context,
                        model: "test".into(),
                        outcome: dume_provider::AttemptOutcome::Failed,
                        duration_ms: 1,
                        usage: None,
                        identity: dume_provider::types::ResponseIdentity {
                            model: Some("other".into()),
                            response_id: Some("response".into()),
                        },
                        error: Some("Provider rejected requested model".into()),
                    })
                    .await
                    .unwrap();
                // All sends complete in one poll; the producer error must not discard queued evidence.
                anyhow::bail!("Provider rejected requested model");
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("Provider rejected requested model"));
        let metrics = store.conversation_runtime_metrics("identity-test").unwrap();
        assert_eq!(metrics.model_attempts[0].state, "failed");
        assert_eq!(
            metrics.model_attempts[0].usage.as_ref().unwrap()["response_identity"]["model"],
            "other"
        );
        assert!(metrics.tool_executions.is_empty());
        assert!(!directory.path().join("blocked.txt").exists());
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
        assert!(!app.streaming_text.contains("[Tool Call:"));
        assert!(!app.streaming_text.contains("opaque-secret"));
        assert!(app.active_tools.iter().any(|t| t.name == "write_file"));
        assert!(app.active_tools.iter().any(|t| t.name == "read_file"));
        app.receive(ConversationEvent::Finished {
            run_id: None,
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
            run_id: None,
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

    #[test]
    fn retrying_discards_provisional_text_and_tool_calls() {
        let mut turn = StreamTurn::default();
        turn.push(&StreamEvent::TextDelta("discard me".into()))
            .unwrap();
        turn.push(&delta(0, Some("old"), Some("write_file"), "{}"))
            .unwrap();
        turn.push(&StreamEvent::Retrying {
            next_attempt: 2,
            delay_ms: 1,
            reason: "temporary".into(),
        })
        .unwrap();
        turn.push(&StreamEvent::TextDelta("kept".into())).unwrap();
        turn.push(&completed("stop")).unwrap();
        let (assistant, calls) = turn.finish().unwrap();
        assert_eq!(assistant.content, "kept");
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn cancellation_drops_live_stream_without_executing_pending_calls() {
        let dir = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let mut dispatcher = ToolDispatcher::new(dir.path(), "test", cancellation.clone());
        let mut messages = vec![ChatMessage::user("write")];
        let (tx, _rx) = mpsc::channel(100);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut session_id = String::from("test-session");
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
            &mut session_id,
            &cancellation,
            None,
            Some(10),
            None,
            "test",
            |_, _, _, _, tx| {
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
                    std::future::pending::<Result<PreparedRequest>>().await
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
        assert_eq!(
            payload[0].content,
            dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT
        );
        assert_eq!(payload[1].content, "First request");
        assert_eq!(payload[2].content, "Second request");
        assert_eq!(model_messages(&history[..2]), payload[..2]);
    }

    #[test]
    fn stale_finished_event_does_not_replace_current_run_state() {
        let mut app = App::new("test");
        app.current_run_id = Some("run-new".into());
        app.is_busy = true;
        app.messages = vec![ChatMessage::user("current transcript")];
        app.receive(ConversationEvent::Finished {
            run_id: Some("run-old".into()),
            messages: vec![ChatMessage::assistant("stale response")],
            error: None,
            session_id: None,
        });
        assert!(app.is_busy);
        assert_eq!(app.messages, vec![ChatMessage::user("current transcript")]);
    }

    #[test]
    fn explicit_run_resume_selects_saved_paused_run_without_admitting_another_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::open(dir.path().join("resume.db"), &dir.path().join("artifacts"))
            .unwrap();
        store
            .create_conversation("resume-conversation", "/tmp", "model", "session")
            .unwrap();
        let scope = RuntimeScope::Conversation("resume-conversation".into());
        let receipt = store
            .admit_input(
                scope.clone(),
                "original-request",
                InputKind::Submit,
                "saved marker",
            )
            .unwrap();
        let run_id = receipt.run_id.unwrap();
        let token = store.start_run(scope.clone(), &run_id).unwrap();
        store
            .settle_run(&token, "interrupted", Some("restart"), 10)
            .unwrap();

        let selected = find_resumable_run(&store, "resume-conversation", "").unwrap();
        assert_eq!(selected.run_id, run_id);
        assert_eq!(
            store
                .conversation_history("resume-conversation")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.inbox(&scope, &[]).unwrap().len(), 1);
        assert!(
            find_resumable_run(&store, "resume-conversation", "other-conversation-run").is_err()
        );
    }

    #[test]
    fn session_usage_refreshes_from_persisted_attempts_and_reports_missing_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            HarnessStore::open(dir.path().join("usage.db"), &dir.path().join("artifacts")).unwrap();
        store
            .create_conversation("usage-conversation", "/tmp", "model", "session")
            .unwrap();
        let scope = RuntimeScope::Conversation("usage-conversation".into());
        let receipt = store
            .admit_input(scope.clone(), "usage-request", InputKind::Submit, "prompt")
            .unwrap();
        let token = store.start_run(scope, &receipt.run_id.unwrap()).unwrap();
        store
            .start_model_attempt(&token, "attempt-1", "session", "model")
            .unwrap();
        store.finish_model_attempt("attempt-1", dume_store::ModelAttemptOutcome {
            state: "succeeded".into(), reason: None,
            usage: Some(serde_json::json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15})),
            timing_complete: true,
        }, 1).unwrap();
        store
            .start_model_attempt(&token, "attempt-2", "session", "model")
            .unwrap();
        store
            .finish_model_attempt(
                "attempt-2",
                dume_store::ModelAttemptOutcome {
                    state: "failed".into(),
                    reason: Some("missing provider usage".into()),
                    usage: None,
                    timing_complete: false,
                },
                1,
            )
            .unwrap();
        let mut app = App::new("model");
        app.conversation_id = "usage-conversation".into();
        refresh_session_timing(&mut app, &store).unwrap();
        assert_eq!(app.session_usage.0, 10);
        assert_eq!(app.session_usage.1, 5);
        assert_eq!(app.session_usage.2, 15);
        assert_eq!(app.session_usage_incomplete, 1);
    }

    #[test]
    fn durable_inputs_preserve_fifo_and_pause_when_run_needs_attention() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            HarnessStore::open(dir.path().join("queue.db"), &dir.path().join("artifacts")).unwrap();
        store
            .create_conversation("queue-test", "/tmp", "model", "session")
            .unwrap();
        let scope = RuntimeScope::Conversation("queue-test".into());
        let active = store
            .admit_input(scope.clone(), "first", InputKind::Submit, "first prompt")
            .unwrap();
        let run_id = active.run_id.unwrap();
        let token = store.start_run(scope.clone(), &run_id).unwrap();
        store
            .settle_run(&token, "needs_attention", Some("operator action"), 1)
            .unwrap();
        let first = store
            .admit_input(scope.clone(), "queued-1", InputKind::FollowUp, "one")
            .unwrap();
        let second = store
            .admit_input(scope.clone(), "queued-2", InputKind::FollowUp, "two")
            .unwrap();
        assert_eq!(first.disposition, "pending");
        assert_eq!(second.disposition, "pending");
        let rows = store.inbox(&scope, &["paused"]).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.payload.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert!(
            store
                .active_run(&scope)
                .unwrap()
                .is_some_and(|run| run.status == "needs_attention")
        );
    }

    #[tokio::test]
    async fn provider_failure_after_compaction_keeps_new_session_and_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(dir.path().join("artifacts")).unwrap());
        store
            .create_conversation(
                "pipeline-test",
                "/tmp",
                "unknown-model",
                "before-compaction",
            )
            .unwrap();
        let receipt = store
            .admit_input(
                RuntimeScope::Conversation("pipeline-test".into()),
                "pipeline-request",
                InputKind::Submit,
                "Preserve the public API.",
            )
            .unwrap();
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
        let run_id = receipt.run_id.unwrap();
        dispatch_stream(
            "unknown-model",
            original_session.clone(),
            history,
            tx,
            dispatcher,
            CancellationToken::new(),
            store.clone(),
            "pipeline-test".to_string(),
            run_id.clone(),
            dume_worker::routing::RoutingSnapshot::load_default(false),
        )
        .await;
        let finished = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("conversation event receive timed out")
            {
                Some(event @ ConversationEvent::Finished { .. }) => break event,
                Some(ConversationEvent::RoutingDecision(_)) => continue,
                Some(_) => continue,
                None => panic!("conversation event channel closed before completion"),
            }
        };
        let ConversationEvent::Finished {
            run_id: Some(_),
            mut messages,
            error: Some(error),
            session_id: Some(session_id),
        } = finished
        else {
            panic!("expected a failed conversation with a recoverable session");
        };
        assert!(
            error.contains("Unknown") || error.contains("unknown"),
            "{error}"
        );
        let metrics = store.conversation_runtime_metrics("pipeline-test").unwrap();
        assert!(
            metrics
                .runs
                .iter()
                .any(|run| run.run_id == run_id && run.status == "failed")
        );
        assert_ne!(session_id, original_session);
        assert!(messages.iter().any(|message| {
            message.content.contains("Conversation checkpoint")
                && message.content.contains("Preserve the public API")
        }));
        assert!(
            messages
                .iter()
                .all(|message| !message.content.contains(&"x".repeat(100)))
        );

        messages.push(ChatMessage::assistant("y".repeat(500_000)));
        messages.push(ChatMessage::user("Try again."));
        assert!(
            dume_worker::context::prepare_request_context(dume_worker::context::RequestContext {
                model: "unknown-model",
                session_id: &session_id,
                messages: &mut messages,
                tools: &[],
                store: Some(&store),
            },)
            .is_ok()
        );
    }

    #[test]
    fn test_repair_conversation_history_clean_complete() {
        let mut messages = vec![
            ChatMessage::user("Hello"),
            ChatMessage::assistant_with_tool_calls(
                "Let me run a command",
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "run_command".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
            ChatMessage::tool("Command output", "call_1"),
            ChatMessage::assistant("All done!"),
        ];

        let (repaired, interrupted) = repair_conversation_history(&mut messages);
        assert!(!repaired);
        assert!(interrupted.is_none());
        assert_eq!(messages.len(), 4);
    }

    #[test]
    fn test_repair_conversation_history_synthesizes_missing_tool_responses() {
        let mut messages = vec![
            ChatMessage::user("Build and test"),
            ChatMessage::assistant_with_tool_calls(
                "Running tools",
                vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        name: "cargo_check".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        name: "cargo_test".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
            ),
            ChatMessage::tool("Check ok", "call_1"),
            // call_2 is missing because process was forcefully killed
        ];

        let (repaired, interrupted) = repair_conversation_history(&mut messages);
        assert!(repaired);
        assert!(interrupted.is_none());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_2"));
        assert!(messages[3].content.contains("Interrupted"));
    }

    #[test]
    fn test_repair_conversation_history_pops_interrupted_user_prompt() {
        let mut messages = vec![
            ChatMessage::user("First prompt"),
            ChatMessage::assistant("First answer"),
            ChatMessage::user("Second interrupted prompt"),
        ];

        let (repaired, interrupted) = repair_conversation_history(&mut messages);
        assert!(repaired);
        assert_eq!(interrupted.as_deref(), Some("Second interrupted prompt"));
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_conversation_file_lock_mutual_exclusion() {
        let conv_id = format!(
            "test_lock_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let lock1 =
            ConversationLock::try_acquire(&conv_id).expect("First lock acquire must succeed");
        let lock2_res = ConversationLock::try_acquire(&conv_id);
        assert!(
            lock2_res.is_err(),
            "Second lock acquire must fail while held"
        );

        drop(lock1);
        let lock3_res = ConversationLock::try_acquire(&conv_id);
        assert!(
            lock3_res.is_ok(),
            "Lock acquire must succeed after previous lock is dropped"
        );
    }

    #[test]
    fn test_detect_interrupted_session_notice() {
        let temp_dir = tempfile::tempdir().unwrap();
        let artifacts_dir = temp_dir.path().join("artifacts");
        let store = HarnessStore::open(temp_dir.path().join("test.db"), &artifacts_dir).unwrap();
        let cwd = "/test/repo";

        // No sessions initially
        assert!(detect_interrupted_session_notice(&store, cwd, "current_1").is_none());

        // Create an interrupted session with messages
        store
            .create_conversation("prev_1", cwd, "model", "sess")
            .unwrap();
        let user_msg = ChatMessage::user("Fix build error in main.rs");
        let json = serde_json::to_string(&user_msg).unwrap();
        store.append_conversation_message("prev_1", &json).unwrap();
        store
            .set_conversation_status("prev_1", "interrupted")
            .unwrap();

        let notice = detect_interrupted_session_notice(&store, cwd, "current_1");
        assert!(notice.is_some());
        let text = notice.unwrap();
        assert!(text.contains("prev_1"));
        assert!(text.contains("Fix build error in main.rs"));
        assert!(text.contains("/resume"));

        // When previous session is closed, notice is None
        store.set_conversation_status("prev_1", "closed").unwrap();
        assert!(detect_interrupted_session_notice(&store, cwd, "current_1").is_none());
    }

    #[test]
    fn timing_format_and_terminal_outcomes_are_explicit() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(999), "1s");
        assert_eq!(format_duration(60_000), "1m0s");
        assert_eq!(status_label("succeeded"), "DONE");
        assert_eq!(status_label("failed"), "ERROR");
        assert_eq!(status_label("cancelled"), "CANCELLED");
        assert_eq!(status_label("needs_attention"), "ATTENTION");
        assert_eq!(status_label("unknown"), "RUN");

        let metrics = dume_store::ConversationRuntimeMetrics {
            runs: vec![
                dume_store::RunRecord {
                    run_id: "failed-run".into(),
                    scope: RuntimeScope::Conversation("c".into()),
                    status: "failed".into(),
                    owner_generation: 1,
                    reason: None,
                    started_at: 0,
                    ended_at: Some(0),
                    elapsed_ms: 0,
                    timing_complete: false,
                },
                dume_store::RunRecord {
                    run_id: "cancelled-run".into(),
                    scope: RuntimeScope::Conversation("c".into()),
                    status: "cancelled".into(),
                    owner_generation: 1,
                    reason: None,
                    started_at: 1,
                    ended_at: Some(1),
                    elapsed_ms: 1_500,
                    timing_complete: true,
                },
            ],
            model_attempts: vec![],
            tool_executions: vec![],
            tool_attempts: vec![],
        };
        let summary = format_session_metrics(&metrics);
        assert!(summary.contains("ERROR 0s (lower bound)"));
        assert!(summary.contains("CANCELLED 2s"));
    }

    #[tokio::test]
    async fn test_run_conversation_unlimited_turns_exceeds_ten() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(directory.path().join("artifacts")).unwrap());
        let mut dispatcher =
            ToolDispatcher::new(directory.path(), "test", CancellationToken::new())
                .with_store(store);
        let mut messages = vec![ChatMessage::user("run 12 steps")];
        let mut session_id = "test-session".to_string();
        let (tx, _rx) = mpsc::channel(100);
        let turn_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tc = turn_counter.clone();

        let result = run_conversation(
            &mut messages,
            &tx,
            &mut dispatcher,
            &mut session_id,
            &CancellationToken::new(),
            None,
            None,
            None,
            "test",
            move |history, _tools, _session, effective_model, tx| {
                let turn = tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if turn < 12 {
                        let _ = tx
                            .send(delta(
                                0,
                                Some(&format!("call_{turn}")),
                                Some("bash"),
                                "{\"command\":\"echo step\"}",
                            ))
                            .await;
                        let _ = tx.send(completed("tool_calls")).await;
                    } else {
                        let _ = tx
                            .send(StreamEvent::TextDelta("Finished 12 steps".into()))
                            .await;
                        let _ = tx.send(completed("stop")).await;
                    }
                    Ok(PreparedRequest {
                        messages: history,
                        effective_model,
                    })
                }
            },
        )
        .await;

        assert!(
            result.is_ok(),
            "run_conversation must succeed beyond 10 turns without stopping: {:?}",
            result.err()
        );
        assert_eq!(turn_counter.load(std::sync::atomic::Ordering::SeqCst), 13);
        assert_eq!(messages.last().unwrap().content, "Finished 12 steps");
    }

    #[tokio::test]
    async fn test_run_conversation_cancellation_during_execution() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(HarnessStore::in_memory(directory.path().join("artifacts")).unwrap());
        let mut dispatcher =
            ToolDispatcher::new(directory.path(), "test", CancellationToken::new())
                .with_store(store);
        let mut messages = vec![ChatMessage::user("cancel test")];
        let mut session_id = "test-session".to_string();
        let (tx, _rx) = mpsc::channel(100);
        let cancellation = CancellationToken::new();
        let cancel_trigger = cancellation.clone();
        let turn_counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let result = run_conversation(
            &mut messages,
            &tx,
            &mut dispatcher,
            &mut session_id,
            &cancellation,
            None,
            None,
            None,
            "test",
            move |history, _tools, _session, effective_model, tx| {
                let turn = turn_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let cancel_trigger = cancel_trigger.clone();
                async move {
                    if turn == 2 {
                        cancel_trigger.cancel();
                    }
                    let _ = tx
                        .send(delta(
                            0,
                            Some("call"),
                            Some("bash"),
                            "{\"command\":\"echo step\"}",
                        ))
                        .await;
                    let _ = tx.send(completed("tool_calls")).await;
                    Ok(PreparedRequest {
                        messages: history,
                        effective_model,
                    })
                }
            },
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn test_tool_navigation_and_details_toggle() {
        let mut app = App::new("test-model");
        app.messages = vec![
            ChatMessage::user("do work"),
            ChatMessage::assistant_with_tool_calls(
                "I will read and edit",
                vec![
                    dume_provider::types::ToolCall {
                        id: "tool_1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"app.rs\"}".into(),
                    },
                    dume_provider::types::ToolCall {
                        id: "tool_2".into(),
                        name: "replace_file_content".into(),
                        arguments: "{\"path\":\"lib.rs\"}".into(),
                    },
                ],
            ),
            ChatMessage::tool("content of app.rs", "tool_1").with_duration_ms(15),
            ChatMessage::tool("error: cannot replace", "tool_2")
                .with_duration_ms(30)
                .with_is_error(true),
        ];

        let ids = app.tool_ids();
        assert_eq!(ids, vec!["tool_1".to_string(), "tool_2".to_string()]);

        assert_eq!(app.selected_tool_id, None);

        app.select_next_tool();
        assert_eq!(app.selected_tool_id.as_deref(), Some("tool_1"));

        app.select_next_tool();
        assert_eq!(app.selected_tool_id.as_deref(), Some("tool_2"));

        app.select_next_tool();
        assert_eq!(app.selected_tool_id.as_deref(), Some("tool_2"));

        app.select_prev_tool();
        assert_eq!(app.selected_tool_id.as_deref(), Some("tool_1"));

        app.select_prev_tool();
        assert_eq!(app.selected_tool_id.as_deref(), Some("tool_1"));

        assert!(!app.expanded_tools.contains("tool_1"));
        app.toggle_selected_tool_details();
        assert!(app.expanded_tools.contains("tool_1"));
        app.toggle_selected_tool_details();
        assert!(!app.expanded_tools.contains("tool_1"));

        app.toggle_selected_tool_details();
        let cfg = app.transcript_config();
        assert_eq!(cfg.selected_tool_id, Some("tool_1"));
        assert!(cfg.expanded_tools.contains("tool_1"));
    }

    #[test]
    fn test_reloaded_conversation_preserves_tool_duration_and_failure() {
        let msg = ChatMessage::tool("Error: file not found", "call_abc")
            .with_duration_ms(45)
            .with_is_error(true);
        let serialized = serde_json::to_string(&msg).unwrap();
        assert!(serialized.contains("\"duration_ms\":45"));
        assert!(serialized.contains("\"is_error\":true"));

        let deserialized: ChatMessage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.tool_call_id.as_deref(), Some("call_abc"));
        assert_eq!(deserialized.duration_ms, Some(45));
        assert_eq!(deserialized.is_error, Some(true));
        assert_eq!(deserialized.content, "Error: file not found");
    }

    #[test]
    fn test_select_login_provider_opens_api_key_input() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        let mut app = App::with_auth_store("test-model", store);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        select_login_provider(&mut app, 1, &tx);
        match &app.modal {
            ModalState::ApiKeyInput { provider_id, provider_name, input } => {
                assert_eq!(provider_id, "openai");
                assert_eq!(provider_name, "OpenAI ChatGPT");
                assert!(input.is_empty());
            }
            _ => panic!("expected ApiKeyInput"),
        }
    }

    #[test]
    fn test_select_logout_provider_closes_modal_and_records_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        store.save_credential("openai", "dummy-key").unwrap();
        let mut app = App::with_auth_store("test-model", store.clone());
        app.modal = ModalState::LogoutSelector { selected_idx: 1 };
        let prev_session = app.session_id.clone();
        select_logout_provider(&mut app, 1);
        assert!(matches!(app.modal, ModalState::None));
        assert_ne!(app.session_id, prev_session);
        assert!(app.messages.iter().any(|m| m.content.contains("Logged out from provider 'openai'")));
        assert!(!store.has_credential("openai"));
    }

    #[test]
    fn test_handle_logout_command_empty_opens_modal() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        let mut app = App::with_auth_store("test-model", store);
        handle_logout_command(&mut app, "");
        assert!(matches!(app.modal, ModalState::LogoutSelector { selected_idx: 0 }));
    }

    #[test]
    fn test_handle_logout_command_all_clears_stored_and_standard_providers() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_provider::CredentialStore::new(dir.path().join("auth.json"));
        store.save_credential("openai", "sk-test").unwrap();
        store.save_credential("jev-local-system-one", "custom-token").unwrap();
        let mut app = App::with_auth_store("test-model", store.clone());
        let prev_session = app.session_id.clone();

        handle_logout_command(&mut app, "all");

        assert_ne!(app.session_id, prev_session);
        assert!(!store.has_credential("openai"));
        assert!(!store.has_credential("jev-local-system-one"));
        assert!(app.messages.iter().any(|m| m.content.contains("Logged out from all providers")));
    }

    #[cfg(unix)]
    #[test]
    fn test_handle_logout_command_all_reports_failures_when_store_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("auth.json");
        let store = dume_provider::CredentialStore::new(&store_path);
        store.save_credential("openai", "sk-test").unwrap();

        // Make store directory read-only so write_all tempfile creation / persisting fails
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o500);
        let _ = std::fs::set_permissions(dir.path(), perms.clone());

        let mut app = App::with_auth_store("test-model", store.clone());
        handle_logout_command(&mut app, "all");

        // Restore permissions so tempdir cleanup succeeds
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(dir.path(), perms);

        assert!(app.messages.iter().any(|m| m.content.contains("Failed to log out from") || m.content.contains("failed for:")));
    }
}

pub fn detect_interrupted_session_notice(
    store: &HarnessStore,
    cwd: &str,
    current_conv_id: &str,
) -> Option<String> {
    if let Ok(Some(interrupted)) =
        store.latest_interrupted_conversation_for_cwd(cwd, Some(current_conv_id))
    {
        if let Ok((rec, msg_recs)) = store.load_conversation(&interrupted.id) {
            if !msg_recs.is_empty() {
                let short_id = if rec.id.len() >= 8 {
                    &rec.id[..8]
                } else {
                    &rec.id
                };
                let preview = msg_recs
                    .iter()
                    .filter_map(|m| serde_json::from_str::<ChatMessage>(&m.message_json).ok())
                    .find(|cm| cm.role == dume_provider::Role::User)
                    .map(|cm| {
                        let first_line = cm.content.lines().next().unwrap_or("").trim();
                        if first_line.chars().count() > 40 {
                            let truncated: String = first_line.chars().take(37).collect();
                            format!("\"{truncated}...\"")
                        } else if !first_line.is_empty() {
                            format!("\"{first_line}\"")
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();

                let desc = if preview.is_empty() {
                    format!("id: {short_id}")
                } else {
                    format!("{preview} (id: {short_id})")
                };
                return Some(format!(
                    "Previous interrupted session detected in this repository: {desc}. Type /resume to restore, or continue in this new session."
                ));
            }
        }
    }
    None
}
