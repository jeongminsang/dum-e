use crate::routing_settings::RoutingSettings;
use crate::runtime::RunSession;
use anyhow::{Result, anyhow};
use dume_provider::auth::CredentialStore;
use dume_provider::catalog::ModelCatalog;
use dume_provider::classifier::{ClassifierErrorKind, ClassifierUsage, SystemOneClassifier};
use dume_provider::runtime::resolve_model_with_root;
use dume_provider::types::ChatMessage;
use dume_store::{ConversationRoutingState, RoutingPhase};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct RoutingSnapshot {
    settings: Option<RoutingSettings>,
    invalid_reason: Option<&'static str>,
    preserve_saved_epoch: bool,
    settings_path: Option<PathBuf>,
    models_root: PathBuf,
}

impl std::fmt::Debug for RoutingSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingSnapshot")
            .field(
                "enabled",
                &self.settings.as_ref().is_some_and(|s| s.enabled),
            )
            .field("invalid_reason", &self.invalid_reason)
            .field("preserve_saved_epoch", &self.preserve_saved_epoch)
            .finish()
    }
}

impl RoutingSnapshot {
    pub fn load_default(preserve_saved_epoch: bool) -> Self {
        match RoutingSettings::global_path() {
            Ok(path) => Self::load_at_path(&path, preserve_saved_epoch),
            Err(_) => Self {
                settings: None,
                invalid_reason: Some("invalid_settings"),
                preserve_saved_epoch,
                settings_path: None,
                models_root: ModelCatalog::models_dir(),
            },
        }
    }

    pub fn load(path: &Path, preserve_saved_epoch: bool) -> Self {
        Self::load_at_path(path, preserve_saved_epoch)
    }

    fn load_at_path(path: &Path, preserve_saved_epoch: bool) -> Self {
        let settings = RoutingSettings::load(path);
        match settings {
            Ok(settings) => Self {
                settings: Some(settings),
                invalid_reason: None,
                preserve_saved_epoch,
                settings_path: Some(path.to_path_buf()),
                models_root: ModelCatalog::models_dir(),
            },
            Err(_) => Self {
                settings: None,
                invalid_reason: Some("invalid_settings"),
                preserve_saved_epoch,
                settings_path: Some(path.to_path_buf()),
                models_root: ModelCatalog::models_dir(),
            },
        }
    }

    pub fn from_settings(settings: RoutingSettings, preserve_saved_epoch: bool) -> Self {
        if settings.validate().is_ok() {
            Self {
                settings: Some(settings),
                invalid_reason: None,
                preserve_saved_epoch,
                settings_path: None,
                models_root: ModelCatalog::models_dir(),
            }
        } else {
            Self {
                settings: None,
                invalid_reason: Some("invalid_settings"),
                preserve_saved_epoch,
                settings_path: None,
                models_root: ModelCatalog::models_dir(),
            }
        }
    }

    pub fn with_models_root(mut self, root: PathBuf) -> Self {
        self.models_root = root;
        self
    }

    pub fn with_epoch_preservation(mut self, preserve: bool) -> Self {
        self.preserve_saved_epoch = preserve;
        self
    }

    pub fn reload_for_new_input(&mut self) {
        self.preserve_saved_epoch = false;
        if let Some(path) = self.settings_path.clone() {
            let root = self.models_root.clone();
            *self = Self::load(&path, false).with_models_root(root);
        }
    }
}

#[derive(Clone)]
pub struct RoutingDecision {
    pub original_model: String,
    pub effective_model: String,
    pub phase: RoutingPhase,
    pub epoch: Option<String>,
    pub reason: String,
    pub classifier_usage: Option<ClassifierUsage>,
}

impl std::fmt::Debug for RoutingDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingDecision")
            .field("original_model", &self.original_model)
            .field("effective_model", &self.effective_model)
            .field("phase", &self.phase)
            .field("epoch", &self.epoch)
            .field("reason", &self.reason)
            .field("classifier_usage", &self.classifier_usage)
            .finish()
    }
}

