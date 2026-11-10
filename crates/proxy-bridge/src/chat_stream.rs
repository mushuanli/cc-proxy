//! Chat Completions response translation through Anthropic's sequential blocks.
use proxy_common::protocol::ResponseTranslator;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const REASONING_SIGNATURE: &str = "cc-proxy-chat-reasoning-v1";

fn frame(value: Value) -> Vec<u8> {
    let event = value["type"].as_str().unwrap_or("error");
    format!("event: {event}\ndata: {value}\n\n").into_bytes()
}
fn data(frame: &[u8]) -> Option<&[u8]> {
    frame
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"data: "))
}
fn error(reason: &str) -> Value {
    json!({"type":"error","error":{"type":"api_error","message":reason}})
}
fn stop(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}
fn chat_stop(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}
fn usage(value: &Value) -> Value {
    let prompt = value["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    json!({"input_tokens":prompt.saturating_sub(cached),"output_tokens":value["completion_tokens"].as_u64().unwrap_or(0),"cache_read_input_tokens":cached})
}
fn chat_usage(value: &Value) -> Value {
    let cached = value["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let prompt = value["input_tokens"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(cached);
    let completion = value["output_tokens"].as_u64().unwrap_or(0);
    json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt.saturating_add(completion),"prompt_tokens_details":{"cached_tokens":cached}})
}

