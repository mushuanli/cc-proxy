use proxy_common::SseEvent;
use serde_json::Value;

/// Parses a Server-Sent Events byte stream into structured `SseEvent`s.
pub struct SseParser {
    buffer: Vec<u8>,
    truncated: bool,
}

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            truncated: false,
        }
    }

    /// Feed a chunk of bytes; returns completed SSE events.
    /// SSE format: lines ending in \n\n delimit events.
    /// Fields: `event: <type>\n`, `data: <json>\n`
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_SSE_EVENT_BYTES {
            self.buffer.clear();
            self.truncated = true;
            return Vec::new();
        }
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(pos) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let raw = self.buffer.drain(..=pos + 1).collect::<Vec<_>>();
            if let Some(event) = Self::parse_event_block(&raw) {
                events.push(event);
            }
        }
        // Also handle \r\n\r\n
        while let Some(pos) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let raw = self.buffer.drain(..=pos + 3).collect::<Vec<_>>();
            if let Some(event) = Self::parse_event_block(&raw) {
                events.push(event);
            }
        }

        events
    }

    pub fn was_truncated(&self) -> bool {
        self.truncated
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
            Some(SseEvent { event_type, data })
        } else {
            None
        }
    }

    /// Parse Anthropic-specific SSE event data fields.
    pub fn parse_message_data(&self, data: &str) -> Option<Value> {
        serde_json::from_str(data).ok()
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
        let ev1 =
            parser.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\nevent: delta\ndata: {\"t");
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
}
