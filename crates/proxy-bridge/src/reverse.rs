//! Anthropic Messages SSE → OpenAI Responses SSE.
//!
//! The inverse of [`crate::stream`]: a Codex-speaking client is served by an
//! upstream that only speaks Anthropic.
//!
//! Responses is item-oriented, so the state machine tracks one *output item* per
//! Anthropic content block and assigns it an `output_index`, mirroring the
//! `content_block` index on the Anthropic side. A text block becomes an
//! `output_text` part inside a `message` item; a tool block becomes a
//! `function_call` item.

use proxy_common::protocol::ResponseTranslator;
use serde_json::{json, Value};

/// The item currently being streamed.
#[derive(Debug, Clone, PartialEq)]
enum OpenItem {
    Message {
        output_index: i64,
        item_id: String,
        text: String,
    },
    FunctionCall {
        output_index: i64,
        item_id: String,
        call_id: String,
        name: String,
        /// Input carried by `content_block_start`, used when the upstream never
        /// sends an `input_json_delta` for this block.
        start_arguments: String,
        /// Concatenated `input_json_delta` payloads.
        arguments: String,
        /// Whether an incremental delta was seen.
        saw_delta: bool,
    },
}

impl OpenItem {
    /// Arguments to report: the incremental stream when there was one, the
    /// start block's own `input` otherwise. Returns `None` for a text item.
    fn function_arguments(&self) -> Option<String> {
        match self {
            OpenItem::FunctionCall {
                start_arguments,
                arguments,
                saw_delta,
                ..
            } => Some(if *saw_delta {
                arguments.clone()
            } else {
                start_arguments.clone()
            }),
            OpenItem::Message { .. } => None,
        }
    }
}

impl OpenItem {
    fn output_index(&self) -> i64 {
        match self {
            OpenItem::Message { output_index, .. }
            | OpenItem::FunctionCall { output_index, .. } => *output_index,
        }
    }

    /// The item as it appears in `response.output`.
    fn to_item(&self) -> Value {
        match self {
            OpenItem::Message { item_id, text, .. } => json!({
                "id": item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }),
            OpenItem::FunctionCall {
                item_id,
                call_id,
                name,
                arguments,
                ..
            } => json!({
                "id": item_id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }),
        }
    }
}

/// Translates an Anthropic event stream into the Responses event stream.
#[derive(Debug)]
pub struct ResponsesStreamTranslator {
    response_id: String,
    model: String,
    sequence: u64,
    next_output_index: i64,
    open: Option<OpenItem>,
    completed_items: Vec<Value>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    stop_reason: Option<String>,
    /// An upstream event was actually seen, so a response is in flight.
    started: bool,
    failed: bool,
    finished: bool,
}

impl Default for ResponsesStreamTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesStreamTranslator {
    pub fn new() -> Self {
        Self {
            response_id: String::new(),
            model: String::new(),
            sequence: 0,
            next_output_index: 0,
            open: None,
            completed_items: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            stop_reason: None,
            started: false,
            failed: false,
            finished: false,
        }
    }

    fn emit(out: &mut Vec<Vec<u8>>, sequence: &mut u64, event: &str, mut payload: Value) {
        payload["sequence_number"] = json!(*sequence);
        *sequence += 1;
        out.push(format!("event: {event}\ndata: {payload}\n\n").into_bytes());
    }

    fn response_id(&self) -> String {
        if self.response_id.is_empty() {
            format!("resp_{}", uuid::Uuid::now_v7().simple())
        } else {
            self.response_id.clone()
        }
    }

    fn usage(&self) -> Value {
        responses_usage(
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
        )
    }

