use serde::{Deserialize, Serialize};

/// A tier routing rule.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TierRule {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
}

impl TierRule {
    pub fn is_active(&self) -> bool {
        !self.provider.is_empty() || !self.model.is_empty()
    }

    pub fn matches(&self, model_lower: &str, tier_label: &str) -> bool {
        self.is_active() && model_lower.contains(tier_label)
    }

    /// Resolve the effective provider, inheriting from default when empty.
    pub fn provider_or(&self, default: Option<&TierRule>) -> String {
        if !self.provider.is_empty() {
            self.provider.clone()
        } else {
            default.and_then(|d| if !d.provider.is_empty() { Some(d.provider.clone()) } else { None })
                .unwrap_or_default()
        }
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
