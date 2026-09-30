//! OpenAI Responses SSE → Anthropic Messages SSE.
//!
//! The client protocol dictates the shape precisely, so this is a state machine
//! rather than a per-event mapping:
//!
//! * exactly one `message_start` precedes every block;
//! * content-block indices are monotonic and blocks are opened/closed in order;
//! * `message_delta` carries the stop reason and final usage, and must be
//!   followed by `message_stop`.
//!
//! Nothing is emitted eagerly: a `message_start` is only sent once the upstream
//! has actually produced a first event, so an immediate upstream failure becomes
//! an Anthropic `error` event instead of a half-opened message.
//!
//! # Tool calls are buffered, not streamed through
//!
//! A Responses `function_call` may be announced long before its arguments
//! finish, and several calls can be in flight at once. Anthropic content blocks
//! are strictly sequential, and a `tool_use` block must be complete when it is
//! opened. Arguments are therefore accumulated per `item_id` and each call is
//! emitted as one atomic `content_block_start` + `input_json_delta` +
//! `content_block_stop` when `output_item.done` arrives (or when the stream
//! ends, for a call the upstream never closed). Interleaved calls then produce
//! one block each, with their own id and their own arguments — rather than two
//! argument streams concatenated into one block.

use std::collections::HashMap;

use proxy_common::protocol::ResponseTranslator;
use serde_json::{json, Value};

/// Anthropic content block currently open on the wire.
#[derive(Debug, Clone, PartialEq)]
enum OpenBlock {
    Text { index: i64 },
}

impl OpenBlock {
    fn index(&self) -> i64 {
        match self {
            OpenBlock::Text { index } => *index,
        }
    }
}

/// Accumulated state of one upstream `function_call`.
#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    call_id: String,
    name: String,
    /// Arguments reported in `output_item.added`, used only when no incremental
    /// delta ever arrives.
    start_arguments: String,
    /// Concatenated `function_call_arguments.delta` payloads.
    deltas: String,
}

impl ToolCall {
    fn arguments(&self) -> String {
        if self.deltas.is_empty() {
            self.start_arguments.clone()
        } else {
            self.deltas.clone()
        }
    }
}

/// Translates a Responses event stream into the Anthropic event stream.
#[derive(Debug)]
pub struct AnthropicStreamTranslator {
    message_id: String,
    model: String,
    started: bool,
    next_index: i64,
    open: Option<OpenBlock>,
    /// Announcement order of `function_call`s, so buffered calls are flushed in
    /// the order the upstream mentioned them.
    tool_order: Vec<String>,
    tools: HashMap<String, ToolCall>,
    /// Key of the most recently mentioned call, so an upstream that omits
    /// `item_id` on its deltas still has them delivered.
    current_tool: Option<String>,
    /// Counter for synthesised keys, used only when the upstream sends no ids.
    anonymous_tools: u64,
    /// A `function_call` was emitted, so the stop reason is `tool_use`.
    used_tools: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    finished: bool,
}

impl Default for AnthropicStreamTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicStreamTranslator {
    pub fn new() -> Self {
        Self {
            message_id: String::new(),
            model: String::new(),
            started: false,
            next_index: 0,
            open: None,
            tool_order: Vec::new(),
            tools: HashMap::new(),
            current_tool: None,
            anonymous_tools: 0,
            used_tools: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            finished: false,
        }
    }

    /// Serialise one Anthropic SSE frame.
    fn frame(event: &str, data: Value) -> Vec<u8> {
        format!("event: {event}\ndata: {data}\n\n").into_bytes()
    }