impl RoutingDecision {
    pub fn notice_text(&self) -> String {
        match self.phase {
            RoutingPhase::Planning => "요청의 복잡도에 따라 계획 모델을 선택했습니다.".into(),
            RoutingPhase::Implementation => "구현 단계 모델을 사용합니다.".into(),
            RoutingPhase::Fixed => match self.reason.as_str() {
                "disabled" => "라우팅이 꺼져 있어 기본 모델을 사용합니다.".into(),
                "classifier_pending" => "분류가 완료되지 않아 기본 모델을 사용합니다.".into(),
                "all_roles_inherit_original" => {
                    "모든 역할이 기본 모델을 사용하도록 설정되어 있습니다.".into()
                }
                "invalid_settings" => "라우팅 설정을 확인할 수 없어 기본 모델을 사용합니다.".into(),
                "role_model_unavailable" => {
                    "역할 모델 설정을 사용할 수 없어 기본 모델을 사용합니다.".into()
                }
                "original_model_unavailable" => {
                    "기본 모델을 확인할 수 없어 라우팅을 적용하지 않습니다.".into()
                }
                "role_credential_unavailable" => {
                    "역할 모델 인증 정보를 찾을 수 없어 기본 모델을 사용합니다.".into()
                }
                "invalid_configuration" => {
                    "분류기 설정이 올바르지 않아 기본 모델을 사용합니다.".into()
                }
                "missing_credential" => {
                    "분류기 인증 정보를 찾을 수 없어 기본 모델을 사용합니다.".into()
                }
                "http_status" => "분류기 서버가 오류를 반환해 기본 모델을 사용합니다.".into(),
                "timeout" => "분류기 응답 시간이 초과되어 기본 모델을 사용합니다.".into(),
                "transport" => "분류기 서버에 연결할 수 없어 기본 모델을 사용합니다.".into(),
                "invalid_response" => "분류기 응답을 처리할 수 없어 기본 모델을 사용합니다.".into(),
                "no_user_input" => "사용자 요청을 찾을 수 없어 기본 모델을 사용합니다.".into(),
                "all_roles_match_original_model" => {
                    "모든 역할이 기본 모델과 같아 분류를 생략했습니다.".into()
                }
                _ => "분류에 실패해 기본 모델을 사용합니다.".into(),
            },
        }
    }
}

fn checked_usage(usage: Option<ClassifierUsage>) -> (Option<i64>, Option<i64>) {
    usage
        .map(|u| {
            (
                i64::try_from(u.input_tokens).ok(),
                i64::try_from(u.output_tokens).ok(),
            )
        })
        .unwrap_or((None, None))
}

fn state(
    conversation_id: &str,
    epoch: &str,
    phase: RoutingPhase,
    original: &str,
    planning: &str,
    implementation: &str,
    reason: &str,
    usage: Option<ClassifierUsage>,
) -> ConversationRoutingState {
    let (classifier_input_tokens, classifier_output_tokens) = checked_usage(usage);
    ConversationRoutingState {
        conversation_id: conversation_id.to_owned(),
        epoch: epoch.to_owned(),
        phase,
        original_model: original.to_owned(),
        planning_model: planning.to_owned(),
        implementation_model: implementation.to_owned(),
        reason: reason.to_owned(),
        revision: 0,
        classifier_input_tokens,
        classifier_output_tokens,
    }
}

fn usage_from_state(s: &ConversationRoutingState) -> Option<ClassifierUsage> {
    Some(ClassifierUsage {
        input_tokens: u64::try_from(s.classifier_input_tokens?).ok()?,
        output_tokens: u64::try_from(s.classifier_output_tokens?).ok()?,
    })
}

fn is_complex(probability: f64) -> bool {
    probability >= 0.5
}

fn latest_user_prompt(messages: &[ChatMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == dume_provider::types::Role::User)
        .map(|message| message.content.as_str())
}

fn decision(original: &str, s: &ConversationRoutingState) -> RoutingDecision {
    let effective_model = match s.phase {
        RoutingPhase::Implementation => s.implementation_model.clone(),
        RoutingPhase::Planning => s.planning_model.clone(),
        RoutingPhase::Fixed => original.to_owned(),
    };
    RoutingDecision {
        original_model: original.to_owned(),
        effective_model,
        phase: s.phase,
        epoch: Some(s.epoch.clone()),
        reason: s.reason.clone(),
        classifier_usage: usage_from_state(s),
    }
}

