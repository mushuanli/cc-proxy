use proxy_bridge::AnthropicCodexAdapter;
use proxy_common::protocol::{ProtocolAdapter, ResponseTranslator, WireProtocol as P};
use serde_json::{json, Value};

fn request(from: P, to: P, value: Value) -> Value {
    let translated = AnthropicCodexAdapter::new()
        .translate_request(from, to, &serde_json::to_vec(&value).unwrap())
        .unwrap();
    serde_json::from_slice(&translated.body).unwrap()
}
fn translator(client: P, upstream: P) -> Box<dyn ResponseTranslator> {
    AnthropicCodexAdapter::new()
        .response_translator(client, upstream)
        .unwrap()
}
fn payloads(frames: &[Vec<u8>]) -> Vec<Value> {
    frames
        .iter()
        .flat_map(|f| std::str::from_utf8(f).unwrap().lines())
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}
fn push(t: &mut dyn ResponseTranslator, v: Value) -> Vec<Vec<u8>> {
    t.push(&serde_json::to_vec(&v).unwrap())
}
fn chat_message() -> Value {
    json!({"id":"chatcmpl-1","model":"kimi","choices":[{"index":0,"message":{"role":"assistant","content":"你好","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"上海\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":5,"total_tokens":25,"prompt_tokens_details":{"cached_tokens":7}}})
}

#[test]
fn responses_to_chat_preserves_history_images_tools_and_options() {
    let r = request(
        P::Codex,
        P::ChatCompletions,
        json!({"model":"kimi","stream":true,"instructions":"be brief",
            "input":[{"role":"user","content":[{"type":"input_text","text":"what is here?"},{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]},
            {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
            {"type":"function_call","call_id":"call_2","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_1","output":"sunny"}],
            "tools":[{"type":"function","name":"lookup","description":"weather","parameters":{"type":"object"},"strict":true}],
            "tool_choice":{"type":"function","name":"lookup"},"max_output_tokens":100,"reasoning":{"effort":"high"},
            "text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"},"strict":true}}
        }),
    );
    assert_eq!(
        r["messages"][0],
        json!({"role":"system","content":"be brief"})
    );
    assert_eq!(
        r["messages"][1]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
    assert_eq!(r["messages"][2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(r["messages"][3]["tool_call_id"], "call_1");
    assert_eq!(r["tools"][0]["function"]["strict"], true);
    assert_eq!(r["tool_choice"]["function"]["name"], "lookup");
    assert_eq!(r["max_completion_tokens"], 100);
    assert_eq!(r["reasoning_effort"], "high");
    assert_eq!(r["response_format"]["json_schema"]["name"], "answer");
    assert_eq!(r["stream_options"]["include_usage"], true);
}

#[test]
fn chat_to_responses_preserves_tools_system_and_content() {
    let r = request(
        P::ChatCompletions,
        P::Codex,
        json!({"model":"kimi","messages":[
        {"role":"system","content":"brief"},{"role":"user","content":"hi"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"c1","content":"ok"}],
        "tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"f"}},"max_tokens":12,"reasoning_effort":"low"}),
    );
    assert_eq!(r["input"][0]["role"], "system");
    assert_eq!(r["input"][1]["content"][0]["text"], "hi");
    assert_eq!(r["input"][2]["type"], "function_call");
    assert_eq!(r["input"][3]["output"], "ok");
    assert_eq!(r["tools"][0]["name"], "f");
    assert_eq!(r["tool_choice"]["name"], "f");
    assert_eq!(r["max_output_tokens"], 12);
    assert_eq!(r["reasoning"]["effort"], "low");
}

#[test]
fn anthropic_chat_requests_work_in_both_directions() {
    let chat = request(
        P::Anthropic,
        P::ChatCompletions,
        json!({"model":"kimi","system":"brief","max_tokens":100,"messages":[{"role":"user","content":"你好"}]}),
    );
    assert_eq!(chat["messages"][0]["role"], "system");
    assert_eq!(chat["messages"][1]["content"][0]["text"], "你好");
    let anthropic = request(P::ChatCompletions, P::Anthropic, chat);
    assert_eq!(anthropic["system"], "brief");
    assert_eq!(anthropic["max_tokens"], 100);
    assert_eq!(anthropic["messages"][0]["role"], "user");
}

#[test]
fn unrepresentable_requests_fail_explicitly() {
    let a = AnthropicCodexAdapter::new();
    for r in [
        json!({"input":"hi","previous_response_id":"resp_1"}),
        json!({"input":"hi","tools":[{"type":"web_search"}]}),
    ] {
        assert!(a
            .translate_request(
                P::Codex,
                P::ChatCompletions,
                &serde_json::to_vec(&r).unwrap()
            )
            .is_err());
    }
    assert!(a
        .translate_request(P::ChatCompletions, P::Codex, br#"{"messages":[],"n":2}"#)
        .is_err());
}

#[test]
fn complete_chat_responses_translate_to_both_protocols_with_cached_usage() {
    let bytes = serde_json::to_vec(&chat_message()).unwrap();
    let a: Value = serde_json::from_slice(
        &translator(P::Anthropic, P::ChatCompletions)
            .translate_complete(&bytes)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(a["content"][0]["text"], "你好");
    assert_eq!(a["content"][1]["input"]["city"], "上海");
    assert_eq!(a["stop_reason"], "tool_use");
    assert_eq!(a["usage"]["input_tokens"], 13);
    assert_eq!(a["usage"]["cache_read_input_tokens"], 7);
    let r: Value = serde_json::from_slice(
        &translator(P::Codex, P::ChatCompletions)
            .translate_complete(&bytes)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(r["output"][1]["call_id"], "call_1");
    assert_eq!(r["usage"]["input_tokens"], 20);
    assert_eq!(r["usage"]["input_tokens_details"]["cached_tokens"], 7);
}

#[test]
fn complete_responses_convert_to_chat_with_total_usage() {
    let bytes=serde_json::to_vec(&json!({"id":"resp_1","model":"kimi","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"}],"usage":{"input_tokens":20,"output_tokens":5,"input_tokens_details":{"cached_tokens":7}}})).unwrap();
    let r: Value = serde_json::from_slice(
        &translator(P::ChatCompletions, P::Codex)
            .translate_complete(&bytes)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(r["choices"][0]["message"]["content"], "hi");
    assert_eq!(r["choices"][0]["message"]["tool_calls"][0]["id"], "c1");
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(r["usage"]["prompt_tokens"], 20);
    assert_eq!(r["usage"]["total_tokens"], 25);
}

#[test]
fn streaming_chat_delivers_text_early_and_keeps_interleaved_tools_separate() {
    let mut t = translator(P::Codex, P::ChatCompletions);
    let first = push(
        t.as_mut(),
        json!({"id":"c1","model":"kimi","choices":[{"index":0,"delta":{"content":"你好"},"finish_reason":null}]}),
    );
    assert!(payloads(&first)
        .iter()
        .any(|v| v["type"] == "response.output_text.delta" && v["delta"] == "你好"));
    let mut frames = Vec::new();
    for delta in [
        json!({"tool_calls":[{"index":0,"id":"t1","function":{"name":"one","arguments":"{\"a\":"}},{"index":1,"id":"t2","function":{"name":"two","arguments":"{\"b\":"}}]}),
        json!({"tool_calls":[{"index":1,"function":{"arguments":"2}"}},{"index":0,"function":{"arguments":"1}"}}]}),
    ] {
        frames.extend(push(
            t.as_mut(),
            json!({"choices":[{"delta":delta,"finish_reason":null}]}),
        ));
    }
    frames.extend(push(
        t.as_mut(),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ));
    frames.extend(push(t.as_mut(),json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":7}}})));
    frames.extend(t.push(b"[DONE]"));
    assert!(t.finish().is_empty());
    let events = payloads(&frames);
    let complete = events
        .iter()
        .find(|v| v["type"] == "response.completed")
        .unwrap();
    let output = complete["response"]["output"].as_array().unwrap();
    assert_eq!(output[1]["arguments"], "{\"a\":1}");
    assert_eq!(output[2]["arguments"], "{\"b\":2}");
    assert_eq!(complete["response"]["usage"]["input_tokens"], 20);
}

#[test]
fn json_fallback_and_client_usage_options_generate_chat_sse() {
    let body=serde_json::to_vec(&json!({"id":"m1","model":"kimi","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}})).unwrap();
    for include in [true, false] {
        let mut t = translator(P::ChatCompletions, P::Anthropic);
        t.configure_client(
            &serde_json::to_vec(&json!({"stream_options":{"include_usage":include}})).unwrap(),
        );
        let frames = t.stream_from_complete(&body);
        assert_eq!(frames.last().unwrap(), b"data: [DONE]\n\n");
        assert_eq!(
            payloads(&frames).iter().any(|v| v.get("usage").is_some()),
            include
        );
    }
    let mut t = translator(P::ChatCompletions, P::Codex);
    let frames=t.stream_from_complete(br#"{"id":"r1","status":"completed","model":"kimi","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}"#);
    assert!(payloads(&frames)
        .iter()
        .any(|v| v["choices"][0]["delta"]["content"] == "ok"));
    assert_eq!(frames.last().unwrap(), b"data: [DONE]\n\n");
}

#[test]
fn missing_finish_reason_and_transport_errors_do_not_report_success() {
    for client in [P::Anthropic, P::Codex] {
        let mut t = translator(client, P::ChatCompletions);
        push(
            t.as_mut(),
            json!({"choices":[{"delta":{"content":"partial"}}]}),
        );
        let frames = t.finish();
        let events = payloads(&frames);
        assert!(events
            .iter()
            .any(|v| v["type"] == "error" || v["type"] == "response.failed"));
        assert!(!events
            .iter()
            .any(|v| v["type"] == "message_stop" || v["type"] == "response.completed"));
        assert!(t.finish().is_empty());
    }
    let mut t = translator(P::ChatCompletions, P::Codex);
    let frames = t.abort("connection reset");
    assert!(payloads(&frames)
        .iter()
        .any(|v| v["error"]["message"] == "connection reset"));
    assert!(!frames.iter().any(|f| f == b"data: [DONE]\n\n"));
}

#[test]
fn token_limit_is_an_incomplete_responses_result_in_json_and_sse() {
    let body = json!({"id":"c1","model":"kimi","choices":[{"message":{"content":"partial"},"finish_reason":"length"}]});
    let mut t = translator(P::Codex, P::ChatCompletions);
    let result: Value = serde_json::from_slice(
        &t.translate_complete(&serde_json::to_vec(&body).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["status"], "incomplete");
    assert_eq!(result["incomplete_details"]["reason"], "max_output_tokens");
    let mut t = translator(P::Codex, P::ChatCompletions);
    let frames = t.stream_from_complete(&serde_json::to_vec(&body).unwrap());
    assert_eq!(
        payloads(&frames).last().unwrap()["type"],
        "response.incomplete"
    );
    let mut t = translator(P::Codex, P::ChatCompletions);
    push(
        t.as_mut(),
        json!({"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]}),
    );
    let frames = t.finish();
    assert_eq!(
        payloads(&frames).last().unwrap()["type"],
        "response.incomplete"
    );
}

#[test]
fn thinking_and_tools_round_trip_in_one_assistant_turn() {
    let body = json!({"model":"deepseek","messages":[{"role":"user","content":"check a file"},
        {"role":"assistant","content":[{"type":"thinking","thinking":"I will inspect the file.","signature":"cc-proxy-chat-reasoning-v1"},{"type":"text","text":"Checking."},{"type":"tool_use","id":"call_1","name":"Read","input":{"file_path":"/tmp/probe"}},{"type":"tool_use","id":"call_2","name":"Read","input":{"file_path":"/tmp/other"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"one"},{"type":"tool_result","tool_use_id":"call_2","content":"two"}]}]});
    let chat = request(P::Anthropic, P::ChatCompletions, body);
    assert_eq!(
        chat["messages"][1]["reasoning_content"],
        "I will inspect the file."
    );
    assert_eq!(chat["messages"][1]["content"], "Checking.");
    assert_eq!(
        chat["messages"][1]["tool_calls"].as_array().unwrap().len(),
        2
    );
    assert_eq!(chat["messages"].as_array().unwrap().len(), 4);
    let restored = request(P::ChatCompletions, P::Anthropic, chat);
    let content = restored["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["thinking"], "I will inspect the file.");
    assert_eq!(
        content.iter().filter(|b| b["type"] == "tool_use").count(),
        2
    );
    assert_eq!(
        restored["messages"][2]["content"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn chat_reasoning_stream_is_replayable_after_a_tool_call() {
    let mut t = translator(P::Anthropic, P::ChatCompletions);
    let mut frames = Vec::new();
    for delta in [
        json!({"reasoning_content":""}),
        json!({"reasoning_content":"Inspect "}),
        json!({"reasoning_content":"the file."}),
        json!({"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Read","arguments":"{}"}}]}),
    ] {
        frames.extend(push(
            t.as_mut(),
            json!({"id":"c1","model":"deepseek","choices":[{"delta":delta,"finish_reason":null}]}),
        ));
    }
    frames.extend(push(
        t.as_mut(),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ));
    frames.extend(t.push(b"[DONE]"));
    let events = payloads(&frames);
    assert_eq!(
        events
            .iter()
            .filter_map(|v| v.pointer("/delta/thinking").and_then(Value::as_str))
            .collect::<String>(),
        "Inspect the file."
    );
    assert!(events
        .iter()
        .any(|v| v.pointer("/delta/signature").and_then(Value::as_str)
            == Some("cc-proxy-chat-reasoning-v1")));
    let thinking_stop = events
        .iter()
        .position(|v| v["type"] == "content_block_stop" && v["index"] == 0)
        .unwrap();
    let tool_start = events
        .iter()
        .position(|v| v.pointer("/content_block/type") == Some(&json!("tool_use")))
        .unwrap();
    assert!(thinking_stop < tool_start);
    assert_eq!(events.last().unwrap()["type"], "message_stop");
}

#[test]
fn complete_and_json_fallback_preserve_reasoning_in_both_directions() {
    let body = json!({"id":"c1","model":"deepseek","choices":[{"message":{"content":null,"reasoning_content":"Inspect the file.","tool_calls":[{"id":"t1","function":{"name":"Read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]});
    let bytes = serde_json::to_vec(&body).unwrap();
    let m: Value = serde_json::from_slice(
        &translator(P::Anthropic, P::ChatCompletions)
            .translate_complete(&bytes)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(m["content"][0]["thinking"], "Inspect the file.");
    let frames = translator(P::Anthropic, P::ChatCompletions).stream_from_complete(&bytes);
    assert!(payloads(&frames)
        .iter()
        .any(|v| v.pointer("/delta/thinking") == Some(&json!("Inspect the file."))));
    let reply: Value = serde_json::from_slice(
        &translator(P::ChatCompletions, P::Anthropic)
            .translate_complete(&serde_json::to_vec(&m).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        reply["choices"][0]["message"]["reasoning_content"],
        "Inspect the file."
    );
}

#[test]
fn json_error_fallback_retains_the_actual_upstream_message() {
    let mut t = translator(P::Anthropic, P::ChatCompletions);
    let frames =
        t.stream_from_complete(br#"{"error":{"message":"reasoning_content must be passed back"}}"#);
    assert_eq!(
        payloads(&frames)[0]["error"]["message"],
        "reasoning_content must be passed back"
    );
}

#[test]
fn responses_developer_instructions_use_the_compatible_chat_system_role() {
    let r = request(
        P::Codex,
        P::ChatCompletions,
        json!({"model":"deepseek","input":[{"role":"developer","content":"Follow these instructions."},{"role":"user","content":"hello"}]}),
    );
    assert_eq!(r["messages"][0]["role"], "system");
    assert_eq!(r["messages"][0]["content"], "Follow these instructions.");
}

#[test]
fn codex_reasoning_items_replay_with_the_assistant_tool_calls() {
    let reply = json!({"id":"c1","model":"deepseek","choices":[{"message":{"reasoning_content":"Need to read the file.","content":"Checking.","tool_calls":[{"id":"t1","function":{"name":"exec_command","arguments":"{\"cmd\":\"cat probe.txt\"}"}}]},"finish_reason":"tool_calls"}]});
    let mut t = translator(P::Codex, P::ChatCompletions);
    let bytes = serde_json::to_vec(&reply).unwrap();
    let result: Value = serde_json::from_slice(&t.translate_complete(&bytes).unwrap()).unwrap();
    assert_eq!(result["output"][0]["type"], "reasoning");
    let mut input = result["output"].as_array().unwrap().clone();
    input.push(json!({"type":"function_call_output","call_id":"t1","output":"PROXY_OK"}));
    let next = request(
        P::Codex,
        P::ChatCompletions,
        json!({"model":"deepseek","input":input}),
    );
    let assistant = &next["messages"][0];
    assert_eq!(assistant["reasoning_content"], "Need to read the file.");
    assert_eq!(assistant["tool_calls"][0]["id"], "t1");
    assert_eq!(next["messages"][1]["role"], "tool");
    let frames = translator(P::Codex, P::ChatCompletions).stream_from_complete(&bytes);
    let events = payloads(&frames);
    assert!(events
        .iter()
        .any(|v| v["type"] == "response.output_item.done" && v["item"]["type"] == "reasoning"));
    assert_eq!(events.last().unwrap()["type"], "response.completed");
}