    fn ensure_started(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        self.started = true;
        let id = if self.message_id.is_empty() {
            format!("msg_{}", uuid::Uuid::now_v7().simple())
        } else {
            self.message_id.clone()
        };
        out.push(Self::frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    // Usually 0: the upstream reports usage only at the end of the
                    // stream. See `finish_message` for the authoritative totals.
                    "usage": {
                        "input_tokens": self.input_tokens,
                        "output_tokens": 0,
                    },
                },
            }),
        ));
    }

    fn close_open_block(&mut self, out: &mut Vec<Vec<u8>>) {
        if let Some(block) = self.open.take() {
            out.push(Self::frame(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": block.index()}),
            ));
        }
    }

    fn open_text_block(&mut self, out: &mut Vec<Vec<u8>>) {
        if matches!(self.open, Some(OpenBlock::Text { .. })) {
            return;
        }
        self.close_open_block(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock::Text { index });
        out.push(Self::frame(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
    }

    fn text_delta(&mut self, out: &mut Vec<Vec<u8>>, text: &str) {
        if text.is_empty() {
            return;
        }
        self.open_text_block(out);
        let index = self.open.as_ref().map(OpenBlock::index).unwrap_or(0);
        out.push(Self::frame(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        ));
    }

    /// Buffer key for an event that may omit `item_id`: fall back to the call
    /// mentioned most recently, which is what a single-call-at-a-time upstream
    /// without ids means.
    fn tool_key(&self, item_id: &str) -> Option<String> {
        if !item_id.is_empty() {
            return Some(item_id.to_string());
        }
        self.current_tool.clone()
    }

    /// Key for an event with no usable id at all, minting one if needed.
    fn anonymous_key(&mut self) -> String {
        self.anonymous_tools += 1;
        format!("anonymous#{}", self.anonymous_tools)
    }

    /// Reserve the buffer for `key`, keeping `tool_order` in announcement order.
    fn open_buffer(&mut self, key: &str) {
        if !self.tools.contains_key(key) {
            self.tool_order.push(key.to_string());
        }
    }

    /// Record an announced function call. Idempotent: a repeated announcement
    /// keeps the arguments already buffered.
    fn announce_tool(&mut self, item_id: &str, call_id: &str, name: &str, arguments: &str) {
        self.used_tools = true;
        let key = match self.tool_key(item_id) {
            Some(key) => key,
            None => self.anonymous_key(),
        };
        self.current_tool = Some(key.clone());
        self.open_buffer(&key);
        let entry = self.tools.entry(key).or_insert_with(|| ToolCall {
            call_id: String::new(),
            name: String::new(),
            start_arguments: String::new(),
            deltas: String::new(),
        });
        if entry.call_id.is_empty() {
            entry.call_id = call_id.to_string();
        }
        if entry.name.is_empty() {
            entry.name = name.to_string();
        }
        entry.start_arguments = arguments.to_string();
    }

    /// Accumulate one `function_call_arguments.delta`.
    fn buffer_arguments(&mut self, item_id: &str, partial: &str) {
        if partial.is_empty() {
            return;
        }
        self.used_tools = true;
        let key = match self.tool_key(item_id) {
            Some(key) => key,
            None => {
                let key = self.anonymous_key();
                self.current_tool = Some(key.clone());
                key
            }
        };
        self.open_buffer(&key);
        // An upstream that skipped `output_item.added` still deserves to have its
        // arguments delivered; the call id and name stay empty rather than the
        // delta being dropped.
        let entry = self.tools.entry(key).or_insert_with(|| ToolCall {
            call_id: String::new(),
            name: String::new(),
            start_arguments: String::new(),
            deltas: String::new(),
        });
        entry.deltas.push_str(partial);
    }

    /// Emit one buffered call as a complete Anthropic tool_use block.
    fn flush_tool(&mut self, out: &mut Vec<Vec<u8>>, item_id: &str) {
        let Some(key) = self.tool_key(item_id) else {
            return;
        };
        let Some(call) = self.tools.remove(&key) else {
            return;
        };
        self.tool_order.retain(|id| id != &key);
        if self.current_tool.as_deref() == Some(key.as_str()) {
            self.current_tool = None;
        }
        self.emit_tool_block(out, &key, call);
    }

    /// Write one buffered call out as `content_block_start` + arguments + stop.
    fn emit_tool_block(&mut self, out: &mut Vec<Vec<u8>>, key: &str, call: ToolCall) {
        let _ = key;
        self.close_open_block(out);
        let index = self.next_index;
        self.next_index += 1;
        let id = if call.call_id.is_empty() {
            format!("toolu_{index}")
        } else {
            call.call_id.clone()
        };
        out.push(Self::frame(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": call.name, "input": {}},
            }),
        ));
        let arguments = call.arguments();
        if !arguments.is_empty() {
            out.push(Self::frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": arguments},
                }),
            ));
        }
        out.push(Self::frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }

    /// Emit every call the upstream never closed, in announcement order.
    fn flush_pending_tools(&mut self, out: &mut Vec<Vec<u8>>) {
        let pending = std::mem::take(&mut self.tool_order);
        for key in pending {
            if let Some(call) = self.tools.remove(&key) {
                self.emit_tool_block(out, &key, call);
            }
        }
        self.current_tool = None;
    }

    /// Emit the terminal `message_delta` + `message_stop`.
    fn finish_message(&mut self, out: &mut Vec<Vec<u8>>, stop_reason: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_started(out);
        self.flush_pending_tools(out);
        self.close_open_block(out);
        // Responses only reports usage in its terminal event, which arrives after
        // the first text delta — too late for `message_start`. The authoritative
        // input/cache counts are therefore repeated here so a client that sums
        // usage still ends up with the right totals.
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
        });
        if self.cache_read_tokens > 0 {
            usage["cache_read_input_tokens"] = json!(self.cache_read_tokens);
        }
        out.push(Self::frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage,
            }),
        ));
        out.push(Self::frame("message_stop", json!({"type": "message_stop"})));
    }

    fn absorb_usage(&mut self, response: &Value) {
        let Some(usage) = response.get("usage") else {
            return;
        };
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = value;
        }
        if let Some(value) = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
        {
            self.cache_read_tokens = value;
        }
    }

    /// Failure message from a Responses error body, if the body is one.
    fn error_message(&self, body: &[u8]) -> Option<String> {
        let payload: Value = serde_json::from_slice(body).ok()?;
        if payload.get("type").and_then(Value::as_str) == Some("error") {
            return Some(
                payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_string(),
            );
        }
        payload
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn absorb_response_meta(&mut self, response: &Value) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.message_id = id.to_string();
        }
        if let Some(model) = response.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        self.absorb_usage(response);
    }

    /// Map a Responses terminal status onto an Anthropic stop reason.
    ///
    /// Only `max_output_tokens` means "the answer was cut short". Other
    /// incomplete reasons (`content_filter`, `server_error`) are not truncation,
    /// and reporting them as `max_tokens` made clients retry a request that had
    /// actually been filtered.
    fn stop_reason(&self, status: Option<&str>, incomplete_reason: Option<&str>) -> &'static str {
        let truncated = match incomplete_reason {
            Some("max_output_tokens") => true,
            Some(_) => false,
            // Incomplete with no reason given: truncation is the safer guess.
            None => status == Some("incomplete"),
        };
        if truncated {
            return "max_tokens";
        }
        if self.used_tools {
            return "tool_use";
        }
        "end_turn"
    }

    fn handle_event(&mut self, payload: &Value, out: &mut Vec<Vec<u8>>) {
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "response.created" | "response.in_progress" => {
                if let Some(response) = payload.get("response") {
                    self.absorb_response_meta(response);
                }
                self.ensure_started(out);
            }
            "response.output_item.added" => {
                let Some(item) = payload.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.ensure_started(out);
                    self.announce_tool(
                        item.get("id").and_then(Value::as_str).unwrap_or_default(),
                        item.get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        item.get("arguments").and_then(Value::as_str).unwrap_or(""),
                    );
                }
            }
            "response.output_text.delta" => {
                self.ensure_started(out);
                let text = payload.get("delta").and_then(Value::as_str).unwrap_or("");
                self.text_delta(out, text);
            }
            "response.output_text.done" => self.close_open_block(out),
            "response.function_call_arguments.delta" => {
                self.ensure_started(out);
                let item_id = payload.get("item_id").and_then(Value::as_str).unwrap_or("");
                let partial = payload.get("delta").and_then(Value::as_str).unwrap_or("");
                self.buffer_arguments(item_id, partial);
            }
            "response.output_item.done" => match payload.get("item") {
                // A finished function call is emitted whole, so its block is
                // complete the moment the client sees it.
                Some(item) if item.get("type").and_then(Value::as_str) == Some("function_call") => {
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.flush_tool(out, item_id);
                }
                _ => self.close_open_block(out),
            },
            "response.completed" => {
                if let Some(response) = payload.get("response") {
                    self.absorb_response_meta(response);
                }
                let response = payload.get("response");
                let status = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str);
                let incomplete = response
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str);
                let reason = self.stop_reason(status, incomplete);
                self.finish_message(out, reason);
            }
            "response.incomplete" => {
                if let Some(response) = payload.get("response") {
                    self.absorb_response_meta(response);
                }
                let incomplete = payload
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str);
                let reason = self.stop_reason(Some("incomplete"), incomplete);
                self.finish_message(out, reason);
            }
            "response.failed" => {
                let message = payload
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream reported failure");
                self.emit_error(out, "api_error", message);
            }
            "error" => {
                let message = payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error");
                self.emit_error(out, "api_error", message);
            }
            _ => {}
        }
    }

    /// Anthropic reports mid-stream failures as an `error` event.
    fn emit_error(&mut self, out: &mut Vec<Vec<u8>>, kind: &str, message: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        // Buffered calls are dropped: their arguments are incomplete JSON, and a
        // truncated `tool_use` would make the client fail parsing instead of
        // reporting the upstream error.
        self.tools.clear();
        self.tool_order.clear();
        self.current_tool = None;
        self.ensure_started(out);
        self.close_open_block(out);
        out.push(Self::frame(
            "error",
            json!({
                "type": "error",
                "error": {"type": kind, "message": message},
            }),
        ));
    }
}

