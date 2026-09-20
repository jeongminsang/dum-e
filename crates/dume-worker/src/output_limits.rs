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
}
