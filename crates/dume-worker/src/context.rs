use anyhow::{Context, Result};
use dume_provider::types::{ChatMessage, MessagePurpose, Role, ToolDefinition};
use dume_store::HarnessStore;

pub struct RequestContext<'a> {
    pub model: &'a str,
    pub session_id: &'a str,
    pub messages: &'a mut Vec<ChatMessage>,
    pub tools: &'a [ToolDefinition],
    pub store: Option<&'a HarnessStore>,
}

pub fn prepare_request_context(request: RequestContext<'_>) -> Result<bool> {
    let budget = dume_provider::catalog::ModelCatalog::context_budget(request.model);
    if estimate_request_tokens(request.messages, request.tools)? <= budget {
        return Ok(false);
    }

    let store = request
        .store
        .context("Context exceeds model budget and no session store is configured")?;
    let mut selected = None;
    let first_cut = request
        .messages
        .iter()
        .rposition(|message| message.purpose == MessagePurpose::Checkpoint)
        .map_or(1, |index| index + 1);
    for cut in first_cut..=request.messages.len() {
        if !is_complete_boundary(request.messages, cut) {
            continue;
        }
        let checkpoint = checkpoint_content(&request.messages[..cut], &"0".repeat(64));
        let compacted = compacted_messages(request.messages, cut, &checkpoint);
        if estimate_request_tokens(&compacted, request.tools)? <= budget {
            selected = Some((cut, compacted));
            break;
        }
    }

    let cut = selected.map(|(cut, _)| cut).context(
        "Context cannot be safely compacted below the model budget; original history was preserved",
    )?;
    let archived = serde_json::to_vec(&request.messages[..cut])?;
    let artifact_id = store
        .artifacts
        .save_artifact(&archived)
        .context("Failed to preserve archived conversation before compaction")?;
    let checkpoint = checkpoint_content(&request.messages[..cut], &artifact_id);
    let compacted = compacted_messages(request.messages, cut, &checkpoint);
    anyhow::ensure!(
        estimate_request_tokens(&compacted, request.tools)? <= budget,
        "Context checkpoint exceeds model budget; original history was preserved"
    );

    persist_and_compact(
        store,
        request.session_id,
        request.messages,
        &checkpoint,
        cut,
    )?;
    *request.messages = compacted;
    Ok(true)
}

fn estimate_request_tokens(messages: &[ChatMessage], tools: &[ToolDefinition]) -> Result<usize> {
    let messages_tokens: usize = messages
        .iter()
        .filter(|message| message.is_model_visible())
        .map(ChatMessage::approx_tokens)
        .sum();
    let tool_bytes = serde_json::to_vec(tools)?.len();
    Ok(messages_tokens.saturating_add(tool_bytes.div_ceil(4)))
}

fn is_complete_boundary(messages: &[ChatMessage], cut: usize) -> bool {
    if cut == messages.len() {
        return !messages[cut - 1]
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty());
    }
    if messages[cut].role == Role::Tool {
        return false;
    }
    !messages[cut - 1]
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
}

fn checkpoint_content(prefix: &[ChatMessage], artifact_id: &str) -> String {
    let mut checkpoint = format!(
        "Conversation checkpoint. Exact earlier transcript is preserved in this artifact; retrieve it with read_artifact if details are needed.\nArtifact ID: {}\nUser requests and constraints preserved verbatim:\n",
        artifact_id
    );
    for message in prefix.iter().filter(|message| message.role == Role::User) {
        checkpoint.push_str("\n--- User request ---\n");
        checkpoint.push_str(&message.content);
    }
    for previous in prefix
        .iter()
        .filter(|message| message.purpose == MessagePurpose::Checkpoint)
    {
        checkpoint.push_str("\n--- Earlier checkpoint ---\n");
        checkpoint.push_str(&previous.content);
    }
    if let Some(message) = prefix.iter().rev().find(|message| {
        message.role == Role::Assistant
            && message.tool_calls.is_none()
            && message.content.len() <= 2048
    }) {
        checkpoint.push_str("\n--- Latest prior assistant response ---\n");
        checkpoint.push_str(&message.content);
    }
    let recent_tool_results: Vec<_> = prefix
        .iter()
        .filter(|message| message.role == Role::Tool)
        .rev()
        .take(3)
        .collect();
    if !recent_tool_results.is_empty() {
        checkpoint.push_str("\n--- Recent tool evidence ---\n");
        for message in recent_tool_results.into_iter().rev() {
            checkpoint.push_str(&format!(
                "Tool call {} result excerpt:\n{}\n",
                message.tool_call_id.as_deref().unwrap_or("unknown"),
                message.content.chars().take(1024).collect::<String>()
            ));
        }
    }
    checkpoint
}

fn compacted_messages(messages: &[ChatMessage], cut: usize, checkpoint: &str) -> Vec<ChatMessage> {
    let mut compacted: Vec<ChatMessage> = messages
        .iter()
        .filter(|message| {
            message.role == Role::System && message.purpose == MessagePurpose::Conversation
        })
        .cloned()
        .collect();
    let checkpoint_message = ChatMessage::checkpoint(checkpoint);
    let prefix_count = usize::from(compacted.first().is_some_and(|message| {
        message.content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT
    }));
    compacted.insert(prefix_count, checkpoint_message);
    compacted.extend(
        messages[..cut]
            .iter()
            .filter(|message| message.purpose == MessagePurpose::UiNotice)
            .cloned(),
    );
    compacted.extend(
        messages[cut..]
            .iter()
            .filter(|message| {
                message.role != Role::System || message.purpose == MessagePurpose::UiNotice
            })
            .cloned(),
    );
    compacted
}

