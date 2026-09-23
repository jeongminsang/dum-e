pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024; // 50 KB
pub const DEFAULT_MAX_LINES: usize = 500;
pub const DEFAULT_HEAD_LINES: usize = 200;
pub const DEFAULT_TAIL_LINES: usize = 100;
pub const DEFAULT_MAX_MATCHES: usize = 100;

/// Truncates string content if it exceeds byte limit or line limit,
/// preserving head and tail lines with a clear truncation marker.
pub fn truncate_output_head_tail(
    content: &str,
    max_bytes: usize,
    head_lines: usize,
    tail_lines: usize,
) -> (String, bool) {
    let line_count = content.lines().count();
    if content.len() <= max_bytes && line_count <= (head_lines + tail_lines) {
        return (content.to_string(), false);
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines <= head_lines + tail_lines {
        // Line count is within limits but bytes exceed limit
        let mut truncated = String::new();
        let mut byte_count = 0;
        for line in &lines {
            if byte_count + line.len() + 1 > max_bytes {
                truncated.push_str(&format!(
                    "\n... [Output truncated: total {} bytes exceeded limit {} bytes] ...\n",
                    content.len(),
                    max_bytes
                ));
                return (truncated, true);
            }
            truncated.push_str(line);
            truncated.push('\n');
            byte_count += line.len() + 1;
        }
        return (truncated, true);
    }

    let head = &lines[..head_lines];
    let tail = &lines[total_lines - tail_lines..];
    let omitted_lines = total_lines - head_lines - tail_lines;

    let mut result = String::with_capacity(max_bytes + 512);
    for line in head {
        result.push_str(line);
        result.push('\n');
    }

    result.push_str(&format!(
        "\n... [Truncated {} lines / total {} lines, {} bytes exceeded limit] ...\n\n",
        omitted_lines,
        total_lines,
        content.len()
    ));

    for line in tail {
        result.push_str(line);
        result.push('\n');
    }

    (result, true)
}


/// Helper to find a safe char boundary at or before index.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let mut i = index;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

/// Enforce a model-facing output ceiling before admitting any tool result into conversation history.
/// If oversized, persists the raw output to ArtifactStore (if available) and returns a bounded preview
/// with the artifact ID and continuation guidance.
pub fn admit_tool_output(
    tool_name: &str,
    raw_output: &str,
    store: Option<&dume_store::ArtifactStore>,
) -> String {
    let (head_tail_str, truncated) = truncate_output_head_tail(
        raw_output,
        DEFAULT_MAX_OUTPUT_BYTES,
        DEFAULT_HEAD_LINES,
        DEFAULT_TAIL_LINES,
    );

    // If fits within ceiling, admit directly
    if !truncated && raw_output.len() <= DEFAULT_MAX_OUTPUT_BYTES {
        return raw_output.to_string();
    }

    let original_size = raw_output.len();

    // Persist full output to artifact store if available
    let artifact_ref = if let Some(artifact_store) = store {
        match artifact_store.save_artifact(raw_output.as_bytes()) {
            Ok(hash) => Some(hash),
            Err(e) => {
                tracing::warn!("Failed to persist tool output artifact: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Build bounded preview respecting max byte limit and UTF-8 char boundary
    let max_preview = DEFAULT_MAX_OUTPUT_BYTES.saturating_sub(1024);
    let safe_len = floor_char_boundary(&head_tail_str, max_preview.min(head_tail_str.len()));
    let preview_slice = &head_tail_str[..safe_len];

    match artifact_ref {
        Some(hash) => {
            format!(
                "[Output Ceiling Exceeded for tool '{}': {} bytes total]\nFull raw output persisted to Artifact ID: {}\nRetrieve further sections with 'read_artifact' using path: {}\n\n--- Bounded Output Preview ---\n{}\n--- End of Preview ---",
                tool_name, original_size, hash, hash, preview_slice
            )
        }
        None => {
            format!(
                "[Output Ceiling Exceeded for tool '{}': {} bytes total]\n[Notice: Artifact preservation was unavailable or failed. Output truncated to bounded limit.]\n\n--- Bounded Output Preview ---\n{}\n--- End of Preview ---",
                tool_name, original_size, preview_slice
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_output_not_truncated() {
        let text = "line 1\nline 2\nline 3";
        let (out, truncated) = truncate_output_head_tail(text, 1024, 10, 10);
        assert!(!truncated);
        assert_eq!(out, text);
    }

    #[test]
    fn test_large_line_count_truncated_with_head_tail() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {}", i)).collect();
        let text = lines.join("\n");
        let (out, truncated) = truncate_output_head_tail(&text, 100_000, 5, 5);
        assert!(truncated);
        assert!(out.starts_with("line 1\nline 2"));
        assert!(out.contains("Truncated 90 lines / total 100 lines"));
        assert!(out.ends_with("line 100\n"));
    }

    #[test]
    fn test_admit_tool_output_ceiling_and_artifact_preservation() {
        let dir = tempfile::tempdir().unwrap();
        let store = dume_store::ArtifactStore::new(dir.path()).unwrap();

        // 1. Output within limit
        let small = "small output";
        let admitted_small = admit_tool_output("bash", small, Some(&store));
        assert_eq!(admitted_small, small);

        // 2. Oversized output (> 50KB)
        let large = "x".repeat(70 * 1024);
        let admitted_large = admit_tool_output("bash", &large, Some(&store));
        assert!(admitted_large.contains("Output Ceiling Exceeded for tool 'bash'"));
        assert!(admitted_large.contains("Full raw output persisted to Artifact ID:"));

        // Extract artifact ID from message
        let id_marker = "Artifact ID: ";
        let id_start = admitted_large.find(id_marker).unwrap() + id_marker.len();
        let artifact_id = &admitted_large[id_start..id_start + 64];
        assert_eq!(artifact_id.len(), 64);

        // Retrieve raw from store using range
        let retrieved = store.get_artifact_range(artifact_id, 0, 100).unwrap();
        assert_eq!(retrieved.len(), 100);
        assert_eq!(retrieved, vec![b'x'; 100]);

        // 3. Fallback when store is None
        let admitted_no_store = admit_tool_output("bash", &large, None);
        assert!(admitted_no_store.contains("Artifact preservation was unavailable or failed"));
        assert!(!admitted_no_store.contains("Full raw output persisted to Artifact ID:"));
    }
}
