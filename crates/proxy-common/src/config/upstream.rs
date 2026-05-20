use serde::{Deserialize, Serialize};

/// A tier routing rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TierRule {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
}

impl TierRule {
    pub fn is_active(&self) -> bool {
        !self.provider.is_empty()
            && self.keywords.iter().any(|kw| !kw.is_empty())
    }

    pub fn matches(&self, model_lower: &str) -> bool {
        self.is_active()
            && self
                .keywords
                .iter()
                .any(|kw| !kw.is_empty() && model_lower.contains(kw.to_lowercase().as_str()))
    }
}

/// A named upstream configuration with tiered routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<TierRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid: Option<TierRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<TierRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<TierRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
