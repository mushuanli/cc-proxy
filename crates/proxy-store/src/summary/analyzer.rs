use std::collections::HashMap;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::Task;

// ── Public data structures ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummaryV1 {
    #[serde(default = "summary_version")]
    pub version: u32,
    #[serde(default)]
    pub task_id: String,
    pub session_id: String,
    #[serde(default)]
    pub status: String,
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub upstream: Option<String>,
    #[serde(default)]
    pub priced: bool,
    pub started_at: String,
    pub status_code: Option<u16>,
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cost_microusd: i64,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub ttft_ms: Option<i64>,
    #[serde(default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    pub user_prompts: Vec<UserPrompt>,
    pub assistant_actions: Vec<AssistantAction>,
    pub touched_files: Vec<FileTouched>,
    pub final_response: String,
    pub stats: SessionStats,
}

pub type SessionSummary = TaskSummaryV1;

fn summary_version() -> u32 {
    1
}

/// Extract a compact operation preview from a persisted task summary.
pub fn summary_current_operation(summary_json: &str) -> Option<String> {
    let summary = serde_json::from_str::<TaskSummaryV1>(summary_json).ok()?;
    let prompt_index = summary.user_prompts.last().map(|prompt| prompt.msg_index);
    summary
        .assistant_actions
        .iter()
        .rev()
        .find(|action| prompt_index.is_none_or(|index| action.msg_index > index))
        .and_then(action_preview)
        .or_else(|| compact_preview(&summary.final_response))
}

/// Extract the list preview once when a summary is written or migrated.
pub fn persisted_operation_preview(summary_json: Option<&str>) -> Option<String> {
    summary_json.and_then(summary_current_operation)
}

/// Refresh recording-time summary fields after a task reaches a terminal state.
pub fn refresh_task_summary(task: &Task) -> Option<String> {
    let mut summary = task
        .summary_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<TaskSummaryV1>(json).ok())
        .or_else(|| analyze_task(task))?;
    apply_final_metadata(&mut summary, task);
    apply_final_response(&mut summary, task.response_body.as_ref());
    serde_json::to_string(&summary).ok()
}

fn apply_final_metadata(summary: &mut TaskSummaryV1, task: &Task) {
    summary.status = task.status.as_str().to_string();
    summary.status_code = task.http_status_code;
    summary.stop_reason.clone_from(&task.stop_reason);
    summary.input_tokens = task.input_tokens;
    summary.output_tokens = task.output_tokens;
    summary.cache_read_tokens = task.cache_read_tokens;
    summary.cache_creation_tokens = task.cache_creation_tokens;
    summary.cost_microusd = task.cost_microusd;
    summary.duration_ms = task.duration_ms;
    summary.ttft_ms = task.ttft_ms;
    summary.error_type = task.error_type.clone().filter(|value| !value.is_empty());
    summary.error_message = task.error_message.clone().filter(|value| !value.is_empty());
}

fn apply_final_response(
    summary: &mut TaskSummaryV1,
    response: Option<&proxy_common::NormalizedResponse>,
) {
    let Some(response) = response else { return };
    let text = response.text.join("");
    if !text.is_empty() {
        summary.final_response = truncate(&text, 3000);
    }
    append_response_tools(summary, response);
}

fn append_response_tools(summary: &mut TaskSummaryV1, response: &proxy_common::NormalizedResponse) {
    if response.tool_calls.is_empty() {
        return;
    }
    let prompt_index = summary.user_prompts.last().map(|prompt| prompt.msg_index);
    let already_present = summary
        .assistant_actions
        .iter()
        .any(|action| prompt_index.is_none_or(|index| action.msg_index > index));
    if already_present {
        return;
    }
    let tools = summarize_response_tools(response, &mut summary.stats);
    let msg_index = prompt_index.map_or(0, |index| index + 1);
    summary.assistant_actions.push(AssistantAction {
        msg_index,
        thought: None,
        tools,
    });
}