    fn response_snapshot(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.response_id(),
            "object": "response",
            "status": status,
            "model": self.model,
            "output": output,
            "usage": self.usage(),
        })
    }

    /// Open the response exactly once, before any content event.
    ///
    /// Content events can arrive without a preceding `message_start` (a truncated
    /// or hand-rolled upstream stream), and a Responses client that sees
    /// `output_item.added` before `response.created` cannot build a response
    /// object.
    fn ensure_created(&mut self, out: &mut Vec<Vec<u8>>) {
        if !self.started {
            self.emit_created(out);
        }
    }

    fn emit_created(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        self.started = true;
        let snapshot = self.response_snapshot("in_progress", Vec::new());
        Self::emit(
            out,
            &mut self.sequence,
            "response.created",
            json!({"type": "response.created", "response": snapshot}),
        );
        let snapshot = self.response_snapshot("in_progress", Vec::new());
        Self::emit(
            out,
            &mut self.sequence,
            "response.in_progress",
            json!({"type": "response.in_progress", "response": snapshot}),
        );
    }

    /// Close the open item, emitting its terminal events.
    fn close_item(&mut self, out: &mut Vec<Vec<u8>>) {
        let Some(item) = self.open.take() else {
            return;
        };
        let output_index = item.output_index();
        match &item {
            OpenItem::Message { text, item_id, .. } => {
                Self::emit(
                    out,
                    &mut self.sequence,
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "text": text,
                    }),
                );
            }
            OpenItem::FunctionCall { item_id, .. } => {
                // Anthropic's spec puts the whole `input` in
                // `content_block_start`; an upstream that streams no deltas must
                // still produce non-empty arguments.
                let arguments = item.function_arguments().unwrap_or_default();
                Self::emit(
                    out,
                    &mut self.sequence,
                    "response.function_call_arguments.done",
                    json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "arguments": arguments,
                    }),
                );
            }
        }
        let mut snapshot = item.to_item();
        if let (OpenItem::FunctionCall { .. }, Some(arguments)) = (&item, item.function_arguments())
        {
            snapshot["arguments"] = json!(arguments);
        }
        Self::emit(
            out,
            &mut self.sequence,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": snapshot,
            }),
        );
        self.completed_items.push(snapshot);
    }

    fn open_message(&mut self, out: &mut Vec<Vec<u8>>) {
        self.ensure_created(out);
        if matches!(self.open, Some(OpenItem::Message { .. })) {
            return;
        }
        self.close_item(out);
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("msg_{}", uuid::Uuid::now_v7().simple());
        Self::emit(
            out,
            &mut self.sequence,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        );
        Self::emit(
            out,
            &mut self.sequence,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }),
        );
        self.open = Some(OpenItem::Message {
            output_index,
            item_id,
            text: String::new(),
        });
    }

    fn open_function_call(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        call_id: &str,
        name: &str,
        start_arguments: &str,
    ) {
        self.ensure_created(out);
        if matches!(self.open, Some(OpenItem::FunctionCall { .. })) {
            return;
        }
        self.close_item(out);
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("fc_{}", uuid::Uuid::now_v7().simple());
        Self::emit(
            out,
            &mut self.sequence,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                },
            }),
        );
        self.open = Some(OpenItem::FunctionCall {
            output_index,
            item_id,
            call_id: call_id.to_string(),
            name: name.to_string(),
            start_arguments: start_arguments.to_string(),
            arguments: String::new(),
            saw_delta: false,
        });
    }

    fn text_delta(&mut self, out: &mut Vec<Vec<u8>>, text: &str) {
        if text.is_empty() {
            return;
        }
        self.open_message(out);
        let Some(OpenItem::Message {
            output_index,
            item_id,
            text: accumulated,
        }) = self.open.as_mut()
        else {
            return;
        };
        accumulated.push_str(text);
        let output_index = *output_index;
        let item_id = item_id.clone();
        let text = text.to_string();
        Self::emit(
            out,
            &mut self.sequence,
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta",
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": text,
            }),
        );
    }

    fn arguments_delta(&mut self, out: &mut Vec<Vec<u8>>, partial: &str) {
        if partial.is_empty() {
            return;
        }
        let Some(OpenItem::FunctionCall {
            output_index,
            item_id,
            arguments,
            saw_delta,
            ..
        }) = self.open.as_mut()
        else {
            return;
        };
        arguments.push_str(partial);
        *saw_delta = true;
        let output_index = *output_index;
        let item_id = item_id.clone();
        let partial = partial.to_string();
        Self::emit(
            out,
            &mut self.sequence,
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "delta": partial,
            }),
        );
    }

    fn absorb_message_start(&mut self, message: &Value) {
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            self.response_id = id.to_string();
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        self.absorb_usage(message.get("usage"));
    }

    fn absorb_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = value;
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read_tokens = value;
        }
    }

    /// Responses has no tool-specific stop reason; `tool_calls` is the analogue.
    fn responses_stop_reason(&self) -> &'static str {
        responses_stop_reason(self.stop_reason.as_deref())
    }

    /// Terminal failure: open the response first when the error arrives before
    /// `message_start`, then report it with a full response object.
    fn emit_failed(&mut self, out: &mut Vec<Vec<u8>>, message: &str) {
        if self.finished {
            return;
        }
        self.ensure_created(out);
        self.failed = true;
        self.finished = true;
        self.close_item(out);
        let mut snapshot = self.response_snapshot("failed", self.completed_items.clone());
        snapshot["error"] = json!({"code": "api_error", "message": message});
        Self::emit(
            out,
            &mut self.sequence,
            "response.failed",
            json!({"type": "response.failed", "response": snapshot}),
        );
    }

    fn finish_response(&mut self, out: &mut Vec<Vec<u8>>, status: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.close_item(out);
        let snapshot = {
            let mut snapshot = self.response_snapshot(status, self.completed_items.clone());
            snapshot["stop_reason"] = json!(self.responses_stop_reason());
            snapshot
        };
        Self::emit(
            out,
            &mut self.sequence,
            "response.completed",
            json!({"type": "response.completed", "response": snapshot}),
        );
    }

    fn handle_event(&mut self, payload: &Value, out: &mut Vec<Vec<u8>>) {
        match payload.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(message) = payload.get("message") {
                    self.absorb_message_start(message);
                }
                self.ensure_created(out);
            }
            "content_block_start" => {
                let Some(block) = payload.get("content_block") else {
                    return;
                };
                match block.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => self.open_message(out),
                    "tool_use" => {
                        // Anthropic normally sends the full `input` here; it is
                        // used when no `input_json_delta` follows for the block.
                        let start_arguments = block
                            .get("input")
                            .filter(|input| !input.is_null())
                            .map(Value::to_string);
                        self.open_function_call(
                            out,
                            block.get("id").and_then(Value::as_str).unwrap_or_default(),
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            start_arguments.as_deref().unwrap_or(""),
                        )
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(delta) = payload.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        self.text_delta(out, text);
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.arguments_delta(out, partial);
                    }
                    _ => {}
                }
            }
            "content_block_stop" => self.close_item(out),
            "message_delta" => {
                if let Some(reason) = payload
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
                self.absorb_usage(payload.get("usage"));
            }
            "message_stop" => self.finish_response(out, "completed"),
            "error" => {
                let message = payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_string();
                self.emit_failed(out, &message);
            }
            _ => {}
        }
    }
}

