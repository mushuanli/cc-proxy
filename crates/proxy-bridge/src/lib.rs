//! Anthropic Messages ⇄ OpenAI Responses translation.
//!
//! Mechanism behind [`proxy_common::protocol::ProtocolAdapter`]. Lets a Claude
//! Code client (which speaks Anthropic Messages) ride an upstream that only
//! speaks the OpenAI Responses API — e.g. a ChatGPT/Codex subscription.
//!
//! # Scope
//!
//! | element | request | response |
//! |---|---|---|
//! | system prompt | ✅ → `instructions` | — |
//! | text messages | ✅ | ✅ |
//! | tool definitions | ✅ → `tools[].function` | — |
//! | `tool_choice` | ✅ | — |
//! | assistant `tool_use` | ✅ → `function_call` | ✅ ← `function_call` |
//! | user `tool_result` | ✅ → `function_call_output` | — |
//! | base64 images | ✅ → `input_image` data URL | — |
//! | usage | — | ✅ (incl. cached tokens) |
//! | thinking / redacted blocks | dropped (not representable) | — |
//!
//! Unknown Anthropic content blocks are dropped rather than failing the request:
//! a client that sends something exotic should still get an answer for the parts
//! that do translate.

use proxy_common::protocol::{
    ProtocolAdapter, ResponseTranslator, TranslatedRequest, WireProtocol,
};
use serde_json::{json, Map, Value};

mod reverse;
mod stream;

pub use reverse::ResponsesStreamTranslator;
pub use stream::AnthropicStreamTranslator;

/// Translates Anthropic Messages to and from OpenAI Responses.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicCodexAdapter;

impl AnthropicCodexAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProtocolAdapter for AnthropicCodexAdapter {
    fn supports(&self, from: WireProtocol, to: WireProtocol) -> bool {
        matches!(
            (from, to),
            (WireProtocol::Anthropic, WireProtocol::Codex)
                | (WireProtocol::Codex, WireProtocol::Anthropic)
        )
    }

    fn translate_request(
        &self,
        from: WireProtocol,
        to: WireProtocol,
        body: &[u8],
    ) -> Result<TranslatedRequest, String> {
        match (from, to) {
            (WireProtocol::Anthropic, WireProtocol::Codex) => messages_to_responses(body),
            (WireProtocol::Codex, WireProtocol::Anthropic) => responses_to_messages(body),
            (from, to) => Err(format!(
                "no request translation from {} to {}",
                from.as_str(),
                to.as_str()
            )),
        }
    }

    fn response_translator(
        &self,
        from: WireProtocol,
        to: WireProtocol,
    ) -> Option<Box<dyn ResponseTranslator>> {
        // `from` is the client protocol, `to` the upstream one. The upstream
        // answers in `to`, so translating back into `from` means Codex → Anthropic.
        match (from, to) {
            (WireProtocol::Anthropic, WireProtocol::Codex) => {
                Some(Box::new(AnthropicStreamTranslator::new()))
            }
            (WireProtocol::Codex, WireProtocol::Anthropic) => {
                Some(Box::new(ResponsesStreamTranslator::new()))
            }
            _ => None,
        }
    }
}

// ── Request: Anthropic Messages → OpenAI Responses ──

/// Translate a `/v1/messages` body into a `/responses` body.
pub fn messages_to_responses(body: &[u8]) -> Result<TranslatedRequest, String> {
    let request: Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid Anthropic body: {e}"))?;

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut out = Map::new();
    out.insert("model".into(), json!(model));

    if let Some(system) = flatten_system(request.get("system")) {
        if !system.is_empty() {
            out.insert("instructions".into(), json!(system));
        }
    }

    out.insert("input".into(), Value::Array(translate_messages(&request)?));

    if let Some(tools) = translate_tools(request.get("tools")) {
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
        if let Some(choice) = translate_tool_choice(request.get("tool_choice")) {
            out.insert("tool_choice".into(), choice);
        }
    }

    for (from, to) in [
        ("max_tokens", "max_output_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = request.get(from) {
            if !value.is_null() {
                out.insert(to.into(), value.clone());
            }
        }
    }

    // Responses has no equivalent for these; say so rather than letting a client
    // believe its stop sequences are enforced.
    for field in ["stop_sequences", "top_k", "metadata"] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            tracing::warn!("[bridge] Anthropic '{field}' has no Responses equivalent; dropped");
        }
    }

    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    out.insert("stream".into(), json!(stream));
    // Subscription-backed upstreams reject requests that ask to be persisted.
    out.insert("store".into(), json!(false));

    let bytes = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("failed to serialise Responses body: {e}"))?;
    Ok(TranslatedRequest {
        body: bytes,
        stream,
        model: Some(model),
    })
}

