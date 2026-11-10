//! Metadata and tool observations for Chat Completions streams.
use super::{
    obs_from_kind, tool_calls_to_observations, ClientParser, ParseContext, RequestFacts,
    StreamUpdate,
};
use crate::{Observation, ObservationKind};
use proxy_common::{ClientType, NormalizedResponse, SseEvent};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct ChatParser {
    tools: BTreeMap<u64, (String, String, String)>,
}
impl ChatParser {
    fn tool_updates(&mut self, delta: &Value) {
        for tool in delta["tool_calls"].as_array().into_iter().flatten() {
            let t = self
                .tools
                .entry(tool["index"].as_u64().unwrap_or(0))
                .or_default();
            if let Some(id) = tool["id"].as_str() {
                t.0.push_str(id);
            }
            if let Some(name) = tool["function"]["name"].as_str() {
                t.1.push_str(name);
            }
            if let Some(args) = tool["function"]["arguments"].as_str() {
                t.2.push_str(args);
            }
        }
    }
    fn complete_tools(&mut self, ctx: &ParseContext) -> Vec<Observation> {
        let mut out = Vec::new();
        for (_, (id, name, args)) in std::mem::take(&mut self.tools) {
            out.push(obs_from_kind(
                ObservationKind::ToolEmitted {
                    call_id: ctx.call_id.clone(),
                    tool_use_id: id.clone(),
                    tool_name: name,
                    started_at: chrono::Utc::now().timestamp_millis(),
                },
                ctx,
            ));
            out.push(obs_from_kind(
                ObservationKind::ToolInputComplete {
                    tool_use_id: id,
                    input_json: args,
                },
                ctx,
            ));
        }
        out
    }
}
impl ClientParser for ChatParser {
    fn client_type(&self) -> ClientType {
        ClientType::Codex
    }
    fn parse_request(&self, body: &Value) -> RequestFacts {
        let mut facts = super::AnthropicParser::default().parse_request(body);
        facts.is_tool_result_last = body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .is_some_and(|m| m["role"] == "tool");
        if facts.is_tool_result_last {
            facts.prompt_type = Some("continuation".into());
        }
        facts
    }
    fn parse_sse(&self, _ev: &SseEvent, _ctx: &ParseContext) -> Vec<Observation> {
        Vec::new()
    }
    fn parse_response(
        &self,
        normalized: &NormalizedResponse,
        ctx: &ParseContext,
    ) -> Vec<Observation> {
        tool_calls_to_observations(normalized, ctx)
    }
    fn feed_sse(&mut self, v: &Value, ctx: &ParseContext) -> StreamUpdate {
        let choice = &v["choices"][0];
        let delta = &choice["delta"];
        let mut update = StreamUpdate {
            text: delta["content"].as_str().map(str::to_owned),
            thinking: delta["reasoning_content"].as_str().map(str::to_owned),
            input_tokens: v["usage"]["prompt_tokens"].as_u64().map(super::u64_to_u32),
            output_tokens: v["usage"]["completion_tokens"]
                .as_u64()
                .map(super::u64_to_u32),
            cache_read_tokens: v["usage"]["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .map(super::u64_to_u32),
            stop_reason: choice["finish_reason"].as_str().map(str::to_owned),
            message_id: v["id"].as_str().map(str::to_owned),
            model: v["model"].as_str().map(str::to_owned),
            error: v["error"]["message"].as_str().map(str::to_owned),
            ..Default::default()
        };
        self.tool_updates(delta);
        if update.stop_reason.is_some() {
            update.observations = self.complete_tools(ctx);
        }
        update
    }
    fn finish_stream(&mut self, ctx: &ParseContext) -> Vec<Observation> {
        std::mem::take(&mut self.tools)
            .into_values()
            .map(|(id, _, _)| {
                obs_from_kind(
                    ObservationKind::ToolResult {
                        tool_use_id: id,
                        raw_result_preview: None,
                        effective_result_preview: None,
                        status: "abandoned".into(),
                    },
                    ctx,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn interleaved_chat_tools_and_usage_become_observations() {
        let mut p = ChatParser::default();
        let ctx = ParseContext::default();
        p.feed_sse(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"one","arguments":"{\"x\":"}},{"index":1,"id":"c2","function":{"name":"two","arguments":"{}"}}]}}]}),&ctx);
        let update=p.feed_sse(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),&ctx);
        assert_eq!(update.observations.len(), 4);
        assert!(update.observations.iter().any(|o|matches!(&o.kind,ObservationKind::ToolInputComplete{tool_use_id,input_json} if tool_use_id=="c1" && input_json=="{\"x\":1}")));
        let usage=p.feed_sse(&json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":7}}}),&ctx);
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cache_read_tokens, Some(7));
        assert!(p.finish_stream(&ctx).is_empty());
    }
    #[test]
    fn tool_response_requests_are_continuations_and_truncated_calls_are_abandoned() {
        let mut p = ChatParser::default();
        let facts=p.parse_request(&json!({"messages":[{"role":"user","content":"hi"},{"role":"tool","tool_call_id":"c1","content":"ok"}]}));
        assert_eq!(facts.prompt_text.as_deref(), Some("hi"));
        assert!(facts.is_tool_result_last);
        let ctx = ParseContext::default();
        p.feed_sse(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"one","arguments":"{"}}]}}]}),&ctx);
        assert!(
            matches!(&p.finish_stream(&ctx)[0].kind,ObservationKind::ToolResult{status,..} if status=="abandoned")
        );
    }
}

#[cfg(test)]
mod result_tests {
    #[test]
    fn chat_tool_results_close_the_matching_tool_invocation() {
        let obs = super::super::extract_tool_results(
            r#"{"messages":[{"role":"tool","tool_call_id":"call_1","content":"done"}]}"#,
            "sess-1",
        );
        assert!(
            matches!(&obs[0].kind,crate::ObservationKind::ToolResult{tool_use_id,status,raw_result_preview,..} if tool_use_id=="call_1" && status=="succeeded" && raw_result_preview.as_deref()==Some("done"))
        );
    }
}
