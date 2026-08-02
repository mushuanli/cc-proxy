//! Heuristic classifier: request body text features → run kind.
//!
//! These are weak signals (internal text patterns may change between Claude
//! Code versions), so all classifications carry `confidence=weak` and a
//! classifier version.

use crate::domain::execution_run::RunKind;

/// Classify a request's final user text into a run kind.
pub struct HeuristicClassifier;

impl HeuristicClassifier {
    pub fn classify(text: &str) -> (RunKind, &'static str) {
        let t = text.trim_start();
        if t.starts_with("<session>") && t.contains("Write the title") {
            (RunKind::Title, "title")
        } else if t.starts_with("<conversation>") {
            (RunKind::Memory, "memory")
        } else if t.starts_with("<transcript>") {
            (RunKind::Subagent, "subagent")
        } else if t.contains("The user stepped away") && t.contains("Recap") {
            (RunKind::Recap, "recap")
        } else {
            (RunKind::Main, "main")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_internal_run_kinds() {
        assert_eq!(HeuristicClassifier::classify("<session>\nFix\n</session>\n\nWrite the title").0, RunKind::Title);
        assert_eq!(HeuristicClassifier::classify("<conversation>\n# Recap").0, RunKind::Memory);
        assert_eq!(HeuristicClassifier::classify("<transcript>\nUser: hello").0, RunKind::Subagent);
        assert_eq!(HeuristicClassifier::classify("The user stepped away and is coming back. Recap in under 40 words").0, RunKind::Recap);
        assert_eq!(HeuristicClassifier::classify("normal prompt").0, RunKind::Main);
    }
}