impl ResponseTranslator for AnthropicStreamTranslator {
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        // The upstream may send `[DONE]`, comments, or pings; anything that is not
        // a JSON object is not a Responses event.
        let Ok(payload) = serde_json::from_slice::<Value>(trim_payload(event)) else {
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
        if !self.finished && self.started {
            // Upstream closed without a terminal event; close the message so the
            // client is not left waiting.
            self.finish_message(
                &mut out,
                if self.used_tools {
                    "tool_use"
                } else {
                    "end_turn"
                },
            );
        }
        out
    }

    fn abort(&mut self, reason: &str) -> Vec<Vec<u8>> {
        // A truncated or untranslatable stream must not look complete.
        let mut out = Vec::new();
        self.emit_error(&mut out, "api_error", reason);
        out
    }

    fn stream_from_complete(&mut self, body: &[u8]) -> Vec<Vec<u8>> {
        if let Some(message) = self.error_message(body) {
            return self.abort(&message);
        }
        self.translate_complete(body)
            .and_then(|message| serde_json::from_slice::<Value>(&message).ok())
            .map(|message| message_to_sse(&message))
            // A client that asked to stream must always get a terminal event; an
            // empty body used to produce a silent, empty stream.
            .unwrap_or_else(|| self.abort("upstream returned no translatable body"))
    }