// ── Request: OpenAI Responses → Anthropic Messages ──

/// Translate a `/responses` body into a `/v1/messages` body.
pub fn responses_to_messages(body: &[u8]) -> Result<TranslatedRequest, String> {
    let request: Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid Responses body: {e}"))?;

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut out = Map::new();
    out.insert("model".into(), json!(model));

    let (messages, system_texts) = translate_input(&request)?;
    if let Some(system) = merge_system(request.get("instructions"), system_texts) {
        out.insert("system".into(), json!(system));
    }
    out.insert("messages".into(), Value::Array(messages));

    let tools = translate_tools_reverse(request.get("tools"));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = translate_tool_choice_reverse(request.get("tool_choice")) {
            out.insert("tool_choice".into(), choice);
        }
    }

    // Anthropic requires `max_tokens`; Responses makes it optional.
    out.insert(
        "max_tokens".into(),
        request
            .get("max_output_tokens")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );

    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(value) = request.get(from) {
            if !value.is_null() {
                out.insert(to.into(), value.clone());
            }
        }
    }

    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    out.insert("stream".into(), json!(stream));

    let bytes = serde_json::to_vec(&Value::Object(out))
        .map_err(|e| format!("failed to serialise Messages body: {e}"))?;
    Ok(TranslatedRequest {
        body: bytes,
        stream,
        model: Some(model),
    })
}

/// Expand Responses `input` items into Anthropic messages.
///
/// Anthropic requires every `tool_use` of a turn inside one assistant message,
/// answered by a single user message holding all matching `tool_result` blocks.
/// Responses instead emits one item per call, so consecutive items are grouped.
fn translate_input(request: &Value) -> Result<(Vec<Value>, Vec<String>), String> {
    let input = request
        .get("input")
        .ok_or_else(|| "Responses body has no input".to_string())?;

    if let Some(text) = input.as_str() {
        return Ok((vec![json!({"role": "user", "content": text})], Vec::new()));
    }

    let items = input
        .as_array()
        .ok_or_else(|| "Responses input must be a string or an array".to_string())?;

    let mut messages: Vec<Value> = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut system_texts: Vec<String> = Vec::new();

    for item in items {
        // A bare string is Responses shorthand for a user message.
        if let Some(text) = item.as_str() {
            flush(&mut messages, "assistant", &mut calls);
            flush(&mut messages, "user", &mut results);
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": text}]}));
            continue;
        }
        // EasyInputMessage omits `type`; without this the whole message was
        // silently dropped and the upstream rejected an empty conversation.
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let kind = if kind.is_empty() && item.get("role").is_some() {
            "message"
        } else {
            kind
        };
        match kind {
            "function_call" => {
                flush(&mut messages, "user", &mut results);
                calls.push(tool_use_block(item));
            }
            "function_call_output" => {
                flush(&mut messages, "assistant", &mut calls);
                results.push(tool_result_block(item));
            }
            "message" => {
                flush(&mut messages, "assistant", &mut calls);
                flush(&mut messages, "user", &mut results);
                // Anthropic allows only user/assistant inside `messages`; a
                // system or developer turn belongs in the top-level `system`.
                if matches!(
                    item.get("role").and_then(Value::as_str).unwrap_or("user"),
                    "system" | "developer"
                ) {
                    if let Some(text) = message_text(item) {
                        system_texts.push(text);
                    }
                    continue;
                }
                if let Some(message) = message_item(item) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }
    flush(&mut messages, "assistant", &mut calls);
    flush(&mut messages, "user", &mut results);
    Ok((messages, system_texts))
}

/// Join the top-level `instructions` with any system turns found in `input`.
fn merge_system(instructions: Option<&Value>, system_texts: Vec<String>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(text) = instructions.and_then(Value::as_str) {
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }
    parts.extend(system_texts.into_iter().filter(|text| !text.is_empty()));
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Plain text of a message item, regardless of how its content is shaped.
fn message_text(item: &Value) -> Option<String> {
    let text = match item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Emit a grouped message if any blocks accumulated.
fn flush(messages: &mut Vec<Value>, role: &str, blocks: &mut Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    let content = std::mem::take(blocks);
    messages.push(json!({"role": role, "content": content}));
}

fn tool_use_block(item: &Value) -> Value {
    let input = item
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
        "input": input,
    })
}

fn tool_result_block(item: &Value) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
        "content": item.get("output").and_then(Value::as_str).unwrap_or_default(),
    })
}