/// Chat chunks can interleave tool calls. Buffer each call until the final chunk.
#[derive(Default)]
pub struct ChatToAnthropic {
    started: bool,
    finished: bool,
    text_open: bool,
    thinking_open: bool,
    next_index: usize,
    tools: BTreeMap<u64, Value>,
    usage: Value,
    finish_reason: Option<String>,
}
impl ChatToAnthropic {
    fn start(&mut self, v: &Value, out: &mut Vec<Vec<u8>>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(frame(json!({"type":"message_start","message":{"id":v["id"],"type":"message","role":"assistant","model":v["model"],"content":[],"stop_reason":null,"usage":usage(&v["usage"])}})));
    }
    fn close_thinking(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.thinking_open {
            out.push(frame(json!({"type":"content_block_delta","index":self.next_index,"delta":{"type":"signature_delta","signature":REASONING_SIGNATURE}})));
            out.push(frame(
                json!({"type":"content_block_stop","index":self.next_index}),
            ));
            self.next_index += 1;
            self.thinking_open = false;
        }
    }
    fn thinking(&mut self, text: &str, out: &mut Vec<Vec<u8>>) {
        self.close_text(out);
        if !self.thinking_open {
            out.push(frame(json!({"type":"content_block_start","index":self.next_index,"content_block":{"type":"thinking","thinking":""}})));
            self.thinking_open = true;
        }
        if !text.is_empty() {
            out.push(frame(json!({"type":"content_block_delta","index":self.next_index,"delta":{"type":"thinking_delta","thinking":text}})));
        }
    }
    fn close_text(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.text_open {
            out.push(frame(
                json!({"type":"content_block_stop","index":self.next_index}),
            ));
            self.next_index += 1;
            self.text_open = false;
        }
    }
    fn text(&mut self, text: &str, out: &mut Vec<Vec<u8>>) {
        self.close_thinking(out);
        if text.is_empty() {
            return;
        }
        if !self.text_open {
            out.push(frame(json!({"type":"content_block_start","index":self.next_index,"content_block":{"type":"text","text":""}})));
            self.text_open = true;
        }
        out.push(frame(json!({"type":"content_block_delta","index":self.next_index,"delta":{"type":"text_delta","text":text}})));
    }
    fn accumulate_tools(&mut self, delta: &Value) {
        for t in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = t["index"].as_u64().unwrap_or(0);
            let entry = self
                .tools
                .entry(index)
                .or_insert_with(|| json!({"id":"","name":"","arguments":""}));
            for (target, value) in [
                ("id", &t["id"]),
                ("name", &t["function"]["name"]),
                ("arguments", &t["function"]["arguments"]),
            ] {
                if let Some(s) = value.as_str() {
                    let mut acc = entry[target].as_str().unwrap_or("").to_owned();
                    acc.push_str(s);
                    entry[target] = json!(acc);
                }
            }
        }
    }
    fn complete_message(body: &[u8]) -> Option<Vec<u8>> {
        let v: Value = serde_json::from_slice(body).ok()?;
        let m = v.pointer("/choices/0/message")?;
        let mut content = Vec::new();
        if let Some(reasoning) = m["reasoning_content"].as_str() {
            content.push(
                json!({"type":"thinking","thinking":reasoning,"signature":REASONING_SIGNATURE}),
            );
        }
        if let Some(text) = m["content"].as_str().filter(|s| !s.is_empty()) {
            content.push(json!({"type":"text","text":text}));
        }
        for t in m["tool_calls"].as_array().into_iter().flatten() {
            let input: Value =
                serde_json::from_str(t["function"]["arguments"].as_str().unwrap_or("{}")).ok()?;
            content.push(
                json!({"type":"tool_use","id":t["id"],"name":t["function"]["name"],"input":input}),
            );
        }
        serde_json::to_vec(&json!({"id":v["id"],"type":"message","role":"assistant","model":v["model"],"content":content,"stop_reason":stop(v["choices"][0]["finish_reason"].as_str().unwrap_or("stop")),"stop_sequence":null,"usage":usage(&v["usage"])})).ok()
    }
}
impl ResponseTranslator for ChatToAnthropic {
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        if event == b"[DONE]" {
            return self.finish();
        }
        let Ok(v) = serde_json::from_slice::<Value>(event) else {
            return vec![];
        };
        if let Some(e) = v.get("error") {
            return self.abort(e["message"].as_str().unwrap_or("upstream error"));
        }
        if v.get("choices").is_none() {
            return vec![];
        }
        let mut out = Vec::new();
        self.start(&v, &mut out);
        if v["usage"].is_object() {
            self.usage = v["usage"].clone();
        }
        let c = &v["choices"][0];
        if let Some(reasoning) = c["delta"]["reasoning_content"].as_str() {
            self.thinking(reasoning, &mut out);
        }
        if let Some(text) = c["delta"]["content"].as_str() {
            self.text(text, &mut out);
        }
        self.accumulate_tools(&c["delta"]);
        if let Some(reason) = c["finish_reason"].as_str() {
            self.finish_reason = Some(reason.into());
        }
        out
    }
    fn finish(&mut self) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        if !self.started || self.finish_reason.is_none() {
            return self.abort("Chat Completions stream ended before finish_reason");
        }
        self.finished = true;
        let mut out = Vec::new();
        self.close_thinking(&mut out);
        self.close_text(&mut out);
        for tool in self.tools.values() {
            if serde_json::from_str::<Value>(tool["arguments"].as_str().unwrap_or("")).is_err() {
                out.push(frame(error("invalid tool call JSON from Chat Completions")));
                return out;
            }
            out.push(frame(json!({"type":"content_block_start","index":self.next_index,"content_block":{"type":"tool_use","id":tool["id"],"name":tool["name"],"input":{}}})));
            out.push(frame(json!({"type":"content_block_delta","index":self.next_index,"delta":{"type":"input_json_delta","partial_json":tool["arguments"]}})));
            out.push(frame(
                json!({"type":"content_block_stop","index":self.next_index}),
            ));
            self.next_index += 1;
        }
        out.push(frame(json!({"type":"message_delta","delta":{"stop_reason":stop(self.finish_reason.as_deref().unwrap()),"stop_sequence":null},"usage":usage(&self.usage)})));
        out.push(frame(json!({"type":"message_stop"})));
        out
    }
    fn abort(&mut self, reason: &str) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        self.finished = true;
        vec![frame(error(reason))]
    }
    fn translate_complete(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        Self::complete_message(body)
    }
    fn stream_from_complete(&mut self, body: &[u8]) -> Vec<Vec<u8>> {
        if let Ok(value) = serde_json::from_slice::<Value>(body) {
            if let Some(reason) = value.pointer("/error/message").and_then(Value::as_str) {
                return self.abort(reason);
            }
        }
        match Self::complete_message(body).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(m) => {
                self.finished = true;
                crate::stream::message_to_sse(&m)
            }
            None => self.abort("upstream returned no Chat Completions message"),
        }
    }
}

