use crate::models::{NormalizedResponse, ToolCallRecord, ToolResultRecord};
use serde_json::Value;

/// Sanitize text by removing control characters except \n, \r, \t.
pub fn sanitize_text(input: &str) -> String {
    input
        .chars()
        .filter(|ch| matches!(ch, '\n' | '\r' | '\t') || !ch.is_control())
        .collect()
}

/// Normalize a raw SSE-accumulated response into a structured form.
///
/// Expects input to be a JSON value containing Anthropic message delta/content
/// blocks. Returns a NormalizedResponse with thinking, text, tool_calls, and
/// tool_results separated.
pub fn normalize_response(raw: &Value) -> NormalizedResponse {
    let mut result = NormalizedResponse::default();

    // Handle the common Anthropic content block structure
    let content_blocks = raw
        .get("content")
        .and_then(|c| c.as_array())
        .or_else(|| raw.as_array());

    let blocks = match content_blocks {
        Some(b) => b,
        None => {
            // Fallback: treat the entire value as text
            let text = sanitize_text(&raw.to_string());
            if !text.is_empty() && text != "null" {
                result.text.push(text);
            }
            return result;
        }
    };

    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match block_type {
            "thinking" => {
                if let Some(thinking) = block.get("thinking").and_then(|t| t.as_str()) {
                    result.thinking.push(sanitize_text(thinking));
                }
            }
            "text" | "text_delta" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    result.text.push(sanitize_text(text));
                }
            }
            "tool_use" => {
                result.tool_calls.push(ToolCallRecord {
                    id: block
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                });
            }
            "tool_result" => {
                let content = block
                    .get("content")
                    .map(|c| match c {
                        Value::String(s) => sanitize_text(s),
                        other => sanitize_text(&other.to_string()),
                    })
                    .unwrap_or_default();

                result.tool_results.push(ToolResultRecord {
                    tool_use_id: block
                        .get("tool_use_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    content,
                });
            }
            _ => {
                // Unknown block type: capture as text
                let text = sanitize_text(&block.to_string());
                if !text.is_empty() {
                    result.text.push(text);
                }
            }
        }
    }

    result
}

/// Sanitize every string field in a NormalizedResponse.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_null_byte() {
        assert_eq!(sanitize_text("hello\0world"), "helloworld");
    }

    #[test]
    fn sanitize_keeps_newlines() {
        assert_eq!(
            sanitize_text("line1\nline2\r\nline3"),
            "line1\nline2\r\nline3"
        );
    }

    #[test]
    fn normalize_text_block() {
        let raw = serde_json::json!({
            "content": [
                {"type": "text", "text": "Hello, world!"}
            ]
        });
        let resp = normalize_response(&raw);
        assert_eq!(resp.text, vec!["Hello, world!"]);
        assert!(resp.thinking.is_empty());
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn normalize_tool_use() {
        let raw = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "toolu_001", "name": "Read", "input": {"file_path": "src/main.rs"}}
            ]
        });
        let resp = normalize_response(&raw);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "Read");
        assert_eq!(resp.tool_calls[0].id, "toolu_001");
    }
}