fn message_item(item: &Value) -> Option<Value> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    let blocks: Vec<Value> = match item.get("content") {
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(parts)) => parts.iter().filter_map(translate_part).collect(),
        _ => Vec::new(),
    };
    if blocks.is_empty() {
        return None;
    }
    Some(json!({"role": role, "content": blocks}))
}

/// One Responses content part → one Anthropic content block.
fn translate_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str)? {
        "input_text" | "output_text" => Some(
            json!({"type": "text", "text": part.get("text").and_then(Value::as_str).unwrap_or_default()}),
        ),
        "input_image" | "image_url" => {
            let url = part.get("image_url").and_then(|value| match value {
                Value::String(url) => Some(url.as_str()),
                Value::Object(_) => value.get("url").and_then(Value::as_str),
                _ => None,
            })?;
            Some(image_block(url))
        }
        _ => None,
    }
}

/// A `data:` URL becomes an Anthropic base64 source, anything else a URL source.
fn image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((media, data)) = rest.split_once(";base64,") {
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media, "data": data},
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// Responses `tools[].function` shape → Anthropic tool definitions.
fn translate_tools_reverse(tools: Option<&Value>) -> Vec<Value> {
    let Some(tools) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let mut entry = Map::new();
            entry.insert("name".into(), json!(name));
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                entry.insert("description".into(), json!(description));
            }
            entry.insert(
                "input_schema".into(),
                tool.get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            );
            Some(Value::Object(entry))
        })
        .collect()
}

/// Responses `tool_choice` → Anthropic `tool_choice`.
fn translate_tool_choice_reverse(choice: Option<&Value>) -> Option<Value> {
    match choice? {
        Value::String(kind) => match kind.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            // Anthropic has no "none"; leaving it out keeps the default (`auto`)
            // rather than inventing a value the upstream rejects.
            _ => None,
        },
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("function") => map
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "tool", "name": name})),
        _ => None,
    }
}

/// Anthropic `system` is either a string or an array of text blocks.
fn flatten_system(system: Option<&Value>) -> Option<String> {
    match system? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|block| {
                    (block.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| block.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect();
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

fn translate_messages(request: &Value) -> Result<Vec<Value>, String> {
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Anthropic body has no messages array".to_string())?;

    let mut items = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match message.get("content") {
            Some(Value::String(text)) => {
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": "input_text", "text": text}],
                }));
            }
            Some(Value::Array(blocks)) => translate_blocks(role, blocks, &mut items),
            _ => {}
        }
    }
    Ok(items)
}

/// Turn one Anthropic message's content blocks into Responses input items.
///
/// A single Anthropic message can mix text, tool calls and tool results, but
/// Responses wants them as separate items of distinct types, so blocks are
/// expanded rather than wrapped.
fn translate_blocks(role: &str, blocks: &[Value], items: &mut Vec<Value>) {
    let mut pending_text: Vec<Value> = Vec::new();

    let flush_text = |items: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        let content = std::mem::take(pending);
        items.push(json!({ "type": "message", "role": role, "content": content }));
    };

    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let kind = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    pending_text.push(json!({ "type": kind, "text": text }));
                }
            }
            "image" => {
                if let Some(part) = translate_image(block) {
                    pending_text.push(part);
                }
            }
            "tool_use" => {
                flush_text(items, &mut pending_text);
                items.push(translate_tool_use(block));
            }
            "tool_result" => {
                flush_text(items, &mut pending_text);
                items.push(translate_tool_result(block));
            }
            // `thinking` and `redacted_thinking` have no Responses equivalent and
            // are dropped on purpose.
            _ => {}
        }
    }
    flush_text(items, &mut pending_text);
}