pub async fn select_model(
    snapshot: &RoutingSnapshot,
    session: &RunSession,
    original_model: &str,
    messages: &[ChatMessage],
    credentials: &CredentialStore,
    cancellation: &CancellationToken,
) -> Result<RoutingDecision> {
    if cancellation.is_cancelled() {
        return Err(anyhow!("Agent execution cancelled"));
    }
    let existing = session.routing_state()?;
    if snapshot.preserve_saved_epoch {
        if let Some(saved) = existing
            .as_ref()
            .filter(|s| s.original_model == original_model)
        {
            session.set_routing_epoch(
                (saved.phase == RoutingPhase::Planning).then_some(saved.epoch.as_str()),
            );
            return Ok(decision(original_model, saved));
        }
    }
    let settings = match snapshot.settings.as_ref() {
        Some(settings) if settings.enabled => settings,
        Some(settings) => {
            let reason = "disabled";
            let epoch = settings
                .fingerprint(original_model)
                .map_err(|_| anyhow!("validated settings fingerprint failed"))?;
            return persist_fixed(session, existing.as_ref(), original_model, &epoch, reason);
        }
        None => {
            let reason = snapshot.invalid_reason.unwrap_or("invalid_settings");
            let epoch = format!(
                "{}",
                hex::encode(sha2::Sha256::digest(
                    format!("dume-routing-invalid-v1\0{reason}\0{original_model}").as_bytes()
                ))
            );
            return persist_fixed(session, existing.as_ref(), original_model, &epoch, reason);
        }
    };
    let classifier_config = settings
        .classifier
        .as_ref()
        .ok_or_else(|| anyhow!("validated routing settings lack classifier"))?;
    let epoch = settings
        .fingerprint(original_model)
        .map_err(|_| anyhow!("validated routing settings fingerprint failed"))?;
    if let Some(saved) = existing
        .as_ref()
        .filter(|s| s.epoch == epoch && s.original_model == original_model)
    {
        if saved.phase == RoutingPhase::Fixed {
            session.set_routing_epoch(None);
            return Ok(decision(original_model, saved));
        }
        session.set_routing_epoch(
            (saved.phase == RoutingPhase::Planning).then_some(saved.epoch.as_str()),
        );
        return Ok(decision(original_model, saved));
    }
    if settings.all_roles_match_original(original_model) {
        return persist_fixed(
            session,
            existing.as_ref(),
            original_model,
            &epoch,
            "all_roles_inherit_original",
        );
    }
    let root = snapshot.models_root.as_path();
    let baseline = match resolve_model_with_root(original_model, root) {
        Ok(info) if dume_provider::runtime::is_model_supported(&info) => info,
        _ => {
            return persist_fixed(
                session,
                existing.as_ref(),
                original_model,
                &epoch,
                "original_model_unavailable",
            );
        }
    };
    let selected = [
        settings.model_for(
            crate::routing_settings::RoutingRole::StandardPlanning,
            original_model,
        ),
        settings.model_for(
            crate::routing_settings::RoutingRole::ComplexPlanning,
            original_model,
        ),
        settings.model_for(
            crate::routing_settings::RoutingRole::Implementation,
            original_model,
        ),
    ];
    let mut resolved = Vec::new();
    for model in selected {
        let info = match resolve_model_with_root(model, root) {
            Ok(info) => info,
            Err(_) => {
                return persist_fixed(
                    session,
                    existing.as_ref(),
                    original_model,
                    &epoch,
                    "role_model_unavailable",
                );
            }
        };
        if !dume_provider::runtime::is_model_supported(&info) {
            return persist_fixed(
                session,
                existing.as_ref(),
                original_model,
                &epoch,
                "role_model_unavailable",
            );
        }
        if !credentials.has_credential(&info.provider) {
            return persist_fixed(
                session,
                existing.as_ref(),
                original_model,
                &epoch,
                "role_credential_unavailable",
            );
        }
        resolved.push((
            format!(
                "{}/{}",
                dume_provider::auth::normalize_provider(&info.provider),
                info.id
            ),
            info,
        ));
    }
    let baseline_id = format!(
        "{}/{}",
        dume_provider::auth::normalize_provider(&baseline.provider),
        baseline.id
    );
    if resolved.iter().all(|(id, _)| *id == baseline_id) {
        return persist_fixed(
            session,
            existing.as_ref(),
            original_model,
            &epoch,
            "all_roles_match_original_model",
        );
    }
    let pending = state(
        session.conversation_id(),
        &epoch,
        RoutingPhase::Fixed,
        original_model,
        original_model,
        original_model,
        "classifier_pending",
        None,
    );
    let expected = existing.as_ref().map(|s| s.revision);
    let pending = session.save_routing_state(&pending, expected)?;
    let Some(user_prompt) = latest_user_prompt(messages) else {
        let failed = state(
            session.conversation_id(),
            &epoch,
            RoutingPhase::Fixed,
            original_model,
            original_model,
            original_model,
            "no_user_input",
            None,
        );
        let saved = session.save_routing_state(&failed, Some(pending.revision))?;
        session.set_routing_epoch(None);
        return Ok(decision(original_model, &saved));
    };
    let key = match credentials
        .resolve_credential(classifier_config.backend.credential_provider())
        .await
    {
        Ok(credential) => credential.and_then(|c| c.key.or(c.access_token)),
        Err(_) => None,
    };
    let classifier = match SystemOneClassifier::new(classifier_config, key) {
        Ok(classifier) => classifier,
        Err(error) => {
            if matches!(error.kind, ClassifierErrorKind::Cancelled) || cancellation.is_cancelled() {
                return Err(anyhow!("Agent execution cancelled"));
            }
            let failed = state(
                session.conversation_id(),
                &epoch,
                RoutingPhase::Fixed,
                original_model,
                original_model,
                original_model,
                error.reason_code(),
                error.usage,
            );
            let saved = session.save_routing_state(&failed, Some(pending.revision))?;
            session.set_routing_epoch(None);
            return Ok(decision(original_model, &saved));
        }
    };
    let classification = match classifier.classify(user_prompt, cancellation).await {
        Ok(result) => result,
        Err(error) => {
            if matches!(error.kind, ClassifierErrorKind::Cancelled) || cancellation.is_cancelled() {
                return Err(anyhow!("Agent execution cancelled"));
            }
            let failed = state(
                session.conversation_id(),
                &epoch,
                RoutingPhase::Fixed,
                original_model,
                original_model,
                original_model,
                error.reason_code(),
                error.usage,
            );
            let saved = session.save_routing_state(&failed, Some(pending.revision))?;
            session.set_routing_epoch(None);
            return Ok(decision(original_model, &saved));
        }
    };
    if cancellation.is_cancelled() {
        return Err(anyhow!("Agent execution cancelled"));
    }
    let planning_index = usize::from(is_complex(classification.complex_probability));
    let success = state(
        session.conversation_id(),
        &epoch,
        RoutingPhase::Planning,
        original_model,
        &resolved[planning_index].0,
        &resolved[2].0,
        if planning_index == 1 {
            "complex"
        } else {
            "standard"
        },
        classification.usage,
    );
    let saved = session.save_routing_state(&success, Some(pending.revision))?;
    session.set_routing_epoch(Some(&epoch));
    Ok(decision(original_model, &saved))
}