    fn translate_complete(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        // Non-streaming: a single Responses object becomes a single Anthropic
        // message. Building it through the same rules keeps the two paths
        // consistent.
        let response: Value = serde_json::from_slice(body).ok()?;
        self.absorb_response_meta(&response);

        let mut blocks: Vec<Value> = Vec::new();
        // A Responses object always carries `output`; its absence means this is
        // not one, and the relay should report the failure instead of inventing
        // an empty answer. Individual odd *items* are skipped below, not fatal.
        let output = response.get("output").and_then(Value::as_array)?;
        for item in output {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "message" => {
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                    {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    blocks.push(json!({"type": "text", "text": text}));
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    self.used_tools = true;
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "input": serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({})),
                    }));
                }
                _ => {}
            }
        }

        let status = response.get("status").and_then(Value::as_str);
        let reason = self.stop_reason(status, None);
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
        });
        if self.cache_read_tokens > 0 {
            usage["cache_read_input_tokens"] = json!(self.cache_read_tokens);
        }

        let message = json!({
            "id": if self.message_id.is_empty() {
                format!("msg_{}", uuid::Uuid::now_v7().simple())
            } else {
                self.message_id.clone()
            },
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": blocks,
            "stop_reason": reason,
            "stop_sequence": null,
            "usage": usage,
        });
        self.finished = true;
        serde_json::to_vec(&message).ok()
    }
}

/// Render a complete Anthropic message as its SSE event sequence.
///
/// Used when the client asked to stream but the upstream replied with a single
/// JSON body. The ordering rules are the same as for a live stream: one
/// `message_start`, one block open/close per content block, then
/// `message_delta` + `message_stop`.
pub fn message_to_sse(message: &Value) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start_message = message.clone();
    start_message["content"] = json!([]);
    start_message["stop_reason"] = Value::Null;
    out.push(AnthropicStreamTranslator::frame(
        "message_start",
        json!({"type": "message_start", "message": start_message}),
    ));

    let mut output_tokens = 0u64;
    for (index, block) in message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let index = index as i64;
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                out.push(AnthropicStreamTranslator::frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index,
                           "content_block": {"type": "text", "text": ""}}),
                ));
                out.push(AnthropicStreamTranslator::frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index,
                           "delta": {"type": "text_delta",
                                     "text": block.get("text").and_then(Value::as_str).unwrap_or("")}}),
                ));
            }
            "tool_use" => {
                out.push(AnthropicStreamTranslator::frame(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index,
                           "content_block": {"type": "tool_use",
                                             "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                             "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                             "input": {}}}),
                ));
                out.push(AnthropicStreamTranslator::frame(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index,
                           "delta": {"type": "input_json_delta",
                                     "partial_json": block.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into())}}),
                ));
            }
            _ => continue,
        }
        out.push(AnthropicStreamTranslator::frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }

    if let Some(usage) = message.get("usage") {
        output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    out.push(AnthropicStreamTranslator::frame(
        "message_delta",
        json!({"type": "message_delta",
               "delta": {"stop_reason": message.get("stop_reason").cloned().unwrap_or(json!("end_turn")),
                         "stop_sequence": Value::Null},
               "usage": message.get("usage").cloned().unwrap_or(json!({"output_tokens": output_tokens}))}),
    ));
    out.push(AnthropicStreamTranslator::frame(
        "message_stop",
        json!({"type": "message_stop"}),
    ));
    out
}

