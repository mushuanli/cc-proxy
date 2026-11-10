//! Chat Completions requests, using Responses as the common request shape.
use proxy_common::protocol::TranslatedRequest;
use serde_json::{json, Map, Value};

fn parts(content: &Value, assistant: bool) -> Result<Value, String> {
    if content.is_null() {
        return Ok(json!([]));
    }
    if let Some(text) = content.as_str() {
        return Ok(
            json!([{"type": if assistant {"output_text"} else {"input_text"}, "text":text}]),
        );
    }
    let mut out = Vec::new();
    for part in content
        .as_array()
        .ok_or("message content must be text or an array")?
    {
        out.push(match part["type"].as_str().unwrap_or("") {
            "text" => json!({"type":if assistant {"output_text"} else {"input_text"},"text":part["text"]}),
            "image_url" => json!({"type":"input_image","image_url":part["image_url"]["url"],"detail":part["image_url"].get("detail").cloned().unwrap_or(json!("auto"))}),
            kind => return Err(format!("unsupported Chat Completions content type: {kind}")),
        });
    }
    Ok(json!(out))
}

fn translated(out: Map<String, Value>) -> Result<TranslatedRequest, String> {
    Ok(TranslatedRequest {
        stream: out.get("stream").and_then(Value::as_bool).unwrap_or(false),
        model: out.get("model").and_then(Value::as_str).map(str::to_owned),
        body: serde_json::to_vec(&out).map_err(|e| e.to_string())?,
    })
}

pub fn chat_to_responses(body: &[u8]) -> Result<TranslatedRequest, String> {
    let r: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if r.get("n").and_then(Value::as_u64).is_some_and(|n| n != 1) {
        return Err("cross-protocol translation supports only n=1".into());
    }
    let mut input = Vec::new();
    for m in r["messages"]
        .as_array()
        .ok_or("Chat Completions requires messages")?
    {
        if m["role"] == "tool" {
            input.push(json!({"type":"function_call_output","call_id":m["tool_call_id"],"output":m["content"]}));
            continue;
        }
        if !m["content"].is_null() {
            input.push(json!({"type":"message","role":m["role"],"content":parts(&m["content"],m["role"]=="assistant")?}));
        }
        for call in m["tool_calls"].as_array().into_iter().flatten() {
            input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
        }
    }
    let mut out = Map::new();
    copy_fields(
        &r,
        &mut out,
        &[
            "model",
            "stream",
            "temperature",
            "top_p",
            "parallel_tool_calls",
            "metadata",
        ],
    );
    out.insert("input".into(), json!(input));
    out.insert("store".into(), json!(false));
    if let Some(max) = r
        .get("max_completion_tokens")
        .or_else(|| r.get("max_tokens"))
    {
        out.insert("max_output_tokens".into(), max.clone());
    }
    if let Some(effort) = r.get("reasoning_effort") {
        out.insert("reasoning".into(), json!({"effort":effort}));
    }
    map_chat_tools(&r, &mut out)?;
    if let Some(format) = r.get("response_format") {
        let format = if format["type"] == "json_schema" {
            let mut schema = format["json_schema"]
                .as_object()
                .cloned()
                .ok_or("invalid json_schema")?;
            schema.insert("type".into(), json!("json_schema"));
            Value::Object(schema)
        } else {
            format.clone()
        };
        out.insert("text".into(), json!({"format":format}));
    }
    translated(out)
}

fn copy_fields(r: &Value, out: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(v) = r.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
}

fn map_chat_tools(r: &Value, out: &mut Map<String, Value>) -> Result<(), String> {
    if let Some(tools) = r["tools"].as_array() {
        let mut mapped = Vec::new();
        for tool in tools {
            if tool["type"] != "function" {
                return Err("only function tools can be translated".into());
            }
            let mut function = tool["function"]
                .as_object()
                .cloned()
                .ok_or("invalid function tool")?;
            function.insert("type".into(), json!("function"));
            mapped.push(Value::Object(function));
        }
        out.insert("tools".into(), json!(mapped));
    }
    if let Some(choice) = r.get("tool_choice") {
        out.insert(
            "tool_choice".into(),
            if choice.is_object() {
                json!({"type":"function","name":choice["function"]["name"]})
            } else {
                choice.clone()
            },
        );
    }
    Ok(())
}

