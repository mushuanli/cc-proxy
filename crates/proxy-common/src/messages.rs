//! Shared Anthropic `messages`-format parsing helpers.
//!
//! Both the store summary analyzer and the proxy-session client parsers need
//! to classify user messages and extract prompt text from an Anthropic
//! `messages` array. Centralizing here keeps protocol semantics in one place.

use serde_json::Value;

/// True if a message's content contains a `tool_result` block.
pub fn is_tool_result(msg: &Value) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
        .unwrap_or(false)
}

/// True if a user message is a real user prompt (not a tool result, system
/// reminder, or empty system-injected text).
pub fn is_real_user_prompt(msg: &Value) -> bool {
    let content = match msg.get("content").and_then(Value::as_array) {
        Some(c) => c,
        None => {
            // content may be a plain string in some edge cases
            let s = msg.get("content").and_then(Value::as_str).unwrap_or("");
            return !s.trim_start().starts_with("<system-reminder>") && !s.is_empty();
        }
    };

    // Must be all text blocks
    let all_text = content.iter().all(|b| {
        matches!(
            b.get("type").and_then(Value::as_str),
            Some("text" | "input_text")
        )
    });
    if !all_text {
        return false;
    }

    // None of the blocks should be tool_result
    if content
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        return false;
    }

    // Filter out system-reminder blocks, then check if any real text remains
    let text = content
        .iter()
        .filter_map(block_text)
        .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
        .collect::<Vec<_>>()
        .join("\n");

    !text.trim().is_empty()
}

/// Extract the joined user text from a message, skipping system-reminder blocks.
pub fn extract_user_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                if matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("text" | "input_text")
                ) {
                    block_text(b)
                } else {
                    None
                }
            })
            .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => {
            if s.trim_start().starts_with("<system-reminder>") {
                String::new()
            } else {
                s.clone()
            }
        }
        _ => String::new(),
    }
}

fn block_text(block: &Value) -> Option<&str> {
    block
        .get("text")
        .or_else(|| block.get("input_text"))
        .or_else(|| block.get("output_text"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_user_prompt_detection() {
        let real = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "Real prompt"}]
        });
        assert!(is_real_user_prompt(&real));

        let reminder = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "<system-reminder>context</system-reminder>"}]
        });
        assert!(!is_real_user_prompt(&reminder));

        let tool = serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1"}]
        });
        assert!(!is_real_user_prompt(&tool));
    }

    #[test]
    fn extracts_user_text() {
        let msg = serde_json::json!({
            "content": [
                {"type": "text", "text": "<system-reminder>x</system-reminder>"},
                {"type": "text", "text": "hello"}
            ]
        });
        assert_eq!(extract_user_text(&msg), "hello");
    }
}