impl ResponseTranslator for ResponsesStreamTranslator {
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let text = std::str::from_utf8(event).unwrap_or("").trim();
        let text = text.strip_prefix("data:").map(str::trim).unwrap_or(text);
        let Ok(payload) = serde_json::from_str::<Value>(text) else {
            return out;
        };
        if !payload.is_object() {
            return out;
        }
        self.handle_event(&payload, &mut out);
        out
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        // Only close a response that was actually opened: a stream that never
        // produced an event must stay silent rather than fabricate a completion.
        if self.started && !self.finished && !self.failed {
            // Upstream closed without message_stop; close the response so the
            // client is not left waiting.
            self.finish_response(&mut out, "completed");
        }
        out
    }

    fn abort(&mut self, reason: &str) -> Vec<Vec<u8>> {
        // A truncated stream must not look like a finished one.
        let mut out = Vec::new();
        self.emit_failed(&mut out, reason);
        out
    }

    fn translate_complete(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let message: Value = serde_json::from_slice(body).ok()?;
        let mut output: Vec<Value> = Vec::new();

        for block in message.get("content")?.as_array()? {
            // One unrepresentable block must not discard the whole answer.
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    output.push(json!({
                        "id": format!("msg_{}", uuid::Uuid::now_v7().simple()),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                    }));
                }
                "tool_use" => output.push(json!({
                    "id": format!("fc_{}", uuid::Uuid::now_v7().simple()),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
                })),
                _ => {}
            }
        }

        self.stop_reason = message
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            self.response_id = id.to_string();
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        self.absorb_usage(message.get("usage"));
        self.finished = true;

        let mut snapshot = self.response_snapshot("completed", output);
        snapshot["stop_reason"] = json!(self.responses_stop_reason());
        serde_json::to_vec(&snapshot).ok()
    }

    fn stream_from_complete(&mut self, body: &[u8]) -> Vec<Vec<u8>> {
        let Ok(message) = serde_json::from_slice::<Value>(body) else {
            return self.abort("upstream returned a non-JSON body");
        };
        if let Some(error) = anthropic_error_message(&message) {
            return self.abort(&error);
        }
        if message.get("content").and_then(Value::as_array).is_none() {
            return self.abort("upstream returned no translatable content");
        }
        message_to_responses_events(&message)
    }
}