/// Strip SSE framing and whitespace around a payload.
fn trim_payload(event: &[u8]) -> &[u8] {
    let text = std::str::from_utf8(event).unwrap_or("");
    let mut payload = text.trim();
    if let Some(rest) = payload.strip_prefix("data:") {
        payload = rest.trim();
    }
    payload.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(frames: &[Vec<u8>]) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                let data = text
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))?
                    .to_string();
                serde_json::from_str(&data).ok()
            })
            .collect()
    }

    fn event_types(frames: &[Vec<u8>]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|frame| {
                let text = std::str::from_utf8(frame).unwrap();
                text.lines()
                    .find_map(|line| line.strip_prefix("event: "))
                    .map(str::to_string)
            })
            .collect()
    }

    fn feed(translator: &mut AnthropicStreamTranslator, events: &[Value]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for event in events {
            out.extend(translator.push(event.to_string().as_bytes()));
        }
        out
    }

    #[test]
    fn a_text_stream_becomes_the_anthropic_sequence() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5"}}),
                json!({"type": "response.output_item.added", "item": {"type": "message"}}),
                json!({"type": "response.output_text.delta", "delta": "Hel"}),
                json!({"type": "response.output_text.delta", "delta": "lo"}),
                json!({"type": "response.output_text.done"}),
                json!({"type": "response.completed", "response": {
                    "id": "resp_1", "model": "gpt-5", "status": "completed",
                    "usage": {"input_tokens": 11, "output_tokens": 3,
                              "input_tokens_details": {"cached_tokens": 5}}
                }}),
            ],
        );

        assert_eq!(
            event_types(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let payloads = text_of(&frames);
        assert_eq!(payloads[0]["message"]["id"], "resp_1");
        assert_eq!(payloads[0]["message"]["model"], "gpt-5");
        assert_eq!(
            payloads[0]["message"]["usage"]["input_tokens"], 0,
            "usage is unknown when the first delta arrives"
        );
        assert_eq!(payloads[1]["content_block"]["type"], "text");
        assert_eq!(payloads[1]["index"], 0);
        assert_eq!(payloads[2]["delta"]["text"], "Hel");
        assert_eq!(payloads[3]["delta"]["text"], "lo");
        assert_eq!(payloads[5]["delta"]["stop_reason"], "end_turn");
        // The terminal event carries the authoritative totals.
        assert_eq!(payloads[5]["usage"]["input_tokens"], 11);
        assert_eq!(payloads[5]["usage"]["output_tokens"], 3);
        assert_eq!(payloads[5]["usage"]["cache_read_input_tokens"], 5);
    }

    #[test]
    fn exactly_one_message_start_and_one_message_stop() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.in_progress", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "a"}),
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
            ],
        );
        let types = event_types(&frames);
        assert_eq!(types.iter().filter(|t| *t == "message_start").count(), 1);
        assert_eq!(types.iter().filter(|t| *t == "message_stop").count(), 1);
    }

    #[test]
    fn a_function_call_becomes_a_tool_use_block() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "resp_2", "model": "gpt-5"}}),
                json!({"type": "response.output_item.added", "item": {
                    "type": "function_call", "call_id": "call_7", "name": "Read"
                }}),
                json!({"type": "response.function_call_arguments.delta", "delta": "{\"pa"}),
                json!({"type": "response.function_call_arguments.delta", "delta": "th\":\"/a\"}"}),
                json!({"type": "response.output_item.done"}),
                json!({"type": "response.completed", "response": {
                    "status": "completed", "usage": {"input_tokens": 4, "output_tokens": 9}
                }}),
            ],
        );

        let payloads = text_of(&frames);
        let start = payloads
            .iter()
            .find(|p| p["type"] == "content_block_start")
            .unwrap();
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["id"], "call_7");
        assert_eq!(start["content_block"]["name"], "Read");

        // Arguments are buffered and emitted as one delta carrying the complete
        // JSON, so the client never sees a half-written tool call.
        let deltas: Vec<&Value> = payloads
            .iter()
            .filter(|p| p["type"] == "content_block_delta")
            .collect();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0]["delta"]["partial_json"], "{\"path\":\"/a\"}");

        let message_delta = payloads
            .iter()
            .find(|p| p["type"] == "message_delta")
            .unwrap();
        assert_eq!(
            message_delta["delta"]["stop_reason"], "tool_use",
            "a tool call must report tool_use"
        );
    }

    #[test]
    fn text_then_tool_call_uses_distinct_indices() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "thinking out loud"}),
                json!({"type": "response.output_item.added", "item": {
                    "type": "function_call", "call_id": "c1", "name": "Bash"
                }}),
                json!({"type": "response.function_call_arguments.delta", "delta": "{}"}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
            ],
        );
        let payloads = text_of(&frames);
        let starts: Vec<&Value> = payloads
            .iter()
            .filter(|p| p["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["index"], 0);
        assert_eq!(starts[1]["index"], 1);
        assert_eq!(starts[0]["content_block"]["type"], "text");
        assert_eq!(starts[1]["content_block"]["type"], "tool_use");
        // The text block must be closed before the tool block opens.
        let types = event_types(&frames);
        let first_stop = types
            .iter()
            .position(|t| t == "content_block_stop")
            .unwrap();
        let last_start = types
            .iter()
            .rposition(|t| t == "content_block_start")
            .unwrap();
        assert!(first_stop > 0 && first_stop < last_start, "{types:?}");
    }

    #[test]
    fn incomplete_stream_reports_max_tokens() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "cut off"}),
                json!({"type": "response.incomplete", "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {"input_tokens": 2, "output_tokens": 99}
                }}),
            ],
        );
        let payloads = text_of(&frames);
        let message_delta = payloads
            .iter()
            .find(|p| p["type"] == "message_delta")
            .unwrap();
        assert_eq!(message_delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn a_failed_response_becomes_an_error_event() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.failed", "response": {
                    "error": {"message": "rate limited"}
                }}),
            ],
        );
        let types = event_types(&frames);
        assert!(types.contains(&"error".to_string()), "{types:?}");
        assert!(!types.contains(&"message_stop".to_string()));
        let payloads = text_of(&frames);
        let error = payloads.iter().find(|p| p["type"] == "error").unwrap();
        assert_eq!(error["error"]["message"], "rate limited");
    }

    #[test]
    fn a_top_level_error_event_is_translated() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[json!({"type": "error", "message": "boom"})],
        );
        let payloads = text_of(&frames);
        assert_eq!(payloads.last().unwrap()["type"], "error");
    }

    #[test]
    fn non_json_frames_are_ignored() {
        let mut translator = AnthropicStreamTranslator::new();
        assert!(translator.push(b"[DONE]").is_empty());
        assert!(translator.push(b": keep-alive").is_empty());
        assert!(translator.push(b"").is_empty());
        assert!(translator.push(b"\"just a string\"").is_empty());
    }

    #[test]
    fn data_prefixed_frames_are_accepted() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = translator.push(br#"data: {"type":"response.output_text.delta","delta":"x"}"#);
        assert_eq!(
            event_types(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
    }

    #[test]
    fn finish_closes_a_stream_that_never_completed() {
        let mut translator = AnthropicStreamTranslator::new();
        feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "partial"}),
            ],
        );
        let types = event_types(&translator.finish());
        assert_eq!(
            types,
            vec!["content_block_stop", "message_delta", "message_stop"]
        );
        // Finishing twice must not duplicate the terminator.
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn finish_on_an_untouched_stream_emits_nothing() {
        let mut translator = AnthropicStreamTranslator::new();
        assert!(translator.finish().is_empty());
    }

    #[test]
    fn non_streaming_body_becomes_a_complete_message() {
        let mut translator = AnthropicStreamTranslator::new();
        let body = json!({
            "id": "resp_9", "model": "gpt-5", "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hi there"}]},
                {"type": "function_call", "call_id": "c9", "name": "Grep",
                 "arguments": "{\"pattern\":\"x\"}"}
            ],
            "usage": {"input_tokens": 7, "output_tokens": 5,
                      "input_tokens_details": {"cached_tokens": 2}}
        });
        let out = translator
            .translate_complete(body.to_string().as_bytes())
            .unwrap();
        let message: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(message["id"], "resp_9");
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "hi there");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "c9");
        assert_eq!(message["content"][1]["input"]["pattern"], "x");
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["usage"]["input_tokens"], 7);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 2);
    }

    #[test]
    fn non_streaming_malformed_body_returns_none() {
        // `None` means "not translatable"; the relay then reports the failure
        // rather than forwarding a Responses body to an Anthropic client.
        let mut translator = AnthropicStreamTranslator::new();
        assert!(translator.translate_complete(b"nope").is_none());
        assert!(translator
            .translate_complete(json!({"status": "completed"}).to_string().as_bytes())
            .is_none());
    }

    #[test]
    fn non_streaming_skips_an_unrepresentable_item_instead_of_failing() {
        // One broken output item must not discard the whole answer.
        let mut translator = AnthropicStreamTranslator::new();
        let body = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "kept"}]},
                {"content": null},
            ],
        })
        .to_string();
        let message: Value =
            serde_json::from_slice(&translator.translate_complete(body.as_bytes()).unwrap())
                .unwrap();
        assert_eq!(message["content"][0]["text"], "kept");
    }

    #[test]
    fn a_non_sse_body_is_rendered_as_an_event_sequence() {
        // The upstream ignored `stream: true`; the client must still get a full
        // event stream rather than nothing.
        let mut translator = AnthropicStreamTranslator::new();
        let body = json!({
            "id": "resp_off", "model": "gpt-5", "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "offline answer"}]},
                {"type": "function_call", "call_id": "c1", "name": "Read",
                 "arguments": "{\"path\":\"/a\"}"}
            ],
            "usage": {"input_tokens": 8, "output_tokens": 6}
        });
        let frames = translator.stream_from_complete(body.to_string().as_bytes());
        let types = event_types(&frames);

        assert_eq!(
            types,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let payloads = text_of(&frames);
        assert_eq!(payloads[0]["message"]["id"], "resp_off");
        assert!(payloads[0]["message"]["content"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(payloads[2]["delta"]["text"], "offline answer");
        assert_eq!(payloads[4]["content_block"]["type"], "tool_use");
        assert_eq!(payloads[5]["delta"]["partial_json"], "{\"path\":\"/a\"}");
        assert_eq!(payloads[7]["delta"]["stop_reason"], "tool_use");
        assert_eq!(payloads[7]["usage"]["input_tokens"], 8);
    }

    #[test]
    fn stream_from_complete_reports_garbage_instead_of_going_silent() {
        // A client that asked to stream must always get a terminal event: an
        // empty result used to leave it waiting forever.
        let mut translator = AnthropicStreamTranslator::new();
        let frames = translator.stream_from_complete(b"not json");
        assert_eq!(
            event_types(&frames).last().map(String::as_str),
            Some("error")
        );
        assert_eq!(text_of(&frames).last().unwrap()["type"], "error");

        let mut translator = AnthropicStreamTranslator::new();
        let frames =
            translator.stream_from_complete(json!({"status": "completed"}).to_string().as_bytes());
        assert_eq!(
            event_types(&frames).last().map(String::as_str),
            Some("error")
        );
    }

    #[test]
    fn stream_from_complete_surfaces_an_upstream_error_body() {
        // The upstream ignored `stream: true` and answered with an error object;
        // the message must reach the client instead of being dropped.
        let mut translator = AnthropicStreamTranslator::new();
        let frames = translator.stream_from_complete(
            json!({"error": {"message": "invalid api key", "type": "invalid_request_error"}})
                .to_string()
                .as_bytes(),
        );
        assert_eq!(
            event_types(&frames).last().map(String::as_str),
            Some("error")
        );
        assert_eq!(
            text_of(&frames).last().unwrap()["error"]["message"],
            "invalid api key"
        );
    }

    #[test]
    fn message_to_sse_handles_an_empty_content_list() {
        let frames = message_to_sse(&json!({
            "id": "m1", "type": "message", "role": "assistant",
            "model": "gpt-5", "content": [], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }));
        assert_eq!(
            event_types(&frames),
            vec!["message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn a_content_filter_incomplete_is_not_reported_as_truncation() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r", "model": "gpt-5"}}),
                json!({"type": "response.output_text.delta", "delta": "partial"}),
                json!({"type": "response.incomplete", "response": {
                    "id": "r", "status": "incomplete",
                    "incomplete_details": {"reason": "content_filter"},
                }}),
            ],
        );
        let payloads = text_of(&frames);
        let delta = payloads
            .iter()
            .find(|payload| payload["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn max_output_tokens_is_still_truncation() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "cut"}),
                json!({"type": "response.incomplete", "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"},
                }}),
            ],
        );
        let payloads = text_of(&frames);
        let delta = payloads
            .iter()
            .find(|payload| payload["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn interleaved_function_calls_get_their_own_blocks() {
        // Two calls announced before either finishes: each needs its own block
        // with its own id, instead of both arguments concatenated into one.
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_item.added", "item": {
                    "id": "item_1", "type": "function_call", "call_id": "c1", "name": "Read"}}),
                json!({"type": "response.output_item.added", "item": {
                    "id": "item_2", "type": "function_call", "call_id": "c2", "name": "Write"}}),
                json!({"type": "response.function_call_arguments.delta",
                       "item_id": "item_1", "delta": "{\"p\":1}"}),
                json!({"type": "response.function_call_arguments.delta",
                       "item_id": "item_2", "delta": "{\"q\":2}"}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
            ],
        );

        let payloads = text_of(&frames);
        let starts: Vec<&Value> = payloads
            .iter()
            .filter(|payload| payload["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 2, "one block per call");
        assert_eq!(starts[0]["content_block"]["id"], "c1");
        assert_eq!(starts[1]["content_block"]["id"], "c2");

        let deltas: Vec<&Value> = payloads
            .iter()
            .filter(|payload| payload["type"] == "content_block_delta")
            .collect();
        assert_eq!(deltas.len(), 2);
        assert_ne!(deltas[0]["index"], deltas[1]["index"], "separate blocks");
        assert_eq!(deltas[0]["delta"]["partial_json"], "{\"p\":1}");
        assert_eq!(deltas[1]["delta"]["partial_json"], "{\"q\":2}");
    }

    #[test]
    fn arguments_for_an_unannounced_call_reach_a_valid_block() {
        // A delta with no `output_item.added` used to be sent with index 0, which
        // the client had never seen opened. It now gets its own block.
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.function_call_arguments.delta",
                       "item_id": "ghost", "delta": "{}"}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
            ],
        );
        let payloads = text_of(&frames);
        let start = payloads
            .iter()
            .find(|payload| payload["type"] == "content_block_start")
            .expect("the block must be opened before its delta");
        let delta = payloads
            .iter()
            .find(|payload| payload["type"] == "content_block_delta")
            .expect("the arguments must still be delivered");
        assert_eq!(delta["index"], start["index"]);
        assert_eq!(delta["delta"]["partial_json"], "{}");
    }

    #[test]
    fn arguments_carried_by_the_start_event_are_flushed_once() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_item.added", "item": {
                    "id": "item_1", "type": "function_call", "call_id": "c1", "name": "Read",
                    "arguments": "{\"path\":\"/a\"}"}}),
                json!({"type": "response.output_item.done", "item": {"id": "item_1"}}),
                json!({"type": "response.completed", "response": {"status": "completed"}}),
            ],
        );
        let payloads = text_of(&frames);
        let deltas: Vec<&Value> = payloads
            .iter()
            .filter(|payload| payload["type"] == "content_block_delta")
            .collect();
        assert_eq!(deltas.len(), 1, "exactly one flush, no duplication");
        assert_eq!(deltas[0]["delta"]["partial_json"], "{\"path\":\"/a\"}");
    }

    #[test]
    fn abort_reports_a_failure_rather_than_a_completion() {
        let mut translator = AnthropicStreamTranslator::new();
        let frames = feed(
            &mut translator,
            &[
                json!({"type": "response.created", "response": {"id": "r"}}),
                json!({"type": "response.output_text.delta", "delta": "half"}),
            ],
        );
        assert!(!frames.is_empty());

        let frames = translator.abort("connection reset");
        let payloads = text_of(&frames);
        assert_eq!(payloads.last().unwrap()["type"], "error");
        assert_eq!(
            payloads.last().unwrap()["error"]["message"],
            "connection reset"
        );
        assert!(
            !text_of(&frames)
                .iter()
                .any(|payload| payload["type"] == "message_delta"),
            "an aborted stream must not claim a normal stop reason"
        );
    }
}
