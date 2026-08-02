//! Timeline queries: assemble the session task timeline document.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::domain::execution_run::RunKind;
use crate::domain::model_call::ModelCallRow;
use crate::domain::tool_invocation::ToolInvocationRow;
use crate::persist::repo::SessionRepo;
use crate::SessionResult;

/// Serialization-friendly timeline document for `GET /api/session/:id/timeline`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineDocument {
    pub session_id: String,
    pub total_model_calls: usize,
    pub user_interactions: usize,
    pub interactions: Vec<InteractionNode>,
}

/// A user interaction with its execution runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionNode {
    pub id: String,
    pub prompt_text: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub runs: Vec<ExecutionRunNode>,
}

/// An execution run (main/subagent/title/memory/recap) with its model calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRunNode {
    pub id: String,
    pub run_kind: String,
    pub started_at: i64,
    pub status: String,
    pub tool_call_count: usize,
    pub model_calls: Vec<ModelCallNode>,
}

/// A model call with its tool invocations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallNode {
    pub id: String,
    pub sequence_no: i64,
    pub previous_model_call_id: Option<String>,
    pub status: String,
    pub started_at: i64,
    pub resolved_model: String,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_microusd: i64,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub stop_reason: Option<String>,
    pub prompt: Option<String>,
    pub operations: Vec<ToolNode>,
}

/// A tool invocation within a model call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolNode {
    pub tool_use_id: Option<String>,
    pub operation_seq: i64,
    pub tool_name: String,
    pub status: String,
    pub input_preview: Option<String>,
    pub result_preview: Option<String>,
}

/// Reads and assembles a session timeline from the repo.
pub struct TimelineReader {
    repo: Arc<SessionRepo>,
}

impl TimelineReader {
    pub fn new(repo: Arc<SessionRepo>) -> Self {
        Self { repo }
    }

    /// Materialize observations, reconcile groupings, then build the timeline.
    ///
    /// This is the idempotent read path: safe to call on every request.
    pub fn load(&self, session_id: &str) -> SessionResult<TimelineDocument> {
        self.repo.materialize(session_id)?;
        let reconciler = crate::pipeline::reconciler::Reconciler::new(
            self.repo.clone(),
            crate::pipeline::reconciler::ReconcilerConfig::default(),
        );
        reconciler.reconcile(session_id)?;
        self.build(session_id)
    }

    /// Build the full timeline for a session.
    ///
    /// Callers should have materialized + reconciled first; this is a pure read.
    pub fn build(&self, session_id: &str) -> SessionResult<TimelineDocument> {        let calls = self.repo.list_model_calls(session_id, None, 10_000)?;
        let interactions = self.repo.list_interactions(session_id)?;
        let runs = self.repo.list_execution_runs(session_id)?;

        // Group runs by interaction id.
        let mut runs_by_interaction: HashMap<Option<String>, Vec<_>> = HashMap::new();
        for run in runs {
            runs_by_interaction
                .entry(run.interaction_id.clone())
                .or_default()
                .push(run);
        }

        // Group calls by execution run id.
        let mut calls_by_run: HashMap<String, Vec<ModelCallRow>> = HashMap::new();
        for call in &calls {
            if let Some(run_id) = &call.execution_run_id {
                calls_by_run.entry(run_id.clone()).or_default().push(call.clone());
            }
        }

        let mut interaction_nodes = Vec::new();
        for interaction in &interactions {
            let mut runs_nodes = Vec::new();
            if let Some(runs) = runs_by_interaction.get(&Some(interaction.id.clone())) {
                for run in runs {
                    let calls_for_run = calls_by_run.remove(&run.id).unwrap_or_default();
                    let mut call_nodes = Vec::new();
                    let mut tool_calls = 0usize;
                    for call in &calls_for_run {
                        let tools = self.repo.list_tool_invocations(&call.id)?;
                        tool_calls += tools.len();
                        call_nodes.push(call_to_node(call, tools));
                    }
                    runs_nodes.push(ExecutionRunNode {
                        id: run.id.clone(),
                        run_kind: run.run_kind.as_str().to_string(),
                        started_at: run.started_at,
                        status: run.status.clone(),
                        tool_call_count: tool_calls,
                        model_calls: call_nodes,
                    });
                }
            }
            interaction_nodes.push(InteractionNode {
                id: interaction.id.clone(),
                prompt_text: interaction.prompt_text.clone(),
                started_at: interaction.started_at,
                ended_at: interaction.ended_at,
                status: interaction.status.clone(),
                runs: runs_nodes,
            });
        }

        // Any runs not attached to an interaction (internal runs) get a top-level node.
        let mut orphan_runs = Vec::new();
        for run in runs_by_interaction.remove(&None).unwrap_or_default() {
            let calls_for_run = calls_by_run.remove(&run.id).unwrap_or_default();
            let mut call_nodes = Vec::new();
            let mut tool_calls = 0usize;
            for call in &calls_for_run {
                let tools = self.repo.list_tool_invocations(&call.id)?;
                tool_calls += tools.len();
                call_nodes.push(call_to_node(call, tools));
            }
            orphan_runs.push(ExecutionRunNode {
                id: run.id.clone(),
                run_kind: run.run_kind.as_str().to_string(),
                started_at: run.started_at,
                status: run.status.clone(),
                tool_call_count: tool_calls,
                model_calls: call_nodes,
            });
        }
        if !orphan_runs.is_empty() {
            interaction_nodes.push(InteractionNode {
                id: format!("inter-{session_id}-internal"),
                prompt_text: None,
                started_at: orphan_runs
                    .iter()
                    .map(|r| r.started_at)
                    .min()
                    .unwrap_or(0),
                ended_at: None,
                status: "completed".into(),
                runs: orphan_runs,
            });
        }

        let total = calls.len();
        let user_interactions = interaction_nodes
            .iter()
            .filter(|n| n.runs.iter().any(|r| r.run_kind == RunKind::Main.as_str()))
            .count();

        Ok(TimelineDocument {
            session_id: session_id.to_string(),
            total_model_calls: total,
            user_interactions,
            interactions: interaction_nodes,
        })
    }
}

fn call_to_node(call: &ModelCallRow, tools: Vec<ToolInvocationRow>) -> ModelCallNode {
    ModelCallNode {
        id: call.id.clone(),
        sequence_no: call.sequence_no,
        previous_model_call_id: call.previous_model_call_id.clone(),
        status: call.status.as_str().to_string(),
        started_at: call.started_at,
        resolved_model: call.resolved_model.clone(),
        provider: call.provider.clone(),
        input_tokens: call.input_tokens,
        output_tokens: call.output_tokens,
        cost_microusd: call.cost_microusd,
        duration_ms: call.duration_ms,
        ttft_ms: call.ttft_ms,
        stop_reason: call.stop_reason.clone(),
        prompt: None,
        operations: tools
            .into_iter()
            .map(|t| ToolNode {
                tool_use_id: t.tool_use_id,
                operation_seq: t.operation_seq,
                tool_name: t.tool_name,
                status: t.status.as_str().to_string(),
                input_preview: t.effective_input_preview.or(t.model_input_preview),
                result_preview: t.effective_result_preview.or(t.raw_result_preview),
            })
            .collect(),
    }
}