fn summarize_response_tools(
    response: &proxy_common::NormalizedResponse,
    stats: &mut SessionStats,
) -> Vec<ToolCallSummary> {
    response
        .tool_calls
        .iter()
        .map(|tool| {
            stats.tool_call_count += 1;
            *stats
                .tool_call_by_name
                .entry(tool.name.clone())
                .or_insert(0) += 1;
            ToolCallSummary {
                name: tool.name.clone(),
                description: describe_tool(&tool.name, &tool.input),
            }
        })
        .collect()
}

fn action_preview(action: &AssistantAction) -> Option<String> {
    if let Some(tool) = action.tools.last() {
        let description = tool.description.trim();
        let label = if description.is_empty() {
            tool.name.trim().to_string()
        } else if description
            .to_lowercase()
            .starts_with(&tool.name.trim().to_lowercase())
        {
            description.to_string()
        } else {
            format!("{} {description}", tool.name.trim())
        };
        return compact_preview(&label);
    }
    action.thought.as_deref().and_then(compact_preview)
}

fn compact_preview(value: &str) -> Option<String> {
    const MAX_CHARS: usize = 160;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(MAX_CHARS).collect::<String>();
    Some(if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    })
}

/// Extract the latest real user prompt from a request payload.
///
/// This lightweight path is used while a response is still streaming, so the
/// UI can display the prompt without retaining response chunks.
pub fn extract_latest_user_prompt(body: &Value) -> Option<String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| body.get("input").and_then(Value::as_array))?;

    messages.iter().rev().find_map(|message| {
        let is_user = message.get("role").and_then(Value::as_str) == Some("user");
        if !is_user || !is_real_user_prompt(message) {
            return None;
        }
        let prompt = extract_user_text(message);
        (!prompt.trim().is_empty()).then(|| prompt_preview(&prompt))
    })
}