fn persist_and_compact(
    store: &HarnessStore,
    session_id: &str,
    messages: &[ChatMessage],
    checkpoint: &str,
    cut: usize,
) -> Result<()> {
    let persisted: Vec<_> = messages
        .iter()
        .filter(|message| message.is_model_visible())
        .map(|message| {
            let (role, calls, call_id) = match message.role {
                Role::System => ("system", None, None),
                Role::User => ("user", None, None),
                Role::Assistant => (
                    "assistant",
                    message
                        .tool_calls
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    None,
                ),
                Role::Tool => ("tool", None, message.tool_call_id.clone()),
            };
            Ok((
                role.to_string(),
                message.content.clone(),
                calls,
                call_id,
                message.purpose == MessagePurpose::Checkpoint,
            ))
        })
        .collect::<Result<_>>()?;
    let retain_last_n = messages[cut..]
        .iter()
        .filter(|message| {
            message.is_model_visible() && message.purpose != MessagePurpose::Checkpoint
        })
        .count();
    store.persist_and_compact_session(session_id, &persisted, checkpoint, retain_last_n)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dume_provider::types::ToolCall;

    #[test]
    fn oversized_context_keeps_constraints_and_archives_complete_tool_groups() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::in_memory(dir.path().join("artifacts")).unwrap();
        let tools = vec![];
        let calls = (1..=3)
            .map(|index| ToolCall {
                id: format!("tool-{index}"),
                name: format!("inspect-{index}"),
                arguments: "{}".into(),
            })
            .collect();
        let mut messages = vec![
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
            ChatMessage::user("Find the failure and preserve the public API."),
            ChatMessage::assistant_with_tool_calls("", calls),
            ChatMessage::tool("first-result".to_string() + &"x".repeat(150_000), "tool-1"),
            ChatMessage::tool("second-result".to_string() + &"y".repeat(150_000), "tool-2"),
            ChatMessage::tool("third-result".to_string() + &"z".repeat(150_000), "tool-3"),
            ChatMessage::assistant("Three inspections completed."),
            ChatMessage::user("Continue and check the existing tests."),
        ];
        let request = RequestContext {
            model: "unknown-model-with-default-budget",
            session_id: "checkpoint-test",
            messages: &mut messages,
            tools: &tools,
            store: Some(&store),
        };

        assert!(prepare_request_context(request).unwrap());
        assert!(messages[0].content == dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT);
        assert!(messages[1].content.contains("preserve the public API"));
        assert!(messages[1].content.contains("artifact"));
        assert!(messages.iter().all(|message| message.role != Role::Tool));
        assert!(messages
            .iter()
            .any(|message| message.content == "Three inspections completed."));
        assert!(messages
            .iter()
            .any(|message| message.content == "Continue and check the existing tests."));

        let active = store
            .list_active_context_messages("checkpoint-test")
            .unwrap();
        let checkpoint = active.iter().find(|message| message.4).unwrap();
        assert!(checkpoint.1.contains("preserve the public API"));
        assert!(checkpoint.1.contains("Recent tool evidence"));
        assert!(checkpoint.1.contains("first-result"));
        let artifact_id = checkpoint
            .1
            .split_whitespace()
            .find(|part| part.len() == 64 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
            .unwrap();
        let archived = store.artifacts.get_artifact(artifact_id).unwrap();
        let archived_messages: Vec<ChatMessage> = serde_json::from_slice(&archived).unwrap();
        assert_eq!(
            archived_messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .count(),
            3
        );
    }

    #[test]
    fn over_budget_context_without_store_is_left_unchanged() {
        let mut messages = vec![
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
            ChatMessage::user("x".repeat(500_000)),
        ];
        let original = messages.clone();
        let result = prepare_request_context(RequestContext {
            model: "unknown-model-with-default-budget",
            session_id: "no-store",
            messages: &mut messages,
            tools: &[],
            store: None,
        });

        assert!(result.is_err());
        assert_eq!(messages, original);
    }

    #[test]
    fn repeated_compaction_replaces_the_previous_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::in_memory(dir.path().join("artifacts")).unwrap();
        let mut messages = vec![
            ChatMessage::system(dume_core::types::PromptPrefix::BASE_SYSTEM_PROMPT),
            ChatMessage::user("Preserve the public API."),
            ChatMessage::assistant("x".repeat(500_000)),
            ChatMessage::user("Continue."),
        ];
        for (session_id, filler) in [("first", None), ("second", Some("y".repeat(500_000)))] {
            if let Some(filler) = filler {
                messages.push(ChatMessage::assistant(filler));
                messages.push(ChatMessage::user("Finish the task."));
            }
            assert!(prepare_request_context(RequestContext {
                model: "unknown-model-with-default-budget",
                session_id,
                messages: &mut messages,
                tools: &[],
                store: Some(&store),
            })
            .unwrap());
        }
        let checkpoints: Vec<_> = messages
            .iter()
            .filter(|message| message.purpose == MessagePurpose::Checkpoint)
            .collect();
        assert_eq!(checkpoints.len(), 1);
        assert!(checkpoints[0].content.contains("Preserve the public API."));
    }
}
