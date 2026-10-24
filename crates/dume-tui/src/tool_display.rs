use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDisplayInfo {
    pub kind_running: String,
    pub kind_completed: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct ActiveToolItem {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub status: ActiveToolStatus,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StreamingToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub enum ActiveToolStatus {
    Running { started_at: std::time::Instant },
    Completed { duration_ms: u64 },
    Failed { duration_ms: u64, error: String },
}

#[derive(Clone, Copy)]
pub struct TranscriptConfig<'a> {
    pub expanded_tools: &'a std::collections::HashSet<String>,
    pub selected_tool_id: Option<&'a str>,
    pub active_tools: &'a [ActiveToolItem],
    pub streaming_tools: &'a [StreamingToolCall],
}

fn extract_json_string_field(json_str: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let key_pos = json_str.find(&key)?;
    let after_key = &json_str[key_pos + key.len()..];
    let colon_pos = after_key.find(':')?;
    let rest = after_key[colon_pos + 1..].trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let after_quote = &rest[1..];
    let mut escaped = false;
    let mut val = String::new();
    for ch in after_quote.chars() {
        if escaped {
            val.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(val);
        } else {
            val.push(ch);
        }
    }
    if !val.is_empty() {
        Some(val)
    } else {
        None
    }
}

pub fn format_tool_display(name: &str, arguments_json: &str) -> ToolDisplayInfo {
    let args: Value = serde_json::from_str(arguments_json).unwrap_or_else(|_| {
        let mut map = serde_json::Map::new();
        if let Some(path) = extract_json_string_field(arguments_json, "path") {
            map.insert("path".to_string(), Value::String(path));
        }
        if let Some(cmd) = extract_json_string_field(arguments_json, "command") {
            map.insert("command".to_string(), Value::String(cmd));
        }
        if let Some(pattern) = extract_json_string_field(arguments_json, "pattern") {
            map.insert("pattern".to_string(), Value::String(pattern));
        }
        if let Some(query) = extract_json_string_field(arguments_json, "query") {
            map.insert("query".to_string(), Value::String(query));
        }
        Value::Object(map)
    });
    match name {
        "read_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("file");
            let target = match (
                args.get("start_line").and_then(Value::as_u64),
                args.get("end_line").and_then(Value::as_u64),
            ) {
                (Some(start), Some(end)) => format!("{path}:{start}–{end}"),
                (Some(start), None) => format!("{path}:{start}"),
                _ => path.to_string(),
            };
            ToolDisplayInfo {
                kind_running: "Reading".to_string(),
                kind_completed: "Read".to_string(),
                target,
            }
        }
        "grep_search" => {
            let pattern = args
                .get("pattern")
                .or_else(|| args.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let path_filter = args.get("path_filter").and_then(Value::as_str);
            let target = match path_filter {
                Some(filter) if !filter.is_empty() => format!("{pattern} in {filter}"),
                _ => pattern.to_string(),
            };
            ToolDisplayInfo {
                kind_running: "Searching".to_string(),
                kind_completed: "Searched".to_string(),
                target,
            }
        }
        "bash" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            let first_line = cmd.lines().next().unwrap_or("").trim();
            let target = if first_line.chars().count() > 60 {
                let truncated: String = first_line.chars().take(57).collect();
                format!("{truncated}...")
            } else {
                first_line.to_string()
            };
            ToolDisplayInfo {
                kind_running: "Running".to_string(),
                kind_completed: "Ran".to_string(),
                target,
            }
        }
        "replace_file_content" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("file");
            ToolDisplayInfo {
                kind_running: "Editing".to_string(),
                kind_completed: "Edited".to_string(),
                target: path.to_string(),
            }
        }
        "write_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("file");
            ToolDisplayInfo {
                kind_running: "Writing".to_string(),
                kind_completed: "Wrote".to_string(),
                target: path.to_string(),
            }
        }
        "read_artifact" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("artifact");
            ToolDisplayInfo {
                kind_running: "Reading artifact".to_string(),
                kind_completed: "Read artifact".to_string(),
                target: path.to_string(),
            }
        }
        other => {
            let clean = other.strip_prefix("mcp__").unwrap_or(other);
            ToolDisplayInfo {
                kind_running: "Executing".to_string(),
                kind_completed: "Executed".to_string(),
                target: clean.to_string(),
            }
        }
    }
}