fn prompt_preview(prompt: &str) -> String {
    const MAX_CHARS: usize = 1_000;
    let mut chars = prompt.trim().chars();
    let preview = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPrompt {
    pub msg_index: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantAction {
    pub msg_index: usize,
    pub thought: Option<String>,
    pub tools: Vec<ToolCallSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTouched {
    pub path: String,
    pub reads: usize,
    pub writes: usize,
    pub edits: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    pub total_messages: usize,
    pub user_prompt_count: usize,
    pub tool_result_count: usize,
    pub tool_call_count: usize,
    pub tool_call_by_name: HashMap<String, usize>,
    pub thinking_block_count: usize,
}

// ── Entry point ──

/// Analyze a task and extract a human-readable summary.
/// Supports Anthropic `messages` and Codex `input` array formats.
pub fn analyze_task(task: &Task) -> Option<SessionSummary> {
    analyze_body(
        task.request_body.as_deref()?,
        task.response_body.as_ref(),
        task.id.as_str(),
        task.session_id.as_str(),
        task.status.as_str(),
        &task.resolved_model,
        &task.provider,
        task.upstream.as_deref(),
        task.pricing_model_id
            .as_deref()
            .is_some_and(|id| id != "unknown"),
        task.started_at,
        task.http_status_code,
        task.stop_reason.as_deref(),
        task.input_tokens,
        task.output_tokens,
        task.cache_read_tokens,
        task.cache_creation_tokens,
        task.cost_microusd,
        task.duration_ms,
        task.ttft_ms,
        task.error_type.as_deref().filter(|v| !v.is_empty()),
        task.error_message.as_deref().filter(|v| !v.is_empty()),
    )
}

/// Same as analyze_task but takes individual fields instead of a full Task.
/// Used by relay.rs to summarize inline before writing to the store.
#[allow(clippy::too_many_arguments)]
pub fn summarize_task(
    request_body: &str,
    response_body: Option<&proxy_common::NormalizedResponse>,
    task_id: &str,
    session_id: &str,
    status: &str,
    model: &str,
    provider: &str,
    upstream: Option<&str>,
    priced: bool,
    started_at_ms: i64,
    status_code: Option<u16>,
    stop_reason: Option<&str>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_microusd: i64,
    duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    error_type: Option<&str>,
    error_message: Option<&str>,
) -> Option<String> {
    analyze_body(
        request_body,
        response_body,
        task_id,
        session_id,
        status,
        model,
        provider,
        upstream,
        priced,
        started_at_ms,
        status_code,
        stop_reason,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        cost_microusd,
        duration_ms,
        ttft_ms,
        error_type,
        error_message,
    )
    .and_then(|s| serde_json::to_string(&s).ok())
}

fn analyze_body(
    request_body: &str,
    response_body: Option<&proxy_common::NormalizedResponse>,
    task_id: &str,
    session_id: &str,
    status: &str,
    model: &str,
    provider: &str,
    upstream: Option<&str>,
    priced: bool,
    started_at_ms: i64,
    status_code: Option<u16>,
    stop_reason: Option<&str>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    cost_microusd: i64,
    duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    error_type: Option<&str>,
    error_message: Option<&str>,
) -> Option<SessionSummary> {
    let body: Value = serde_json::from_str(request_body).ok()?;

    // Try Anthropic messages format first, then Codex input array
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .or_else(|| body.get("input").and_then(|v| v.as_array()))?;

    let mut user_prompts: Vec<UserPrompt> = Vec::new();
    let mut assistant_actions: Vec<AssistantAction> = Vec::new();
    // Preserve insertion order by using a Vec of (path, FileTouched)
    let mut file_order: Vec<String> = Vec::new();
    let mut file_map: HashMap<String, FileTouched> = HashMap::new();
    let mut stats = SessionStats {
        total_messages: messages.len(),
        user_prompt_count: 0,
        tool_result_count: 0,
        tool_call_count: 0,
        tool_call_by_name: HashMap::new(),
        thinking_block_count: 0,
    };
    let failed_tool_ids: HashSet<&str> = messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("is_error").and_then(Value::as_bool) == Some(true)
        })
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .collect();

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                if is_tool_result(msg) {
                    stats.tool_result_count += 1;
                } else if is_real_user_prompt(msg) {
                    let text = extract_user_text(msg);
                    if !text.trim().is_empty() {
                        stats.user_prompt_count += 1;
                        user_prompts.push(UserPrompt { msg_index: i, text });
                    }
                }
            }
            "assistant" => {
                let content = msg.get("content").and_then(|c| c.as_array());
                if let Some(blocks) = content {
                    let tool_uses = get_tool_uses(blocks);
                    let thought = extract_thought(blocks, 200);

                    // Count thinking blocks
                    for block in blocks {
                        if block.get("type").and_then(|t| t.as_str()) == Some("thinking") {
                            stats.thinking_block_count += 1;
                        }
                    }

                    if !tool_uses.is_empty() {
                        let mut tool_summaries: Vec<ToolCallSummary> = Vec::new();
                        for tool in &tool_uses {
                            let name = tool
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let input = tool.get("input").cloned().unwrap_or(Value::Null);

                            // Update stats
                            *stats.tool_call_by_name.entry(name.clone()).or_insert(0) += 1;
                            stats.tool_call_count += 1;

                            // Collect file touches
                            let tool_id = tool.get("id").and_then(Value::as_str);
                            if !tool_id.is_some_and(|id| failed_tool_ids.contains(id)) {
                                collect_file_touch(&name, &input, &mut file_order, &mut file_map);
                            }

                            let description = describe_tool(&name, &input);
                            tool_summaries.push(ToolCallSummary { name, description });
                        }
                        assistant_actions.push(AssistantAction {
                            msg_index: i,
                            thought,
                            tools: tool_summaries,
                        });
                    } else if thought.is_some() {
                        // Pure text response — record it with no tools
                        assistant_actions.push(AssistantAction {
                            msg_index: i,
                            thought,
                            tools: vec![],
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // Build touched_files in insertion order
    let touched_files: Vec<FileTouched> = file_order
        .iter()
        .filter_map(|p| file_map.get(p).cloned())
        .collect();

    // Final response: prefer the actual response body from this task,
    // fall back to the last assistant text in the request messages
    let final_response = response_body
        .map(|resp| resp.text.join(""))
        .filter(|t| !t.is_empty())
        .map(|t| truncate(&t, 3000))
        .unwrap_or_else(|| extract_last_assistant_text(messages));

    Some(SessionSummary {
        version: 1,
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        status: status.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
        upstream: upstream.map(|s| s.to_string()),
        priced,
        started_at: chrono::DateTime::from_timestamp(started_at_ms / 1000, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        status_code: status_code,
        stop_reason: stop_reason.map(|s| s.to_string()),
        input_tokens: input_tokens,
        output_tokens: output_tokens,
        cache_read_tokens: cache_read_tokens,
        cache_creation_tokens: cache_creation_tokens,
        cost_microusd: cost_microusd,
        duration_ms: duration_ms,
        ttft_ms: ttft_ms,
        error_type: error_type.map(|s| s.to_string()),
        error_message: error_message.map(|s| s.to_string()),
        user_prompts,
        assistant_actions,
        touched_files,
        final_response,
        stats,
    })
}

// ── Message classification ──

fn is_tool_result(msg: &Value) -> bool {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        })
        .unwrap_or(false)
}

pub fn is_real_user_prompt(msg: &Value) -> bool {
    let content = match msg.get("content").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => {
            // content may be a plain string in some edge cases
            let s = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            return !s.trim_start().starts_with("<system-reminder>") && !s.is_empty();
        }
    };

    // Must be all text blocks
    let all_text = content.iter().all(|b| {
        matches!(
            b.get("type").and_then(|t| t.as_str()),
            Some("text" | "input_text")
        )
    });
    if !all_text {
        return false;
    }

    // None of the blocks should be tool_result
    let has_tool_result = content
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"));
    if has_tool_result {
        return false;
    }

    // Filter out system-reminder blocks, then check if any real text remains
    let text = content
        .iter()
        .filter_map(block_text)
        .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
        .collect::<Vec<_>>()
        .join("\n");

    !text.trim().is_empty()
}

pub fn extract_user_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                if matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("text" | "input_text")
                ) {
                    block_text(b)
                } else {
                    None
                }
            })
            .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => {
            if s.trim_start().starts_with("<system-reminder>") {
                String::new()
            } else {
                s.clone()
            }
        }
        _ => String::new(),
    }
}

