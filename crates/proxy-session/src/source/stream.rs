//! Stateful streaming tool parser.
//!
//! SSE tool_use blocks arrive as three events:
//!   content_block_start  → tool_use_id + name (input={})
//!   content_block_delta  → input_json_delta (partial input, index-keyed)
//!   content_block_stop   → input complete
//!
//! This parser tracks in-flight blocks across events so a caller (relay)
//! can emit `ToolEmitted` / `ToolInputComplete` observations without owning
//! any protocol knowledge.

use serde_json::Value;

use crate::ingest::observation::ObservationKind;

/// Tracks tool_use blocks currently streaming in a response.
#[derive(Debug, Default)]
pub struct ToolStreamParser {
    /// block index → (tool_use_id, name, accumulated input)
    blocks: Vec<Option<(String, String, String)>>,
}

impl ToolStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one parsed SSE event; returns observations produced by it.
    ///
    /// Only handles the three tool-related event kinds. Returns empty for
    /// everything else so the caller can keep handling tokens/text itself.
    pub fn feed(&mut self, parsed: &Value) -> Vec<ObservationKind> {
        match parsed.get("type").and_then(Value::as_str) {
            Some("content_block_start") => self.on_block_start(parsed),
            Some("content_block_delta") => self.on_block_delta(parsed),
            Some("content_block_stop") => self.on_block_stop(parsed),
            _ => Vec::new(),
        }
    }

    /// Drop any in-flight blocks (stream ended without a stop event).
    /// Returns them marked abandoned so the caller can record them.
    pub fn finish(&mut self) -> Vec<ObservationKind> {
        let mut out = Vec::new();
        for slot in self.blocks.drain(..) {
            if let Some((tool_use_id, _, _)) = slot {
                out.push(ObservationKind::ToolResult {
                    tool_use_id,
                    raw_result_preview: None,
                    effective_result_preview: None,
                    status: "abandoned".into(),
                });
            }
        }
        out
    }

    fn on_block_start(&mut self, parsed: &Value) -> Vec<ObservationKind> {
        let Some(block) = parsed.get("content_block") else {
            return Vec::new();
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return Vec::new();
        }
        let (Some(tool_use_id), Some(name)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        let index = parsed
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.blocks.resize(index + 1, None);
        self.blocks[index] = Some((tool_use_id.to_string(), name.to_string(), String::new()));
        vec![ObservationKind::ToolEmitted {
            call_id: String::new(), // filled by caller's StreamCtx
            tool_use_id: tool_use_id.to_string(),
            tool_name: name.to_string(),
            started_at: chrono::Utc::now().timestamp_millis(),
        }]
    }

    fn on_block_delta(&mut self, parsed: &Value) -> Vec<ObservationKind> {
        let delta = parsed.get("delta");
        if delta
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            != Some("input_json_delta")
        {
            return Vec::new();
        }
        let index = parsed
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if let Some(Some((_, _, acc))) = self.blocks.get_mut(index) {
            if let Some(partial) = delta
                .and_then(|d| d.get("partial_json"))
                .and_then(Value::as_str)
            {
                acc.push_str(partial);
            }
        }
        Vec::new()
    }

    fn on_block_stop(&mut self, parsed: &Value) -> Vec<ObservationKind> {
        let index = parsed
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let Some(slot) = self.blocks.get_mut(index) else {
            return Vec::new();
        };
        let Some((tool_use_id, _, input)) = slot else {
            return Vec::new();
        };
        let out = vec![ObservationKind::ToolInputComplete {
            tool_use_id: tool_use_id.clone(),
            input_json: std::mem::take(input),
        }];
        *slot = None;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(parsed: Value) -> Vec<ObservationKind> {
        ToolStreamParser::new().feed(&parsed)
    }

    #[test]
    fn parses_tool_emit() {
        let kinds = ev(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}
        }));
        assert_eq!(kinds.len(), 1);
        assert!(matches!(&kinds[0], ObservationKind::ToolEmitted { tool_use_id, tool_name, .. }
            if tool_use_id == "call_1" && tool_name == "Bash"));
    }

    #[test]
    fn ignores_non_tool_block_start() {
        let kinds = ev(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": "hi"}
        }));
        assert!(kinds.is_empty());
    }

    #[test]
    fn accumulates_input_across_deltas() {
        let mut p = ToolStreamParser::new();
        p.feed(&serde_json::json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {}}
        }));
        p.feed(&serde_json::json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":\"ls\"}"}
        }));
        let kinds = p.feed(&serde_json::json!({
            "type": "content_block_stop", "index": 0
        }));
        assert_eq!(kinds.len(), 1);
        match &kinds[0] {
            ObservationKind::ToolInputComplete { tool_use_id, input_json } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(input_json, r#"{"cmd":"ls"}"#);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
