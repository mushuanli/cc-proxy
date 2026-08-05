use serde::{Deserialize, Serialize};

/// A cloud provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Optional per-provider proxy URL.
    /// `None` = inherit global http_proxy or direct,
    /// `Some("")` = force direct connection (bypass global),
    /// `Some(url)` = use this proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Protocols this provider serves (`anthropic` / `codex`).
    /// Empty/missing = serves all protocols (backward compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub protocols: Vec<String>,
    /// Codex-specific endpoint. When set, codex requests use this URL
    /// instead of `url` (e.g. `https://api.deepseek.com/v1`).
    /// Empty = use `url` for codex too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_url: Option<String>,
}

impl Provider {
    /// True if this provider serves the given protocol.
    /// Empty protocols = serves all.
    pub fn serves(&self, protocol: &str) -> bool {
        self.protocols.is_empty() || self.protocols.iter().any(|p| p == protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_protocols_serves_all() {
        let p = Provider {
            name: "p".into(),
            url: "https://x".into(),
            codex_url: None,
            token: None,
            proxy: None,
            protocols: vec![],
        };
        assert!(p.serves("anthropic"));
        assert!(p.serves("codex"));
    }

    #[test]
    fn single_protocol_restricts() {
        let p = Provider {
            name: "p".into(),
            url: "https://x".into(),
            codex_url: None,
            token: None,
            proxy: None,
            protocols: vec!["anthropic".into()],
        };
        assert!(p.serves("anthropic"));
        assert!(!p.serves("codex"));
    }

    #[test]
    fn multi_protocol_serves_both() {
        let p = Provider {
            name: "p".into(),
            url: "https://x".into(),
            codex_url: None,
            token: None,
            proxy: None,
            protocols: vec!["anthropic".into(), "codex".into()],
        };
        assert!(p.serves("anthropic"));
        assert!(p.serves("codex"));
        assert!(!p.serves("other"));
    }

    #[test]
    fn legacy_config_without_protocols_deserializes_empty() {
        let json = r#"
        {"name": "p", "url": "https://x", "token": "tok"}
        "#;
        let p: Provider = serde_json::from_str(json).unwrap();
        assert!(p.protocols.is_empty());
        assert!(p.serves("anthropic"));
        assert!(p.serves("codex"));
    }
}