fn get_tool_uses(blocks: &[Value]) -> Vec<&Value> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .collect()
}

fn block_text(block: &Value) -> Option<&str> {
    block
        .get("text")
        .or_else(|| block.get("input_text"))
        .or_else(|| block.get("output_text"))
        .and_then(|value| value.as_str())
}

fn extract_thought(blocks: &[Value], max_len: usize) -> Option<String> {
    // First try text blocks before any tool_use
    let mut texts = Vec::new();
    for block in blocks {
        let t = block.get("type").and_then(|t| t.as_str());
        match t {
            Some("text" | "output_text") => {
                if let Some(s) = block_text(block) {
                    if !s.trim().is_empty() {
                        texts.push(s);
                    }
                }
            }
            Some("tool_use") => break,
            _ => {}
        }
    }
    if texts.is_empty() {
        return None;
    }
    let joined = texts.join(" ");
    Some(truncate(&joined, max_len))
}

fn extract_last_assistant_text(messages: &[Value]) -> String {
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                let text: String = blocks
                    .iter()
                    .filter(|b| {
                        matches!(
                            b.get("type").and_then(|t| t.as_str()),
                            Some("text" | "output_text")
                        )
                    })
                    .filter_map(block_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    return truncate(&text, 3000);
                }
            }
        }
    }
    String::new()
}

