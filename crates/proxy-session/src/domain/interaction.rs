//! Interaction: one real user input (source of a prompt.id).

use serde::{Deserialize, Serialize};

/// Row projection of a user interaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionRow {
    pub id: String,
    pub session_id: String,
    pub external_prompt_id: Option<String>,
    pub prompt_text: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub classification_source: String,
    pub classification_confidence: String,
    pub classifier_version: String,
}