pub fn responses_to_chat(body: &[u8]) -> Result<TranslatedRequest, String> {
    let r: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    // These require server-side Responses state or built-in tools and cannot be replayed as chat.
    for key in ["previous_response_id", "conversation"] {
        if r.get(key).is_some_and(|v| !v.is_null()) {
            return Err(format!(
                "{key} cannot be translated to Chat Completions; send full input history"
            ));
        }
    }
    let mut messages = Vec::new();
    if let Some(instructions) = r["instructions"].as_str() {
        messages.push(json!({"role":"system","content":instructions}));
    }
    if let Some(text) = r["input"].as_str() {
        messages.push(json!({"role":"user","content":text}));
    } else {
        for item in r["input"].as_array().ok_or("Responses requires input")? {
            append_input(&mut messages, item)?;
        }
    }
    let mut out = Map::new();
    copy_fields(
        &r,
        &mut out,
        &[
            "model",
            "stream",
            "temperature",
            "top_p",
            "parallel_tool_calls",
            "metadata",
        ],
    );
    out.insert("messages".into(), json!(messages));
    if let Some(max) = r.get("max_output_tokens") {
        out.insert("max_completion_tokens".into(), max.clone());
    }
    if let Some(effort) = r.pointer("/reasoning/effort") {
        out.insert("reasoning_effort".into(), effort.clone());
    }
    if r["stream"] == true {
        out.insert("stream_options".into(), json!({"include_usage":true}));
    }
    map_response_tools(&r, &mut out)?;
    if let Some(format) = r.pointer("/text/format") {
        let format = if format["type"] == "json_schema" {
            let mut schema = format.as_object().cloned().ok_or("invalid json_schema")?;
            schema.remove("type");
            json!({"type":"json_schema","json_schema":schema})
        } else {
            format.clone()
        };
        out.insert("response_format".into(), format);
    }
    translated(out)
}

fn append_input(messages: &mut Vec<Value>, item: &Value) -> Result<(), String> {
    if let Some(text) = item.as_str() {
        messages.push(json!({"role":"user","content":text}));
        return Ok(());
    }
    match item["type"].as_str().unwrap_or("message") {
        "message" => {
            let content = if item["content"].is_string() {
                item["content"].clone()
            } else {
                json!(item["content"]
                    .as_array()
                    .ok_or("invalid message content")?
                    .iter()
                    .map(chat_part)
                    .collect::<Result<Vec<_>, _>>()?)
            };
            let role = if item["role"] == "developer" {
                json!("system")
            } else {
                item["role"].clone()
            };
            if role == "assistant"
                && messages.last().is_some_and(|m| {
                    m["role"] == "assistant"
                        && m.get("reasoning_content").is_some()
                        && m["content"].is_null()
                        && m.get("tool_calls").is_none()
                })
            {
                messages.last_mut().unwrap()["content"] = content;
            } else {
                messages.push(json!({"role":role,"content":content}));
            }
        }
        "function_call" => {
            let call = json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}});
            if let Some(last) = messages.last_mut().filter(|m| m["role"] == "assistant") {
                if !last["tool_calls"].is_array() {
                    last["tool_calls"] = json!([]);
                }
                last["tool_calls"].as_array_mut().unwrap().push(call);
            } else {
                messages.push(json!({"role":"assistant","content":null,"tool_calls":[call]}));
            }
        }
        "function_call_output" => messages
            .push(json!({"role":"tool","tool_call_id":item["call_id"],"content":item["output"]})),
        "reasoning" => {
            let text = item["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["text"].as_str())
                .collect::<String>();
            messages.push(json!({"role":"assistant","content":null,"reasoning_content":text}));
        }
        kind => return Err(format!("unsupported Responses input item: {kind}")),
    }
    Ok(())
}

fn chat_part(part: &Value) -> Result<Value, String> {
    Ok(match part["type"].as_str().unwrap_or("") {
        "input_text" | "output_text" => json!({"type":"text","text":part["text"]}),
        "input_image" => {
            let url = part["image_url"]
                .as_str()
                .ok_or("Chat Completions requires an image URL, not a file_id")?;
            json!({"type":"image_url","image_url":{"url":url,"detail":part.get("detail").cloned().unwrap_or(json!("auto"))}})
        }
        kind => return Err(format!("unsupported Responses content type: {kind}")),
    })
}

