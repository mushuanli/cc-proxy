use proxy_common::SseEvent;
use serde_json::Value;

/// Parses a Server-Sent Events byte stream into structured `SseEvent`s.
pub struct SseParser {
    buffer: Vec<u8>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed a chunk of bytes; returns completed SSE events.
    /// SSE format: lines ending in \n\n delimit events.
    /// Fields: `event: <type>\n`, `data: <json>\n`
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(pos) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let raw = self.buffer.drain(..=pos + 1).collect::<Vec<_>>();
            if let Some(event) = Self::parse_event_block(&raw) {
                events.push(event);
            }
        }
        // Also handle \r\n\r\n
        while let Some(pos) = self
            .buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            let raw = self.buffer.drain(..=pos + 3).collect::<Vec<_>>();
            if let Some(event) = Self::parse_event_block(&raw) {
                events.push(event);
            }
        }

        events
    }

    fn parse_event_block(raw: &[u8]) -> Option<SseEvent> {
        let text = String::from_utf8_lossy(raw);
        let mut event_type: Option<String> = None;
        let mut data: Option<String> = None;

        for line in text.lines() {
            if let Some(value) = line.strip_prefix("event: ") {
                event_type = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("event:") {
                event_type = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("data:") {
                data = Some(value.trim().to_string());
            }
        }

        if event_type.is_some() || data.is_some() {
            Some(SseEvent {
                event_type,
                data,
            })
        } else {
            None
        }
    }

    /// Parse Anthropic-specific SSE event data fields.
    pub fn parse_message_data(&self, data: &str) -> Option<Value> {
        serde_json::from_str(data).ok()
    }

    /// Extract stream event type from parsed data.
    pub fn event_kind<'a>(&self, data: &'a Value) -> Option<&'a str> {
        data.get("type").and_then(|v| v.as_str())
    }

    /// Extract content block delta text (for displaying in conversation view).
    /// Handles both `text_delta` and `thinking_delta` types.
    pub fn delta_text<'a>(&self, data: &'a Value) -> Option<&'a str> {
        let delta = data.get("delta").and_then(|d| d.get("type"))?;
        match delta.as_str()? {
            "text_delta" => data
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str()),
            "thinking_delta" => data
                .get("delta")
                .and_then(|d| d.get("thinking"))
                .and_then(|t| t.as_str()),
            _ => None,
        }
    }

    /// Extract output_tokens from message_delta event.
    /// Note: message_delta.usage only contains output_tokens, not input_tokens.
    pub fn output_tokens_from_delta(&self, data: &Value) -> Option<u32> {
        data.get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
    }

    /// Extract cache_creation_input_tokens from message_start event.
    pub fn cache_creation_tokens_from_start(&self, data: &Value) -> Option<u32> {
        data.get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
    }

    /// Extract cache_read_input_tokens from message_start event.
    pub fn cache_read_tokens_from_start(&self, data: &Value) -> Option<u32> {
        data.get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
    }

    /// Extract stop_reason from message_delta event.
    pub fn stop_reason<'a>(&self, data: &'a Value) -> Option<&'a str> {
        data.get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(|s| s.as_str())
    }

    /// Extract message id from message_start event.
    pub fn message_id<'a>(&self, data: &'a Value) -> Option<&'a str> {
        data.get("message")
            .and_then(|m| m.get("id"))
            .and_then(|id| id.as_str())
    }

    /// Extract model name from message_start event.
    pub fn model_from_start<'a>(&self, data: &'a Value) -> Option<&'a str> {
        data.get("message")
            .and_then(|m| m.get("model"))
            .and_then(|m| m.as_str())
    }

    /// Extract input_tokens from message_start event.
    pub fn input_tokens_from_start(&self, data: &Value) -> Option<u32> {
        data.get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("input_tokens"))
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_event() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type.as_deref(), Some("message_start"));
    }

    #[test]
    fn parse_partial_chunks() {
        let mut parser = SseParser::new();
        let ev1 = parser.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\nevent: delta\ndata: {\"t");
        assert_eq!(ev1.len(), 1);
        let ev2 = parser.feed(b"ype\":\"delta\"}\n\n");
        assert_eq!(ev2.len(), 1);
        assert_eq!(ev2[0].event_type.as_deref(), Some("delta"));
    }

    #[test]
    fn parse_empty_chunk() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"");
        assert!(events.is_empty());
    }

    #[test]
    fn parse_no_event_block() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"just some text\nwithout event format\n");
        assert!(events.is_empty());
    }

    #[test]
    fn extracts_anthropic_stream_usage() {
        let parser = SseParser::new();
        let start = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_123",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 42,
                    "cache_creation_input_tokens": 100,
                    "cache_read_input_tokens": 200
                }
            }
        });
        let delta = serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 321 }
        });

        assert_eq!(parser.message_id(&start), Some("msg_123"));
        assert_eq!(parser.model_from_start(&start), Some("claude-sonnet-4-6"));
        assert_eq!(parser.input_tokens_from_start(&start), Some(42));
        assert_eq!(parser.cache_creation_tokens_from_start(&start), Some(100));
        assert_eq!(parser.cache_read_tokens_from_start(&start), Some(200));
        assert_eq!(parser.output_tokens_from_delta(&delta), Some(321));
        assert_eq!(parser.stop_reason(&delta), Some("end_turn"));
    }

    #[test]
    fn output_usage_is_not_nested_under_delta() {
        let parser = SseParser::new();
        let malformed = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn",
                "usage": { "output_tokens": 321 }
            }
        });

        assert_eq!(parser.output_tokens_from_delta(&malformed), None);
    }
}