pub fn format_duration_short(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let sec = ms / 1000;
        format!("{}m{}s", sec / 60, sec % 60)
    }
}

pub fn extract_failure_summary(content: &str) -> String {
    if let Some(pos) = content.find("exit code ") {
        let after = &content[pos..];
        let code_part = after
            .split(|c: char| c == ')' || c == ':' || c == '\n')
            .next()
            .unwrap_or("");
        if !code_part.trim().is_empty() {
            return code_part.trim().to_string();
        }
    }
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Tool failed:") || trimmed == "Tool failed" {
            continue;
        }
        let stripped = trimmed
            .strip_prefix("error: ")
            .or_else(|| trimmed.strip_prefix("Error: "))
            .unwrap_or(trimmed);
        if stripped.chars().count() > 50 {
            let truncated: String = stripped.chars().take(47).collect();
            return format!("{truncated}...");
        }
        return stripped.to_string();
    }
    "failed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tool_display_read_file() {
        let info = format_tool_display(
            "read_file",
            r#"{"path":"app.rs","start_line":4420,"end_line":4450}"#,
        );
        assert_eq!(info.kind_running, "Reading");
        assert_eq!(info.kind_completed, "Read");
        assert_eq!(info.target, "app.rs:4420–4450");
    }

    #[test]
    fn test_format_tool_display_grep_search() {
        let info = format_tool_display("grep_search", r#"{"pattern":"turn_limit"}"#);
        assert_eq!(info.kind_running, "Searching");
        assert_eq!(info.kind_completed, "Searched");
        assert_eq!(info.target, "turn_limit");
    }

    #[test]
    fn test_format_tool_display_bash() {
        let info = format_tool_display("bash", r#"{"command":"cargo check"}"#);
        assert_eq!(info.kind_running, "Running");
        assert_eq!(info.kind_completed, "Ran");
        assert_eq!(info.target, "cargo check");
    }

    #[test]
    fn test_format_duration_short() {
        assert_eq!(format_duration_short(12), "12ms");
        assert_eq!(format_duration_short(1200), "1.2s");
        assert_eq!(format_duration_short(65000), "1m5s");
    }

    #[test]
    fn test_extract_failure_summary() {
        let content = "Tool failed (exit code 1):\nerror: cannot find value `foo`";
        assert_eq!(extract_failure_summary(content), "exit code 1");

        let content2 = "Error: No such file or directory (os error 2)";
        assert_eq!(
            extract_failure_summary(content2),
            "No such file or directory (os error 2)"
        );
    }

    #[test]
    fn test_format_tool_display_replace_and_write() {
        let replace = format_tool_display(
            "replace_file_content",
            r#"{"path":"src/app.rs"}"#,
        );
        assert_eq!(replace.kind_running, "Editing");
        assert_eq!(replace.kind_completed, "Edited");
        assert_eq!(replace.target, "src/app.rs");

        let write = format_tool_display("write_file", r#"{"path":"src/new.rs"}"#);
        assert_eq!(write.kind_running, "Writing");
        assert_eq!(write.kind_completed, "Wrote");
        assert_eq!(write.target, "src/new.rs");
    }

    #[test]
    fn test_format_tool_display_mcp() {
        let mcp = format_tool_display("mcp__github_search", "{}");
        assert_eq!(mcp.kind_running, "Executing");
        assert_eq!(mcp.kind_completed, "Executed");
        assert_eq!(mcp.target, "github_search");
    }

    #[test]
    fn test_format_tool_display_bash_long_command() {
        let long_cmd = "a".repeat(80);
        let info = format_tool_display("bash", &format!(r#"{{"command":"{long_cmd}"}}"#));
        assert!(info.target.ends_with("..."));
        assert_eq!(info.target.chars().count(), 60);
    }
}