fn map_response_tools(r: &Value, out: &mut Map<String, Value>) -> Result<(), String> {
    if let Some(tools) = r["tools"].as_array() {
        let mut mapped = Vec::new();
        for tool in tools {
            if tool["type"] != "function" {
                return Err(
                    "Responses built-in tools cannot be translated to Chat Completions".into(),
                );
            }
            let mut f = tool.as_object().cloned().unwrap();
            f.remove("type");
            mapped.push(json!({"type":"function","function":f}));
        }
        out.insert("tools".into(), json!(mapped));
    }
    if let Some(choice) = r.get("tool_choice") {
        out.insert(
            "tool_choice".into(),
            if choice.is_object() {
                if choice["type"] != "function" {
                    return Err("unsupported Responses tool_choice".into());
                }
                json!({"type":"function","function":{"name":choice["name"]}})
            } else {
                choice.clone()
            },
        );
    }
    Ok(())
}

/// Keep each Anthropic assistant turn intact, including its thinking and calls.
pub fn messages_to_chat(body: &[u8]) -> Result<TranslatedRequest, String> {
    let source: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut result = responses_to_chat(&crate::messages_to_responses(body)?.body)?;
    let mut out: Value = serde_json::from_slice(&result.body).map_err(|e| e.to_string())?;
    let mut messages = Vec::new();
    if let Some(system) = crate::flatten_system(source.get("system")) {
        if !system.is_empty() {
            messages.push(json!({"role":"system","content":system}));
        }
    }
    for message in source["messages"]
        .as_array()
        .ok_or("Anthropic requires messages")?
    {
        if message["role"] == "assistant" {
            messages.push(assistant_to_chat(message)?);
        } else {
            let single = serde_json::to_vec(&json!({"model":source["model"],"messages":[message]}))
                .map_err(|e| e.to_string())?;
            let converted = responses_to_chat(&crate::messages_to_responses(&single)?.body)?;
            let converted: Value =
                serde_json::from_slice(&converted.body).map_err(|e| e.to_string())?;
            messages.extend(
                converted["messages"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }
    out["messages"] = json!(messages);
    result.body = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
    Ok(result)
}

fn assistant_to_chat(message: &Value) -> Result<Value, String> {
    let mut out = json!({"role":"assistant","content":null});
    if message["content"].is_string() {
        out["content"] = message["content"].clone();
        return Ok(out);
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut saw_thinking = false;
    let mut calls = Vec::new();
    for block in message["content"]
        .as_array()
        .ok_or("invalid assistant content")?
    {
        match block["type"].as_str().unwrap_or("") {
            "thinking" => { saw_thinking=true; reasoning.push_str(block["thinking"].as_str().unwrap_or("")); },
            "text" => text.push_str(block["text"].as_str().unwrap_or("")),
            "tool_use" => calls.push(json!({"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":block["input"].to_string()}})),
            _ => {},
        }
    }
    if !text.is_empty() {
        out["content"] = json!(text);
    }
    if saw_thinking {
        out["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        out["tool_calls"] = json!(calls);
    }
    Ok(out)
}

/// Restore Chat reasoning as a thinking block in the same assistant turn.
pub fn chat_to_messages(body: &[u8]) -> Result<TranslatedRequest, String> {
    let source: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let mut result = crate::responses_to_messages(&chat_to_responses(body)?.body)?;
    let mut out: Value = serde_json::from_slice(&result.body).map_err(|e| e.to_string())?;
    let mut messages: Vec<Value> = Vec::new();
    for message in source["messages"]
        .as_array()
        .ok_or("Chat Completions requires messages")?
    {
        if matches!(message["role"].as_str(), Some("system" | "developer")) {
            continue;
        }
        let single = serde_json::to_vec(&json!({"model":source["model"],"messages":[message]}))
            .map_err(|e| e.to_string())?;
        let converted = crate::responses_to_messages(&chat_to_responses(&single)?.body)?;
        let converted: Value =
            serde_json::from_slice(&converted.body).map_err(|e| e.to_string())?;
        let mut blocks = Vec::new();
        if let Some(reasoning) = message["reasoning_content"].as_str() {
            blocks.push(json!({"type":"thinking","thinking":reasoning,"signature":crate::chat_stream::REASONING_SIGNATURE}));
        }
        for m in converted["messages"].as_array().into_iter().flatten() {
            blocks.extend(m["content"].as_array().cloned().unwrap_or_default());
        }
        let role = if message["role"] == "assistant" {
            "assistant"
        } else {
            "user"
        };
        if message["role"] == "tool" && messages.last().is_some_and(|m| m["role"] == "user") {
            messages.last_mut().unwrap()["content"]
                .as_array_mut()
                .unwrap()
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    out["messages"] = json!(messages);
    result.body = serde_json::to_vec(&out).map_err(|e| e.to_string())?;
    Ok(result)
}