/// Responses-shaped usage, built in one place so the live and fallback paths
/// agree. `total_tokens` saturates: an upstream can report numbers that would
/// otherwise overflow (a debug build would panic on plain addition).
pub fn responses_usage(input_tokens: u64, output_tokens: u64, cache_read_tokens: u64) -> Value {
    let mut usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": input_tokens.saturating_add(output_tokens),
    });
    if cache_read_tokens > 0 {
        usage["input_tokens_details"] = json!({"cached_tokens": cache_read_tokens});
    }
    usage
}

/// Anthropic stop reason → Responses stop reason. Single mapping, used by both
/// the live stream and the whole-body fallback.
pub fn responses_stop_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "max_output_tokens",
        _ => "stop",
    }
}

/// Extract a failure message from an Anthropic error body, if this is one.
fn anthropic_error_message(message: &Value) -> Option<String> {
    if message.get("type").and_then(Value::as_str) != Some("error")
        && message.get("error").is_none()
    {
        return None;
    }
    let text = message
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| message.get("message").and_then(Value::as_str))
        .unwrap_or("upstream returned an error");
    Some(text.to_string())
}

/// Render a complete Anthropic message as a Responses event sequence.
///
/// Used when a client asked to stream but the upstream answered with a single
/// JSON body.
pub fn message_to_responses_events(message: &Value) -> Vec<Vec<u8>> {
    let response_id = message
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::now_v7().simple()));
    let model = message.get("model").and_then(Value::as_str).unwrap_or("");
    let usage = message
        .get("usage")
        .map(|usage| {
            responses_usage(
                usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        })
        .unwrap_or_else(|| responses_usage(0, 0, 0));

    let mut out = Vec::new();
    let mut sequence = 0u64;
    ResponsesStreamTranslator::emit(
        &mut out,
        &mut sequence,
        "response.created",
        json!({"type": "response.created", "response": {
            "id": response_id, "object": "response", "status": "in_progress",
            "model": model, "output": [], "usage": usage,
        }}),
    );

    let mut output: Vec<Value> = Vec::new();
    let mut output_index = 0i64;
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let item_id = format!("msg_{}", uuid::Uuid::now_v7().simple());
                for (event, payload) in [
                    (
                        "response.output_item.added",
                        json!({
                        "type": "response.output_item.added", "output_index": output_index,
                        "item": {"id": item_id, "type": "message", "status": "in_progress",
                                 "role": "assistant", "content": []}}),
                    ),
                    (
                        "response.content_part.added",
                        json!({
                        "type": "response.content_part.added", "item_id": item_id,
                        "output_index": output_index, "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}}),
                    ),
                    (
                        "response.output_text.delta",
                        json!({
                        "type": "response.output_text.delta", "item_id": item_id,
                        "output_index": output_index, "content_index": 0, "delta": text}),
                    ),
                    (
                        "response.output_text.done",
                        json!({
                        "type": "response.output_text.done", "item_id": item_id,
                        "output_index": output_index, "content_index": 0, "text": text}),
                    ),
                ] {
                    ResponsesStreamTranslator::emit(&mut out, &mut sequence, event, payload);
                }
                let item = json!({
                    "id": item_id, "type": "message", "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                });
                ResponsesStreamTranslator::emit(
                    &mut out,
                    &mut sequence,
                    "response.output_item.done",
                    json!({"type": "response.output_item.done",
                           "output_index": output_index, "item": item}),
                );
                output.push(item);
            }
            "tool_use" => {
                let item_id = format!("fc_{}", uuid::Uuid::now_v7().simple());
                let call_id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = block
                    .get("input")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".into());
                for (event, payload) in [
                    (
                        "response.output_item.added",
                        json!({
                        "type": "response.output_item.added", "output_index": output_index,
                        "item": {"id": item_id, "type": "function_call", "status": "in_progress",
                                 "call_id": call_id, "name": name, "arguments": ""}}),
                    ),
                    (
                        "response.function_call_arguments.delta",
                        json!({
                        "type": "response.function_call_arguments.delta", "item_id": item_id,
                        "output_index": output_index, "delta": arguments}),
                    ),
                    (
                        "response.function_call_arguments.done",
                        json!({
                        "type": "response.function_call_arguments.done", "item_id": item_id,
                        "output_index": output_index, "arguments": arguments}),
                    ),
                ] {
                    ResponsesStreamTranslator::emit(&mut out, &mut sequence, event, payload);
                }
                let item = json!({
                    "id": item_id, "type": "function_call", "status": "completed",
                    "call_id": call_id, "name": name, "arguments": arguments,
                });
                ResponsesStreamTranslator::emit(
                    &mut out,
                    &mut sequence,
                    "response.output_item.done",
                    json!({"type": "response.output_item.done",
                           "output_index": output_index, "item": item}),
                );
                output.push(item);
            }
            _ => continue,
        }
        output_index += 1;
    }

    let stop_reason = responses_stop_reason(message.get("stop_reason").and_then(Value::as_str));
    ResponsesStreamTranslator::emit(
        &mut out,
        &mut sequence,
        "response.completed",
        json!({"type": "response.completed", "response": {
            "id": response_id, "object": "response", "status": "completed",
            "model": model, "output": output, "usage": usage, "stop_reason": stop_reason,
        }}),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_of(frames: &[Vec<u8>]) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                let data = text.lines().find_map(|l| l.strip_prefix("data: "))?;
                serde_json::from_str(data).ok()
            })
            .collect()
    }

    fn event_types(frames: &[Vec<u8>]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                text.lines()
                    .find_map(|l| l.strip_prefix("event: "))
                    .map(str::to_string)
            })
            .collect()
    }

    fn feed(translator: &mut ResponsesStreamTranslator, events: &[Value]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.push(event.to_string().as_bytes()));
        }
        out
    }

    #[test]
    fn a_text_stream_becomes_the_responses_sequence() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {
                    "id": "msg_1", "model": "claude-sonnet",
                    "usage": {"input_tokens": 9, "output_tokens": 0}}}),
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "Hel"}}),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": "lo"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                       "usage": {"output_tokens": 2}}),
                json!({"type": "message_stop"}),
            ],
        );

        let types = event_types(&frames);
        assert_eq!(types[0], "response.created");
        assert_eq!(types[1], "response.in_progress");
        assert!(types.contains(&"response.output_text.delta".to_string()));
        assert_eq!(types.last().unwrap(), "response.completed");

        let payloads = data_of(&frames);
        let created = &payloads[0];
        assert_eq!(created["response"]["id"], "msg_1");
        assert_eq!(created["response"]["model"], "claude-sonnet");

        let deltas: Vec<&Value> = payloads
            .iter()
            .filter(|p| p["type"] == "response.output_text.delta")
            .collect();
        assert_eq!(deltas[0]["delta"], "Hel");
        assert_eq!(deltas[1]["delta"], "lo");

        let done = payloads
            .iter()
            .find(|p| p["type"] == "response.output_text.done")
            .unwrap();
        assert_eq!(done["text"], "Hello");

        let completed = payloads.last().unwrap();
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 9);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 2);
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m"}}),
                json!({"type": "content_block_start", "content_block": {"type": "text"}}),
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "x"}}),
                json!({"type": "content_block_stop"}),
                json!({"type": "message_stop"}),
            ],
        );
        let numbers: Vec<u64> = data_of(&frames)
            .iter()
            .filter_map(|p| p["sequence_number"].as_u64())
            .collect();
        assert!(!numbers.is_empty());
        assert!(
            numbers.windows(2).all(|w| w[1] == w[0] + 1),
            "sequence numbers must increment by one: {numbers:?}"
        );
    }

    #[test]
    fn a_tool_use_block_becomes_a_function_call_item() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m", "model": "c"}}),
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "tool_use", "id": "toolu_9", "name": "Read", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "input_json_delta", "partial_json": "th\":\"/a\"}"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
                json!({"type": "message_stop"}),
            ],
        );

        let payloads = data_of(&frames);
        let added = payloads
            .iter()
            .find(|p| p["type"] == "response.output_item.added")
            .unwrap();
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["call_id"], "toolu_9");
        assert_eq!(added["item"]["name"], "Read");

        let arg_done = payloads
            .iter()
            .find(|p| p["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(arg_done["arguments"], "{\"path\":\"/a\"}");

        let completed = payloads.last().unwrap();
        assert_eq!(
            completed["response"]["stop_reason"], "tool_calls",
            "Anthropic tool_use maps to the Responses tool_calls stop reason"
        );
    }

    #[test]
    fn text_then_tool_use_gets_distinct_output_indices() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m"}}),
                json!({"type": "content_block_start", "content_block": {"type": "text"}}),
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "hi"}}),
                json!({"type": "content_block_stop"}),
                json!({"type": "content_block_start",
                       "content_block": {"type": "tool_use", "id": "t1", "name": "Bash"}}),
                json!({"type": "content_block_delta",
                       "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
                json!({"type": "content_block_stop"}),
                json!({"type": "message_stop"}),
            ],
        );
        let payloads = data_of(&frames);
        let added: Vec<&Value> = payloads
            .iter()
            .filter(|p| p["type"] == "response.output_item.added")
            .collect();
        assert_eq!(added.len(), 2);
        assert_eq!(added[0]["output_index"], 0);
        assert_eq!(added[1]["output_index"], 1);

        let completed = payloads.last().unwrap();
        assert_eq!(completed["response"]["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_error_event_becomes_response_failed() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m"}}),
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
            ],
        );
        let types = event_types(&frames);
        assert_eq!(types.last().unwrap(), "response.failed");
        let payloads = data_of(&frames);
        assert_eq!(
            payloads.last().unwrap()["response"]["error"]["message"],
            "busy"
        );
    }

    #[test]
    fn non_json_frames_are_ignored() {
        let mut translator = ResponsesStreamTranslator::new();
        assert!(translator.push(b"[DONE]").is_empty());
        assert!(translator.push(b": ping").is_empty());
        assert!(translator.push(b"").is_empty());
    }

    #[test]
    fn data_prefixed_frames_are_accepted() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = translator.push(
            br#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"z"}}"#,
        );
        // A delta with no preceding block still opens the message item.
        assert!(!frames.is_empty());
    }

    #[test]
    fn finish_closes_a_stream_that_never_ended() {
        let mut translator = ResponsesStreamTranslator::new();
        feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m"}}),
                json!({"type": "content_block_start", "content_block": {"type": "text"}}),
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "half"}}),
            ],
        );
        let types = event_types(&translator.finish());
        assert!(types.contains(&"response.output_text.done".to_string()));
        assert_eq!(types.last().unwrap(), "response.completed");
        assert!(translator.finish().is_empty(), "must not double-terminate");
    }

    #[test]
    fn finish_on_an_untouched_stream_emits_nothing() {
        let mut translator = ResponsesStreamTranslator::new();
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn non_streaming_body_becomes_a_complete_response() {
        let mut translator = ResponsesStreamTranslator::new();
        let body = json!({
            "id": "msg_5", "model": "claude-sonnet", "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "toolu_2", "name": "Grep", "input": {"pattern": "x"}}
            ],
            "usage": {"input_tokens": 6, "output_tokens": 3, "cache_read_input_tokens": 1}
        });
        let out = translator
            .translate_complete(body.to_string().as_bytes())
            .unwrap();
        let response: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(response["id"], "msg_5");
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["stop_reason"], "tool_calls");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "looking");
        assert_eq!(response["output"][1]["type"], "function_call");
        assert_eq!(response["output"][1]["call_id"], "toolu_2");
        assert_eq!(response["output"][1]["arguments"], "{\"pattern\":\"x\"}");
        assert_eq!(response["usage"]["input_tokens"], 6);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            1
        );
    }

    #[test]
    fn non_streaming_malformed_body_returns_none() {
        let mut translator = ResponsesStreamTranslator::new();
        assert!(translator.translate_complete(b"nope").is_none());
        assert!(translator
            .translate_complete(json!({"id": "m"}).to_string().as_bytes())
            .is_none());
    }

    #[test]
    fn a_non_sse_body_is_rendered_as_an_event_sequence() {
        let mut translator = ResponsesStreamTranslator::new();
        let body = json!({
            "id": "msg_off", "model": "claude-sonnet", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "offline"}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        });
        let frames = translator.stream_from_complete(body.to_string().as_bytes());
        let types = event_types(&frames);
        assert_eq!(types[0], "response.created");
        assert_eq!(types.last().unwrap(), "response.completed");

        let payloads = data_of(&frames);
        assert_eq!(
            payloads.last().unwrap()["response"]["output"][0]["content"][0]["text"],
            "offline"
        );
    }

    #[test]
    fn message_to_responses_events_handles_empty_content() {
        let frames = message_to_responses_events(&json!({
            "id": "m", "model": "c", "stop_reason": "end_turn",
            "content": [], "usage": {"input_tokens": 1, "output_tokens": 0}
        }));
        assert_eq!(
            event_types(&frames),
            vec!["response.created", "response.completed"]
        );
    }

    // ── defects found by the review ──

    #[test]
    fn a_stream_without_message_start_still_terminates() {
        // `started` used to be set only by `message_start`, so a stream that began
        // with content events never got a terminal `response.completed`.
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "content_block_start",
                       "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta",
                       "delta": {"type": "text_delta", "text": "hi"}}),
            ],
        );
        assert!(!frames.is_empty());
        let finished = translator.finish();
        let types = event_types(&finished);
        assert_eq!(
            types.last().map(String::as_str),
            Some("response.completed"),
            "the client must not be left waiting"
        );
    }

    #[test]
    fn content_events_are_preceded_by_response_created() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "content_block_start",
                       "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta",
                       "delta": {"type": "text_delta", "text": "hi"}}),
            ],
        );
        assert_eq!(event_types(&frames)[0], "response.created");
    }

    #[test]
    fn an_early_error_still_opens_the_response() {
        // A stream whose first frame is an error used to emit only
        // `response.failed`, with a response object missing object/model/output.
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[json!({"type": "error", "error": {"message": "overloaded"}})],
        );
        let types = event_types(&frames);
        assert_eq!(
            types,
            vec![
                "response.created",
                "response.in_progress",
                "response.failed"
            ]
        );
        let failed = data_of(&frames).pop().unwrap();
        assert_eq!(failed["response"]["error"]["message"], "overloaded");
        assert_eq!(failed["response"]["object"], "response");
        assert!(failed["response"]["output"].is_array());
    }

    #[test]
    fn abort_reports_failure_not_completion() {
        let mut translator = ResponsesStreamTranslator::new();
        feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m", "model": "c"}}),
                json!({"type": "content_block_start",
                       "content_block": {"type": "text", "text": ""}}),
            ],
        );
        let frames = translator.abort("connection reset");
        let types = event_types(&frames);
        assert_eq!(types.last().map(String::as_str), Some("response.failed"));
        assert!(!types.iter().any(|kind| kind == "response.completed"));
    }

    #[test]
    fn the_fallback_usage_is_responses_shaped() {
        // The whole-body fallback used to copy the Anthropic usage object through
        // verbatim: no `total_tokens`, and Anthropic-only fields leaked.
        let frames = message_to_responses_events(&json!({
            "id": "m", "model": "c", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 3, "output_tokens": 1, "cache_read_input_tokens": 2}
        }));
        let completed = data_of(&frames).pop().unwrap();
        let usage = &completed["response"]["usage"];
        assert_eq!(usage["total_tokens"], 4);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 2);
        assert!(usage.get("cache_read_input_tokens").is_none());
    }

    #[test]
    fn the_fallback_reports_an_error_body_instead_of_a_success() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = translator.stream_from_complete(
            json!({"type": "error", "error": {"message": "bad key"}})
                .to_string()
                .as_bytes(),
        );
        let types = event_types(&frames);
        assert_eq!(types.last().map(String::as_str), Some("response.failed"));
        assert!(!types.iter().any(|kind| kind == "response.completed"));
    }

    #[test]
    fn the_fallback_rejects_a_body_without_content() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = translator.stream_from_complete(b"{}");
        assert_eq!(
            event_types(&frames).last().map(String::as_str),
            Some("response.failed")
        );
    }

    #[test]
    fn start_block_input_is_used_when_no_delta_follows() {
        // Anthropic's spec puts the whole `input` in `content_block_start`; an
        // upstream that streams no deltas still has to produce arguments.
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m", "model": "c"}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {
                    "type": "tool_use", "id": "t1", "name": "Read",
                    "input": {"path": "/a"}}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_stop"}),
            ],
        );
        let payloads = data_of(&frames);
        let done = payloads
            .iter()
            .find(|payload| payload["type"] == "response.function_call_arguments.done")
            .expect("arguments done event");
        assert_eq!(done["arguments"], "{\"path\":\"/a\"}");
        let item = payloads
            .iter()
            .find(|payload| payload["type"] == "response.output_item.done")
            .unwrap();
        assert_eq!(item["item"]["arguments"], "{\"path\":\"/a\"}");
    }

    #[test]
    fn streamed_deltas_win_over_the_start_block_input() {
        let mut translator = ResponsesStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "message_start", "message": {"id": "m", "model": "c"}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {
                    "type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/a"}}}),
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "input_json_delta", "partial_json": "{\"b\":2}"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_stop"}),
            ],
        );
        let payloads = data_of(&frames);
        let done = payloads
            .iter()
            .find(|payload| payload["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(done["arguments"], "{\"b\":2}");
    }
}