use sha2::Digest;

fn persist_fixed(
    session: &RunSession,
    existing: Option<&ConversationRoutingState>,
    original: &str,
    epoch: &str,
    reason: &str,
) -> Result<RoutingDecision> {
    let saved = state(
        session.conversation_id(),
        epoch,
        RoutingPhase::Fixed,
        original,
        original,
        original,
        reason,
        None,
    );
    if existing.is_none() {
        session.set_routing_epoch(None);
        return Ok(RoutingDecision {
            original_model: original.to_owned(),
            effective_model: original.to_owned(),
            phase: RoutingPhase::Fixed,
            epoch: None,
            reason: reason.to_owned(),
            classifier_usage: None,
        });
    }
    if existing.is_some_and(|current| {
        current.epoch == epoch
            && current.original_model == original
            && current.phase == RoutingPhase::Fixed
    }) {
        session.set_routing_epoch(None);
        return Ok(decision(original, existing.unwrap()));
    }
    let result = session.save_routing_state(&saved, existing.map(|current| current.revision))?;
    session.set_routing_epoch(None);
    Ok(decision(original, &result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dume_provider::auth::Credential;

    fn session() -> (tempfile::TempDir, RunSession) {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            dume_store::HarnessStore::in_memory(dir.path().join("artifacts")).unwrap(),
        );
        store
            .create_conversation("conversation", "/tmp", "original", "provider-cache-session")
            .unwrap();
        let scope = dume_store::RuntimeScope::Conversation("conversation".into());
        let receipt = store
            .admit_input(
                scope.clone(),
                "request",
                dume_store::InputKind::Submit,
                "request",
            )
            .unwrap();
        let session = RunSession::start(
            store,
            scope,
            receipt.run_id.as_deref().unwrap(),
            "conversation",
        )
        .unwrap();
        (dir, session)
    }

    async fn classifier_server(probability: f64) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 2048];
            let (body_start, content_length) = loop {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    stream.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(count > 0, "classifier request ended before body");
                request.extend_from_slice(&chunk[..count]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..index]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    let body_start = index + 4;
                    if request.len() >= body_start + content_length {
                        break (body_start, content_length);
                    }
                }
            };
            let response = format!(
                r#"{{"answers":{{"complexity":{{"type":"choice","choice":"complex","probabilities":{{"standard":{},"complex":{}}},"confidence":0.8}}}},"usage":{{"input_tokens":7,"output_tokens":2}}}}"#,
                1.0 - probability,
                probability
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..body_start + content_length]).into_owned()
        });
        (format!("http://{address}/v1/"), task)
    }

    async fn classifier_response_server(
        response: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 2048];
            let body_start = loop {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(count > 0, "classifier request ended before headers");
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..body_start]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < body_start + length {
                let count = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(count > 0, "classifier request ended before body");
                bytes.extend_from_slice(&chunk[..count]);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                    .await
                    .is_err(),
                "same-epoch failure triggered another classifier request"
            );
        });
        (format!("http://{address}/v1/"), task)
    }

    fn active_settings(url: String) -> RoutingSettings {
        RoutingSettings {
            enabled: true,
            classifier: Some(dume_provider::ClassifierConfig {
                backend: dume_provider::ClassifierBackend::LocalSystemOne,
                base_url: url,
                model: "test-classifier".into(),
            }),
            models: crate::routing_settings::RoutingModels {
                standard: Some("openai/gpt-4o-mini".into()),
                complex: Some("openai/gpt-4o".into()),
                implementation: Some("openai/gpt-4.1-mini".into()),
            },
            ..Default::default()
        }
    }

    #[test]
    fn classifier_threshold_is_inclusive_at_one_half() {
        assert!(!is_complex(0.499));
        assert!(!is_complex(0.5 - f64::EPSILON));
        assert!(is_complex(0.5));
        assert!(is_complex(0.501));
    }

    #[test]
    fn classifier_prompt_uses_only_latest_user_message() {
        let messages = vec![
            ChatMessage::system("system-private"),
            ChatMessage::user("earlier user"),
            ChatMessage::assistant("assistant-private"),
            ChatMessage::tool("tool-private", "call"),
            ChatMessage::user("latest user"),
        ];
        assert_eq!(latest_user_prompt(&messages), Some("latest user"));
        assert_eq!(
            latest_user_prompt(&[ChatMessage::system("only system")]),
            None
        );
    }

    #[test]
    fn fixed_state_never_selects_saved_planning_model() {
        let saved = state(
            "conversation",
            "epoch",
            RoutingPhase::Fixed,
            "original",
            "old-planning",
            "implementation",
            "classifier_pending",
            None,
        );
        assert_eq!(decision("original", &saved).effective_model, "original");
    }

    #[test]
    fn usage_overflow_is_omitted_without_changing_the_routing_choice() {
        let (input, output) = checked_usage(Some(ClassifierUsage {
            input_tokens: u64::MAX,
            output_tokens: 2,
        }));
        assert_eq!(input, None);
        assert_eq!(output, Some(2));
        let selected = state(
            "conversation",
            "epoch",
            RoutingPhase::Planning,
            "original",
            "chosen",
            "implementation",
            "standard",
            Some(ClassifierUsage {
                input_tokens: u64::MAX,
                output_tokens: 2,
            }),
        );
        assert_eq!(decision("original", &selected).effective_model, "chosen");
        assert_eq!(usage_from_state(&selected), None);
    }

    #[tokio::test]
    async fn disabled_routing_uses_original_and_clears_stale_tool_epoch() {
        let (_dir, session) = session();
        session.set_routing_epoch(Some("stale-epoch"));
        let decision = select_model(
            &RoutingSnapshot::from_settings(RoutingSettings::default(), false),
            &session,
            "original",
            &[ChatMessage::user("request")],
            &CredentialStore::new("unused-auth.json"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(decision.effective_model, "original");
        assert_eq!(decision.reason, "disabled");
        assert!(session.routing_state().unwrap().is_none());
    }

    #[tokio::test]
    async fn disabled_unconfigured_and_all_inherit_do_not_post() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (_dir, session) = session();
        let credentials = CredentialStore::new("unused-auth.json");
        let disabled = RoutingSnapshot::from_settings(RoutingSettings::default(), false);
        let unconfigured = RoutingSnapshot::from_settings(
            RoutingSettings {
                enabled: true,
                ..Default::default()
            },
            false,
        );
        let inherited = RoutingSnapshot::from_settings(
            RoutingSettings {
                enabled: true,
                classifier: Some(dume_provider::ClassifierConfig {
                    backend: dume_provider::ClassifierBackend::LocalSystemOne,
                    base_url: url,
                    model: "test".into(),
                }),
                ..Default::default()
            },
            false,
        );
        for snapshot in [disabled, unconfigured, inherited] {
            let decision = select_model(
                &snapshot,
                &session,
                "openai/gpt-4o-mini",
                &[ChatMessage::user("request")],
                &credentials,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(decision.effective_model, "openai/gpt-4o-mini");
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn resolved_roles_equal_to_baseline_skip_classifier_and_unknown_baseline_cannot_route() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (dir, run_session) = session();
        let credentials = CredentialStore::new(dir.path().join("auth.json"));
        credentials
            .save(
                "openai",
                &Credential {
                    cred_type: "api_key".into(),
                    key: Some("model-token".into()),
                    access_token: None,
                    refresh_token: None,
                    expires_at: None,
                    account_id: None,
                },
            )
            .unwrap();
        let mut settings = active_settings(url.clone());
        settings.models = crate::routing_settings::RoutingModels {
            standard: Some("openai/gpt-4o-mini".into()),
            complex: Some("openai/gpt-4o-mini".into()),
            implementation: Some("openai/gpt-4o-mini".into()),
        };
        let same_as_baseline = RoutingSnapshot::from_settings(settings.clone(), false);
        let selected = select_model(
            &same_as_baseline,
            &run_session,
            "gpt-4o-mini",
            &[ChatMessage::user("request")],
            &credentials,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(selected.effective_model, "gpt-4o-mini");
        assert_eq!(selected.reason, "all_roles_match_original_model");

        let (_other_dir, other_session) = session();
        let unknown_baseline = select_model(
            &RoutingSnapshot::from_settings(settings, false),
            &other_session,
            "unsupported-baseline-model",
            &[ChatMessage::user("request")],
            &credentials,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            unknown_baseline.effective_model,
            "unsupported-baseline-model"
        );
        assert_eq!(unknown_baseline.reason, "original_model_unavailable");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn selection_threshold_and_latest_user_wire_body_are_verified_over_local_http() {
        for (probability, expected_model) in [
            (0.499, "openai/gpt-4o-mini"),
            (0.5, "openai/gpt-4o"),
            (0.501, "openai/gpt-4o"),
        ] {
            let (url, server) = classifier_server(probability).await;
            let (dir, session) = session();
            let credential_store = CredentialStore::new(dir.path().join("auth.json"));
            credential_store
                .save(
                    "openai",
                    &Credential {
                        cred_type: "api_key".into(),
                        key: Some("model-token".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
            credential_store
                .save(
                    "jev-local-system-one",
                    &Credential {
                        cred_type: "api_key".into(),
                        key: Some("local-secret".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
            let messages = [
                ChatMessage::system("system secret"),
                ChatMessage::tool("tool secret", "call"),
                ChatMessage::user("latest-only"),
            ];
            let snapshot = RoutingSnapshot::from_settings(active_settings(url), false)
                .with_models_root(dir.path().to_path_buf());
            let selected = select_model(
                &snapshot,
                &session,
                "openai/gpt-4o-mini",
                &messages,
                &credential_store,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(selected.effective_model, expected_model);
            let request = server.await.unwrap();
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer local-secret")
            );
            assert!(request.contains("latest-only"));
            assert!(!request.contains("system secret"));
            assert!(!request.contains("tool secret"));
            assert!(!request.contains("model-token"));
            let again = select_model(
                &snapshot,
                &session,
                "openai/gpt-4o-mini",
                &messages,
                &credential_store,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(again.effective_model, expected_model);
        }
    }

    #[tokio::test]
    async fn malformed_classifier_response_persists_fixed_and_ignores_later_credential_changes() {
        let bad_responses = [
            "not-json",
            r#"{"answers":{"complexity":{"type":"choice","choice":"complex","probabilities":{"standard":0.2,"complex":0.8}}}}"#,
            r#"{"answers":{"complexity":{"type":"choice","choice":"complex","probabilities":{"standard":0.4,"complex":0.8},"confidence":0.9}}}"#,
        ];
        for malformed in bad_responses {
            let (url, server) = classifier_response_server(malformed).await;
            let (dir, run_session) = session();
            let credentials = CredentialStore::new(dir.path().join("auth.json"));
            credentials
                .save(
                    "openai",
                    &Credential {
                        cred_type: "api_key".into(),
                        key: Some("model-key".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
            credentials
                .save(
                    "jev-local-system-one",
                    &Credential {
                        cred_type: "api_key".into(),
                        key: Some("classifier-key".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
            let settings = active_settings(url);
            let snapshot = RoutingSnapshot::from_settings(settings.clone(), false);
            let messages = [ChatMessage::user("첫 요청")];
            let first = select_model(
                &snapshot,
                &run_session,
                "openai/gpt-4o-mini",
                &messages,
                &credentials,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(first.effective_model, "openai/gpt-4o-mini");
            assert_eq!(first.phase, RoutingPhase::Fixed);
            server.await.unwrap();
            let saved = run_session.routing_state().unwrap().unwrap();
            assert_eq!(saved.reason, "invalid_response");
            assert_eq!(saved.revision, 1);
            credentials
                .save(
                    "jev-local-system-one",
                    &Credential {
                        cred_type: "api_key".into(),
                        key: Some("rotated-key".into()),
                        access_token: None,
                        refresh_token: None,
                        expires_at: None,
                        account_id: None,
                    },
                )
                .unwrap();
            let again = select_model(
                &snapshot,
                &run_session,
                "openai/gpt-4o-mini",
                &[ChatMessage::user("다음 요청")],
                &credentials,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(again.effective_model, "openai/gpt-4o-mini");
            assert_eq!(
                run_session.routing_state().unwrap().unwrap().revision,
                saved.revision
            );
        }
    }

    #[tokio::test]
    async fn resumed_classifier_pending_row_wins_over_changed_supplied_snapshot_without_post() {
        let (dir, first_session) = session();
        let store = first_session.store().clone();
        let original = "openai/gpt-4o-mini";
        let pending = state(
            "conversation",
            "saved-epoch",
            RoutingPhase::Fixed,
            original,
            original,
            original,
            "classifier_pending",
            None,
        );
        let saved = first_session.save_routing_state(&pending, None).unwrap();
        let run_id = first_session.token().run_id.clone();
        drop(first_session);
        let resumed = RunSession::resume(store, &run_id, "conversation").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let classifier_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut edited = active_settings(classifier_url);
        edited.models.complex = Some("openai/gpt-4.1-mini".into());
        let snapshot = RoutingSnapshot::from_settings(edited, false)
            .with_models_root(dir.path().to_path_buf())
            .with_epoch_preservation(true);
        let decision = select_model(
            &snapshot,
            &resumed,
            original,
            &[ChatMessage::user("재생 요청")],
            &CredentialStore::new(dir.path().join("auth.json")),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(decision.effective_model, original);
        assert_eq!(decision.phase, RoutingPhase::Fixed);
        assert_eq!(decision.reason, "classifier_pending");
        assert_eq!(decision.epoch.as_deref(), Some("saved-epoch"));
        assert_eq!(
            resumed.routing_state().unwrap().unwrap().revision,
            saved.revision
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reload_after_safe_new_input_uses_supplied_settings_and_classifies_new_epoch_once() {
        let (first_url, first_server) = classifier_server(0.1).await;
        let (dir, run_session) = session();
        let credentials = CredentialStore::new(dir.path().join("auth.json"));
        credentials
            .save(
                "openai",
                &Credential {
                    cred_type: "api_key".into(),
                    key: Some("model-key".into()),
                    access_token: None,
                    refresh_token: None,
                    expires_at: None,
                    account_id: None,
                },
            )
            .unwrap();
        credentials
            .save(
                "jev-local-system-one",
                &Credential {
                    cred_type: "api_key".into(),
                    key: Some("classifier-key".into()),
                    access_token: None,
                    refresh_token: None,
                    expires_at: None,
                    account_id: None,
                },
            )
            .unwrap();
        let initial = RoutingSnapshot::from_settings(active_settings(first_url), true);
        let first = select_model(
            &initial,
            &run_session,
            "openai/gpt-4o-mini",
            &[ChatMessage::user("기존 입력")],
            &credentials,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(first.effective_model, "openai/gpt-4o-mini");
        first_server.await.unwrap();
        let fixed = run_session.routing_state().unwrap().unwrap();
        assert_eq!(fixed.phase, RoutingPhase::Planning);

        let (new_url, new_server) = classifier_server(0.9).await;
        let mut edited = active_settings(new_url);
        edited.models.complex = Some("openai/gpt-4.1-mini".into());
        let mut next_snapshot = RoutingSnapshot::from_settings(edited, true);
        next_snapshot.reload_for_new_input();
        assert!(!next_snapshot.preserve_saved_epoch);
        let next = select_model(
            &next_snapshot,
            &run_session,
            "openai/gpt-4o-mini",
            &[ChatMessage::user("새 입력")],
            &credentials,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(next.effective_model, "openai/gpt-4.1-mini");
        new_server.await.unwrap();
        let latest = run_session.routing_state().unwrap().unwrap();
        assert_ne!(latest.epoch, fixed.epoch);
        assert_eq!(latest.phase, RoutingPhase::Planning);
        let cached = select_model(
            &next_snapshot,
            &run_session,
            "openai/gpt-4o-mini",
            &[ChatMessage::user("압축 후 동등 입력")],
            &credentials,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(cached.effective_model, "openai/gpt-4.1-mini");
        assert_eq!(
            run_session.routing_state().unwrap().unwrap().revision,
            latest.revision
        );
    }

    #[test]
    fn snapshot_validation_failure_is_static_and_does_not_echo_configuration() {
        let mut settings = RoutingSettings::default();
        settings.version = 9;
        let snapshot = RoutingSnapshot::from_settings(settings, false);
        assert_eq!(snapshot.invalid_reason, Some("invalid_settings"));
        assert!(!format!("{snapshot:?}").contains("9"));
    }

    #[test]
    fn snapshot_reload_reads_its_explicit_path_and_drops_resume_preservation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom-routing.json");
        let before = RoutingSettings::default();
        before.save(&path).unwrap();
        let mut snapshot = RoutingSnapshot::load(&path, true);
        let after = RoutingSettings {
            enabled: false,
            models: crate::routing_settings::RoutingModels {
                standard: Some("changed".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        after.save(&path).unwrap();
        snapshot.reload_for_new_input();
        assert_eq!(snapshot.settings.as_ref(), Some(&after));
        assert!(!snapshot.preserve_saved_epoch);
    }

    #[test]
    fn epoch_preservation_override_does_not_change_settings_fingerprint() {
        let settings = RoutingSettings::default();
        let fingerprint = settings.fingerprint("baseline").unwrap();
        let snapshot = RoutingSnapshot::from_settings(settings, false);
        let replay_snapshot = snapshot.clone().with_epoch_preservation(true);
        let fresh_snapshot = replay_snapshot.clone().with_epoch_preservation(false);
        assert!(replay_snapshot.preserve_saved_epoch);
        assert!(!fresh_snapshot.preserve_saved_epoch);
        assert_eq!(
            snapshot
                .settings
                .as_ref()
                .unwrap()
                .fingerprint("baseline")
                .unwrap(),
            fingerprint
        );
        assert_eq!(
            replay_snapshot
                .settings
                .as_ref()
                .unwrap()
                .fingerprint("baseline")
                .unwrap(),
            fingerprint
        );
        assert_eq!(
            fresh_snapshot
                .settings
                .as_ref()
                .unwrap()
                .fingerprint("baseline")
                .unwrap(),
            fingerprint
        );
    }
}