// ── File touch tracking ──

fn collect_file_touch(
    tool_name: &str,
    input: &Value,
    order: &mut Vec<String>,
    map: &mut HashMap<String, FileTouched>,
) {
    let path = match tool_name {
        "Read" | "Write" | "Edit" | "NotebookEdit" => input
            .get("file_path")
            .and_then(|p| p.as_str())
            .map(String::from),
        "Grep" => input.get("path").and_then(|p| p.as_str()).map(String::from),
        "Glob" => input.get("path").and_then(|p| p.as_str()).map(String::from),
        _ => None,
    };

    let path = match path {
        Some(p) if !p.is_empty() => p,
        _ => return,
    };

    if !map.contains_key(&path) {
        order.push(path.clone());
        map.insert(
            path.clone(),
            FileTouched {
                path: path.clone(),
                reads: 0,
                writes: 0,
                edits: 0,
            },
        );
    }

    let entry = map.get_mut(&path).unwrap();
    match tool_name {
        "Read" => entry.reads += 1,
        "Write" | "NotebookEdit" => entry.writes += 1,
        "Edit" => entry.edits += 1,
        "Grep" | "Glob" => entry.reads += 1,
        _ => {}
    }
}

// ── Tool description generation ──

fn describe_tool(name: &str, input: &Value) -> String {
    match name {
        "Read" => {
            let path = str_field(input, "file_path");
            let offset = input.get("offset").and_then(|v| v.as_u64());
            let limit = input.get("limit").and_then(|v| v.as_u64());
            match (offset, limit) {
                (Some(off), Some(lim)) => format!("Read {}:{}-{}", path, off, off + lim),
                (Some(off), None) => format!("Read {}:{}", path, off),
                _ => format!("Read {}", path),
            }
        }
        "Write" => format!("Write {}", str_field(input, "file_path")),
        "Edit" => {
            let path = str_field(input, "file_path");
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("Edit {} — \"{}\"", path, truncate(old, 50))
        }
        "NotebookEdit" => format!("Edit notebook {}", str_field(input, "notebook_path")),
        "Grep" => {
            let pattern = str_field(input, "pattern");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("Search \"{}\" in {}", truncate(pattern, 40), path)
        }
        "Glob" => {
            let pattern = str_field(input, "pattern");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("Find files \"{}\" in {}", truncate(pattern, 40), path)
        }
        "Bash" => {
            let desc = input.get("description").and_then(|v| v.as_str());
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let text = desc.unwrap_or(cmd);
            format!("Run: {}", truncate(text, 80))
        }
        "Agent" => {
            let agent_type = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("agent");
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("Spawn {}: {}", agent_type, truncate(desc, 60))
        }
        "TaskCreate" => format!("Create task: {}", truncate(str_field(input, "subject"), 60)),
        "TaskUpdate" => {
            let id = str_field(input, "taskId");
            let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Update task {} → {}", id, status)
        }
        "TaskGet" => format!("Get task {}", str_field(input, "taskId")),
        "TaskList" => "List tasks".to_string(),
        "TaskStop" => format!("Stop task {}", str_field(input, "task_id")),
        "AskUserQuestion" => {
            let q = input
                .get("questions")
                .and_then(|qs| qs.as_array())
                .and_then(|arr| arr.first())
                .and_then(|q| q.get("question"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("Ask: {}", truncate(q, 80))
        }
        "WebFetch" => format!("Fetch: {}", str_field(input, "url")),
        "WebSearch" => format!("Search web: \"{}\"", str_field(input, "query")),
        "Skill" => {
            let skill = str_field(input, "skill");
            let args = input.get("args").and_then(|v| v.as_str()).unwrap_or("");
            if args.is_empty() {
                format!("Skill /{}", skill)
            } else {
                format!("Skill /{} {}", skill, truncate(args, 40))
            }
        }
        "CronCreate" => {
            let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            format!("Schedule: {}", truncate(prompt, 50))
        }
        "CronDelete" => format!("Delete cron {}", str_field(input, "id")),
        "CronList" => "List cron jobs".to_string(),
        "EnterPlanMode" => "Enter plan mode".to_string(),
        "ExitPlanMode" => "Submit plan for approval".to_string(),
        "EnterWorktree" => "Enter worktree".to_string(),
        "ExitWorktree" => "Exit worktree".to_string(),
        "SendMessage" => format!("Message agent: {}", str_field(input, "to")),
        _ => format!("{}: {}", name, truncate(&input.to_string(), 60)),
    }
}

// ── Utilities ──

fn str_field<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn truncate(s: &str, max_len: usize) -> String {
    let s = s.trim();
    // Operate on char boundaries to avoid panic
    let mut byte_pos = 0;
    for (count, ch) in s.chars().enumerate() {
        if count >= max_len {
            return format!("{}…", &s[..byte_pos]);
        }
        byte_pos += ch.len_utf8();
    }
    s.to_string()
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Task;
    use proxy_common::{SessionId, TaskId, TaskStatus};

    fn make_task(body: Option<&str>) -> Task {
        Task {
            id: TaskId::new("01TEST".into()),
            session_id: SessionId::from_trusted("01TEST".into()),
            sequence_no: 1,
            created_at: 0,
            started_at: 0,
            first_byte_at: None,
            ended_at: None,
            status: TaskStatus::Completed,
            method: "POST".into(),
            path: "/v1/messages".into(),
            request_headers: None,
            request_body: body.map(String::from),
            response_headers: None,
            response_body: None,
            http_status_code: Some(200),
            is_streaming: true,
            requested_model: Some("claude-sonnet-4-6".into()),
            provider: "anthropic".into(),
            pricing_model_id: None,
            resolved_model: "claude-sonnet-4-6".into(),
            upstream: None,
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            duration_ms: None,
            ttft_ms: None,
            stop_reason: None,
            upstream_message_id: None,
            error_type: None,
            error_message: None,
            input_rate_microusd: 0,
            output_rate_microusd: 0,
            cache_write_rate_microusd: 0,
            cache_read_rate_microusd: 0,
            cost_microusd: 0,
            currency: "USD".into(),
            summary_json: None,
            summary_created_at: None,
            prompt_text: None,
            current_operation: None,
            metadata: serde_json::Value::Null,
            messages_count: 0,
        }
    }

    #[test]
    fn analyze_returns_none_when_no_body() {
        let task = make_task(None);
        assert!(analyze_task(&task).is_none());
    }

    #[test]
    fn analyze_extracts_user_prompt() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello, world!"}]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.user_prompts.len(), 1);
        assert_eq!(summary.user_prompts[0].text, "Hello, world!");
    }

    #[test]
    fn extracts_latest_prompt_before_tool_results() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "commit"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "done"}]}
            ]
        });

        assert_eq!(extract_latest_user_prompt(&body).as_deref(), Some("commit"));
    }

    #[test]
    fn analyze_codex_input_and_current_response() {
        let body = serde_json::json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Fix the tests"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "old answer"}]}
            ]
        });
        let mut task = make_task(Some(&body.to_string()));
        task.response_body = Some(proxy_common::NormalizedResponse {
            text: vec!["current ".into(), "answer".into()],
            ..Default::default()
        });
        task.provider = "openai".into();
        task.upstream = Some("codex".into());
        task.cost_microusd = 123;
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.user_prompts[0].text, "Fix the tests");
        assert_eq!(summary.final_response, "current answer");
        assert_eq!(summary.provider, "openai");
        assert_eq!(summary.cost_microusd, 123);
    }

    #[test]
    fn analyze_ignores_system_reminder() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>some context</system-reminder>"}]},
                {"role": "user", "content": [{"type": "text", "text": "Real prompt"}]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.user_prompts.len(), 1);
        assert_eq!(summary.user_prompts[0].text, "Real prompt");
    }

    #[test]
    fn analyze_ignores_tool_results() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "file content"}]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.user_prompts.len(), 0);
        assert_eq!(summary.stats.tool_result_count, 1);
    }

    #[test]
    fn failed_write_is_not_reported_as_a_touched_file() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "write-1", "name": "Write", "input": {"file_path": "/tmp/nope"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "write-1", "is_error": true, "content": "permission denied"}
                ]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert!(summary.touched_files.is_empty());
        assert_eq!(summary.stats.tool_call_count, 1);
    }

    #[test]
    fn analyze_captures_pure_text_assistant_response() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Explain the bug"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "The bug is caused by a filename collision."}
                ]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.assistant_actions.len(), 1);
        assert!(summary.assistant_actions[0].tools.is_empty());
        assert_eq!(
            summary.assistant_actions[0].thought.as_deref(),
            Some("The bug is caused by a filename collision.")
        );
    }

    #[test]
    fn analyze_extracts_tool_calls() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read this file"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me read that."},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/foo/bar.ts"}}
                ]}
            ]
        });
        let task = make_task(Some(&body.to_string()));
        let summary = analyze_task(&task).unwrap();
        assert_eq!(summary.assistant_actions.len(), 1);
        assert_eq!(summary.assistant_actions[0].tools[0].name, "Read");
        assert_eq!(summary.touched_files.len(), 1);
        assert_eq!(summary.touched_files[0].path, "/foo/bar.ts");
        assert_eq!(summary.touched_files[0].reads, 1);
    }

    #[test]
    fn current_operation_uses_latest_action_after_parent_prompt() {
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": "Fix the inspector"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "inspector.js"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "file content"}
                ]}
            ]
        });
        let summary = analyze_task(&make_task(Some(&body.to_string()))).unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            summary_current_operation(&json).as_deref(),
            Some("Read inspector.js")
        );
    }

    #[test]
    fn current_operation_ignores_actions_before_latest_prompt() {
        let body = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "old.js"}}
                ]},
                {"role": "user", "content": "Start a new task"}
            ]
        });
        let summary = analyze_task(&make_task(Some(&body.to_string()))).unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(summary_current_operation(&json), None);
    }

    #[test]
    fn current_operation_falls_back_to_final_response() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "Explain the result"}]
        });
        let mut summary = analyze_task(&make_task(Some(&body.to_string()))).unwrap();
        summary.final_response = "The operation completed successfully.".into();
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            summary_current_operation(&json).as_deref(),
            Some("The operation completed successfully.")
        );
    }

    #[test]
    fn refresh_summary_adds_terminal_response_operation() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "Read the config"}]
        });
        let mut task = make_task(Some(&body.to_string()));
        task.summary_json =
            analyze_task(&task).and_then(|summary| serde_json::to_string(&summary).ok());
        task.status = TaskStatus::Completed;
        task.response_body = Some(proxy_common::NormalizedResponse {
            tool_calls: vec![proxy_common::ToolCallRecord {
                id: "t1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "config.toml"}),
            }],
            ..Default::default()
        });

        let json = refresh_task_summary(&task).unwrap();
        assert_eq!(
            summary_current_operation(&json).as_deref(),
            Some("Read config.toml")
        );
    }

    #[test]
    fn describe_tool_read() {
        let input = serde_json::json!({"file_path": "/src/main.rs", "offset": 10, "limit": 50});
        assert_eq!(describe_tool("Read", &input), "Read /src/main.rs:10-60");
    }

    #[test]
    fn describe_tool_edit() {
        let input = serde_json::json!({"file_path": "/src/lib.rs", "old_string": "fn foo() {}"});
        assert!(describe_tool("Edit", &input).contains("Edit /src/lib.rs"));
    }

    #[test]
    fn truncate_handles_unicode() {
        let s = "你好世界";
        assert_eq!(truncate(s, 2), "你好…");
        assert_eq!(truncate(s, 10), "你好世界");
    }
}