fn translate_tool_use(block: &Value) -> Value {
    let arguments = block
        .get("input")
        .map(|input| input.to_string())
        .unwrap_or_else(|| "{}".to_string());
    json!({
        "type": "function_call",
        "call_id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name": block.get("name").and_then(Value::as_str).unwrap_or_default(),
        "arguments": arguments,
    })
}

fn translate_tool_result(block: &Value) -> Value {
    let output = match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    json!({
        "type": "function_call_output",
        "call_id": block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
        "output": output,
    })
}

/// Anthropic base64/url images → an OpenAI `input_image` part.
fn translate_image(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str)? {
        "base64" => {
            let media = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(json!({
                "type": "input_image",
                "image_url": format!("data:{media};base64,{data}"),
            }))
        }
        "url" => Some(json!({
            "type": "input_image",
            "image_url": source.get("url").and_then(Value::as_str)?,
        })),
        _ => None,
    }
}

/// Anthropic `tools` → Responses `tools`.
fn translate_tools(tools: Option<&Value>) -> Option<Vec<Value>> {
    let tools = tools?.as_array()?;
    Some(
        tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                let mut entry = Map::new();
                entry.insert("type".into(), json!("function"));
                entry.insert("name".into(), json!(name));
                if let Some(description) = tool.get("description").and_then(Value::as_str) {
                    entry.insert("description".into(), json!(description));
                }
                let schema = tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                entry.insert("parameters".into(), schema);
                Some(Value::Object(entry))
            })
            .collect(),
    )
}