/// Convert sequential Anthropic events to Chat Completions chunks.
pub struct AnthropicToChat {
    id: Value,
    model: Value,
    usage: Value,
    tools: BTreeMap<u64, usize>,
    finished: bool,
    terminal: bool,
    include_usage: bool,
}
impl AnthropicToChat {
    pub fn new(include_usage: bool) -> Self {
        Self {
            id: json!(format!("chatcmpl-{}", uuid::Uuid::now_v7().simple())),
            model: json!(""),
            usage: json!({}),
            tools: BTreeMap::new(),
            finished: false,
            terminal: false,
            include_usage,
        }
    }
    fn chunk(&self, delta: Value, reason: Value) -> Vec<u8> {
        format!("data: {}\n\n", json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})).into_bytes()
    }
    fn complete(body: &[u8]) -> Option<Vec<u8>> {
        let m: Value = serde_json::from_slice(body).ok()?;
        let mut text = String::new();
        let mut tools = Vec::new();
        let mut reasoning = String::new();
        let mut saw_thinking = false;
        for b in m["content"].as_array()? {
            match b["type"].as_str().unwrap_or("") {
                "thinking" => { saw_thinking=true; reasoning.push_str(b["thinking"].as_str().unwrap_or("")); },
                "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                "tool_use" => tools.push(json!({"id":b["id"],"type":"function","function":{"name":b["name"],"arguments":b["input"].to_string()}})),
                _ => {},
            }
        }
        let mut message = json!({"role":"assistant","content":if text.is_empty() {Value::Null} else {json!(text)}});
        if saw_thinking {
            message["reasoning_content"] = json!(reasoning);
        }
        if !tools.is_empty() {
            message["tool_calls"] = json!(tools);
        }
        serde_json::to_vec(&json!({"id":m["id"],"object":"chat.completion","created":0,"model":m["model"],"choices":[{"index":0,"message":message,"finish_reason":chat_stop(m["stop_reason"].as_str().unwrap_or("end_turn"))}],"usage":chat_usage(&m["usage"])})).ok()
    }
}
impl ResponseTranslator for AnthropicToChat {
    fn configure_client(&mut self, body: &[u8]) {
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            self.include_usage = v
                .pointer("/stream_options/include_usage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
    }
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        let Ok(v) = serde_json::from_slice::<Value>(event) else {
            return vec![];
        };
        let mut out = Vec::new();
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.id = v["message"]["id"].clone();
                self.model = v["message"]["model"].clone();
                self.usage = v["message"]["usage"].clone();
                out.push(self.chunk(json!({"role":"assistant","content":""}), Value::Null));
            }
            "content_block_start" => {
                let b = &v["content_block"];
                let index = v["index"].as_u64().unwrap_or(0);
                if b["type"] == "tool_use" {
                    let n = self.tools.len();
                    self.tools.insert(index, n);
                    out.push(self.chunk(json!({"tool_calls":[{"index":n,"id":b["id"],"type":"function","function":{"name":b["name"],"arguments":""}}]}),Value::Null));
                    if b["input"].as_object().is_some_and(|o| !o.is_empty()) {
                        out.push(self.chunk(json!({"tool_calls":[{"index":n,"function":{"arguments":b["input"].to_string()}}]}),Value::Null));
                    }
                } else if b["type"] == "thinking" {
                    if let Some(reasoning) = b["thinking"].as_str() {
                        out.push(self.chunk(json!({"reasoning_content":reasoning}), Value::Null));
                    }
                } else if let Some(text) = b["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push(self.chunk(json!({"content":text}), Value::Null));
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                if let Some(reasoning) = d["thinking"].as_str() {
                    out.push(self.chunk(json!({"reasoning_content":reasoning}), Value::Null));
                }
                if let Some(text) = d["text"].as_str() {
                    out.push(self.chunk(json!({"content":text}), Value::Null));
                }
                if let Some(args) = d["partial_json"].as_str() {
                    if let Some(n) = self.tools.get(&v["index"].as_u64().unwrap_or(0)) {
                        out.push(self.chunk(
                            json!({"tool_calls":[{"index":n,"function":{"arguments":args}}]}),
                            Value::Null,
                        ));
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = v["usage"].as_object() {
                    for (k, val) in u {
                        self.usage[k] = val.clone();
                    }
                }
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.terminal = true;
                    out.push(self.chunk(json!({}), json!(chat_stop(reason))));
                }
            }
            "message_stop" => return self.finish(),
            "error" => {
                return self.abort(v["error"]["message"].as_str().unwrap_or("upstream error"))
            }
            _ => {}
        }
        out
    }
    fn finish(&mut self) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        if !self.terminal {
            return self.abort("upstream stream ended before stop reason");
        }
        self.finished = true;
        let mut out = Vec::new();
        if self.include_usage {
            out.push(format!("data: {}\n\n",json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":chat_usage(&self.usage)})).into_bytes());
        }
        out.push(b"data: [DONE]\n\n".to_vec());
        out
    }
    fn abort(&mut self, reason: &str) -> Vec<Vec<u8>> {
        if self.finished {
            return vec![];
        }
        self.finished = true;
        vec![format!(
            "data: {}\n\n",
            json!({"error":{"type":"api_error","message":reason}})
        )
        .into_bytes()]
    }
    fn translate_complete(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        Self::complete(body)
    }
    fn stream_from_complete(&mut self, body: &[u8]) -> Vec<Vec<u8>> {
        let Ok(m) = serde_json::from_slice::<Value>(body) else {
            return self.abort("upstream returned non-JSON body");
        };
        if !m["content"].is_array() {
            return self.abort("upstream returned no translatable message");
        }
        let mut out = Vec::new();
        for f in crate::stream::message_to_sse(&m) {
            if let Some(d) = data(&f) {
                out.extend(self.push(d));
            }
        }
        out
    }
}

/// Compose existing translators while retaining incremental text delivery.
pub struct Chain {
    pub first: Box<dyn ResponseTranslator>,
    pub second: Box<dyn ResponseTranslator>,
}
impl Chain {
    fn feed(&mut self, frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        frames
            .into_iter()
            .flat_map(|f| data(&f).map(|d| self.second.push(d)).unwrap_or_default())
            .collect()
    }
}
impl ResponseTranslator for Chain {
    fn configure_client(&mut self, body: &[u8]) {
        self.second.configure_client(body);
    }
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>> {
        let f = self.first.push(event);
        self.feed(f)
    }
    fn finish(&mut self) -> Vec<Vec<u8>> {
        let f = self.first.finish();
        let mut out = self.feed(f);
        out.extend(self.second.finish());
        out
    }
    fn abort(&mut self, reason: &str) -> Vec<Vec<u8>> {
        let f = self.first.abort(reason);
        let mut out = self.feed(f);
        out.extend(self.second.abort(reason));
        out
    }
    fn translate_complete(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let b = self.first.translate_complete(body)?;
        self.second.translate_complete(&b)
    }
    fn stream_from_complete(&mut self, body: &[u8]) -> Vec<Vec<u8>> {
        let f = self.first.stream_from_complete(body);
        let mut out = self.feed(f);
        out.extend(self.second.finish());
        out
    }
}