/// Anthropic `tool_choice` → Responses `tool_choice`.
fn translate_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    let kind = choice.get("type").and_then(Value::as_str)?;
    match kind {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "name": name})),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(value: Value) -> Value {
        let bytes = serde_json::to_vec(&value).unwrap();
        let translated = messages_to_responses(&bytes).unwrap();
        serde_json::from_slice(&translated.body).unwrap()
    }

    #[test]
    fn supports_both_directions_but_only_for_anthropic_codex() {
        let adapter = AnthropicCodexAdapter::new();
        assert!(adapter.supports(WireProtocol::Anthropic, WireProtocol::Codex));
        assert!(adapter.supports(WireProtocol::Codex, WireProtocol::Anthropic));
        assert!(!adapter.supports(WireProtocol::Codex, WireProtocol::Codex));
        assert!(!adapter.supports(WireProtocol::Anthropic, WireProtocol::Anthropic));
    }

    #[test]
    fn simple_text_request_translates() {
        let out = translate(json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "system": "be terse",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        }));

        assert_eq!(out["model"], "claude-sonnet-4-5");
        assert_eq!(out["instructions"], "be terse");
        assert_eq!(out["max_output_tokens"], 1024);
        assert_eq!(out["stream"], true);
        assert_eq!(out["store"], false);
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn system_as_text_blocks_is_flattened() {
        let out = translate(json!({
            "model": "m",
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": [{"role": "user", "content": "x"}],
        }));
        assert_eq!(out["instructions"], "a\nb");
    }

    #[test]
    fn assistant_tool_use_and_user_tool_result_become_function_items() {
        let out = translate(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "read the file"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "sure"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "file body"}
                ]}
            ]
        }));

        let input = out["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            4,
            "text and tool items are expanded: {input:?}"
        );

        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");

        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "toolu_1");
        assert_eq!(input[2]["name"], "Read");
        assert_eq!(input[2]["arguments"], "{\"path\":\"/a\"}");

        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "toolu_1");
        assert_eq!(input[3]["output"], "file body");
    }

    #[test]
    fn tool_result_accepts_block_content() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": [{"type": "text", "text": "line1"}, {"type": "text", "text": "line2"}]}
            ]}]
        }));
        assert_eq!(out["input"][0]["output"], "line1\nline2");
    }

    #[test]
    fn tools_and_tool_choice_translate() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{
                "name": "Read",
                "description": "read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "tool_choice": {"type": "auto"},
        }));
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["name"], "Read");
        assert_eq!(
            out["tools"][0]["parameters"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn tool_choice_variants_map() {
        let choice = |value: Value| {
            translate(json!({
                "model": "m",
                "messages": [{"role": "user", "content": "x"}],
                "tools": [{"name": "T", "input_schema": {}}],
                "tool_choice": value,
            }))["tool_choice"]
                .clone()
        };
        assert_eq!(choice(json!({"type": "any"})), "required");
        assert_eq!(choice(json!({"type": "none"})), "none");
        assert_eq!(
            choice(json!({"type": "tool", "name": "T"})),
            json!({"type": "function", "name": "T"})
        );
    }

    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "tool_choice": {"type": "auto"},
        }));
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("tools").is_none());
    }

    #[test]
    fn base64_image_becomes_a_data_url() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}]
        }));
        let content = &out["input"][0]["content"];
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn thinking_blocks_are_dropped_without_failing() {
        let out = translate(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret"},
                {"type": "text", "text": "visible"}
            ]}]
        }));
        let content = out["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "visible");
    }

    #[test]
    fn optional_sampling_params_are_carried_over() {
        let out = translate(json!({
            "model": "m",
            "temperature": 0.2,
            "top_p": 0.9,
            "messages": [{"role": "user", "content": "x"}],
        }));
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["top_p"], 0.9);
    }

    #[test]
    fn streaming_defaults_to_false() {
        let bytes = serde_json::to_vec(&json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
        }))
        .unwrap();
        let translated = messages_to_responses(&bytes).unwrap();
        assert!(!translated.stream);
        assert_eq!(translated.model.as_deref(), Some("m"));
    }

    #[test]
    fn a_body_without_messages_is_rejected() {
        let bytes = serde_json::to_vec(&json!({"model": "m"})).unwrap();
        assert!(messages_to_responses(&bytes)
            .unwrap_err()
            .contains("no messages array"));
    }

    #[test]
    fn a_non_json_body_is_rejected() {
        assert!(messages_to_responses(b"not json").is_err());
    }

    // ── Reverse request: Responses → Messages ──

    fn translate_reverse(value: Value) -> Value {
        let bytes = serde_json::to_vec(&value).unwrap();
        let translated = responses_to_messages(&bytes).unwrap();
        serde_json::from_slice(&translated.body).unwrap()
    }

    #[test]
    fn reverse_simple_text_request_translates() {
        let out = translate_reverse(json!({
            "model": "gpt-5",
            "instructions": "be terse",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "hello"}]}],
            "max_output_tokens": 512,
            "stream": true,
        }));

        assert_eq!(out["model"], "gpt-5");
        assert_eq!(out["system"], "be terse");
        assert_eq!(out["max_tokens"], 512);
        assert_eq!(out["stream"], true);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["type"], "text");
        assert_eq!(out["messages"][0]["content"][0]["text"], "hello");
        assert!(out.get("store").is_none(), "Anthropic has no store flag");
    }

    #[test]
    fn reverse_input_as_a_bare_string_is_accepted() {
        let out = translate_reverse(json!({"model": "m", "input": "just text"}));
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "just text");
    }

    #[test]
    fn reverse_defaults_max_tokens_because_anthropic_requires_it() {
        let out = translate_reverse(json!({"model": "m", "input": "x"}));
        assert_eq!(out["max_tokens"], 4096);
    }

    #[test]
    fn reverse_groups_consecutive_calls_into_one_assistant_message() {
        // Anthropic wants every tool_use of a turn in a single assistant message.
        let out = translate_reverse(json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
                {"type": "function_call", "call_id": "c1", "name": "Read", "arguments": "{\"path\":\"/a\"}"},
                {"type": "function_call", "call_id": "c2", "name": "Grep", "arguments": "{\"pattern\":\"x\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "file a"},
                {"type": "function_call_output", "call_id": "c2", "output": "match x"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]},
            ]
        }));

        let messages = out["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            4,
            "user, assistant(tools), user(results), assistant: {messages:?}"
        );

        assert_eq!(messages[1]["role"], "assistant");
        let calls = messages[1]["content"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "both calls share one assistant message");
        assert_eq!(calls[0]["type"], "tool_use");
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["name"], "Read");
        assert_eq!(calls[0]["input"]["path"], "/a");
        assert_eq!(calls[1]["id"], "c2");

        assert_eq!(messages[2]["role"], "user");
        let results = messages[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both results share one user message");
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "c1");
        assert_eq!(results[0]["content"], "file a");
    }

    #[test]
    fn reverse_tools_and_tool_choice_translate() {
        let out = translate_reverse(json!({
            "model": "m",
            "input": "x",
            "tools": [{"type": "function", "name": "Read", "description": "read",
                       "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}],
            "tool_choice": "required",
        }));
        assert_eq!(out["tools"][0]["name"], "Read");
        assert_eq!(out["tools"][0]["description"], "read");
        assert_eq!(
            out["tools"][0]["input_schema"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(out["tool_choice"]["type"], "any");
    }

    #[test]
    fn reverse_tool_choice_variants_map() {
        let choice = |value: Value| {
            translate_reverse(json!({
                "model": "m", "input": "x",
                "tools": [{"type": "function", "name": "T", "parameters": {}}],
                "tool_choice": value,
            }))
            .get("tool_choice")
            .cloned()
        };
        assert_eq!(choice(json!("auto")), Some(json!({"type": "auto"})));
        assert_eq!(
            choice(json!({"type": "function", "name": "T"})),
            Some(json!({"type": "tool", "name": "T"}))
        );
        // "none" has no Anthropic equivalent; omit rather than invent one.
        assert_eq!(choice(json!("none")), None);
    }

    #[test]
    fn reverse_data_url_image_becomes_a_base64_source() {
        let out = translate_reverse(json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ]}]
        }));
        let content = &out["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn reverse_plain_url_image_becomes_a_url_source() {
        let out = translate_reverse(json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_image", "image_url": "https://example.com/a.png"}
            ]}]
        }));
        assert_eq!(out["messages"][0]["content"][0]["source"]["type"], "url");
    }

    #[test]
    fn reverse_rejects_a_body_without_input() {
        let bytes = serde_json::to_vec(&json!({"model": "m"})).unwrap();
        assert!(responses_to_messages(&bytes)
            .unwrap_err()
            .contains("no input"));
    }

    #[test]
    fn reverse_rejects_a_non_json_body() {
        assert!(responses_to_messages(b"not json").is_err());
    }

    #[test]
    fn reverse_streaming_defaults_to_false() {
        let bytes = serde_json::to_vec(&json!({"model": "m", "input": "x"})).unwrap();
        let translated = responses_to_messages(&bytes).unwrap();
        assert!(!translated.stream);
        assert_eq!(translated.model.as_deref(), Some("m"));
    }

    #[test]
    fn both_directions_are_covered_by_the_adapter() {
        let adapter = AnthropicCodexAdapter::new();
        assert!(adapter
            .translate_request(
                WireProtocol::Codex,
                WireProtocol::Anthropic,
                br#"{"model":"m","input":"x"}"#
            )
            .is_ok());
        assert!(adapter
            .response_translator(WireProtocol::Codex, WireProtocol::Anthropic)
            .is_some());
        // Same-protocol pairs remain unsupported.
        assert!(adapter
            .translate_request(WireProtocol::Codex, WireProtocol::Codex, b"{}")
            .is_err());
    }

    #[test]
    fn input_without_type_is_treated_as_a_message() {
        // EasyInputMessage (the official SDK's default shape) omits `type`.
        let body = json!({
            "model": "gpt-5",
            "input": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let out = responses_to_messages(body.as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn string_input_items_become_user_messages() {
        let body = json!({"model": "gpt-5", "input": ["hello", "world"]}).to_string();
        let out = responses_to_messages(body.as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
        assert_eq!(out["messages"][1]["content"][0]["text"], "world");
    }

    #[test]
    fn a_system_turn_in_input_is_hoisted_out_of_messages() {
        // Anthropic only allows user/assistant inside `messages`; a system turn
        // left there makes the upstream reject the whole request.
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "system",
                 "content": [{"type": "input_text", "text": "be terse"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]},
            ],
        })
        .to_string();
        let out = responses_to_messages(body.as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(out["system"], "be terse");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages.iter().all(|message| message["role"] != "system"));
    }

    #[test]
    fn instructions_and_system_turns_are_joined() {
        let body = json!({
            "model": "gpt-5",
            "instructions": "top",
            "input": [
                {"type": "message", "role": "developer",
                 "content": [{"type": "input_text", "text": "extra"}]},
            ],
        })
        .to_string();
        let out = responses_to_messages(body.as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(out["system"], "top\n\nextra");
        assert!(out["messages"].as_array().unwrap().is_empty());
    }
}
