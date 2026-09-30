use serde::{Deserialize, Serialize};

use crate::protocol::WireProtocol;

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
    /// Name of a `[[proxy.accounts]]` entry to authenticate with.
    ///
    /// When set, `token` is ignored for this provider and the credential is
    /// produced by the injected account mechanism (`proxy-planx`), in either
    /// api-key or plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl Provider {
    /// True if this provider serves the given protocol.
    ///
    /// Entries are matched through [`WireProtocol::parse`], so aliases and
    /// casing (`Codex`, `openai`, `messages`) behave as an operator expects
    /// instead of silently failing an exact string comparison. An unrecognised
    /// entry matches nothing — configuration validation reports it separately.
    pub fn serves(&self, protocol: &str) -> bool {
        if self.protocols.is_empty() {
            return true;
        }
        let Some(wanted) = WireProtocol::parse(protocol) else {
            return false;
        };
        self.protocols
            .iter()
            .any(|entry| WireProtocol::parse(entry) == Some(wanted))
    }

    /// `protocols` entries that are not a recognised protocol name.
    pub fn unknown_protocols(&self) -> Vec<String> {
        self.protocols
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty() && WireProtocol::parse(entry).is_none())
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(protocols: &[&str]) -> Provider {
        Provider {
            name: "p".into(),
            url: "https://x".into(),
            token: None,
            proxy: None,
            protocols: protocols.iter().map(|p| p.to_string()).collect(),
            codex_url: None,
            account: None,
        }
    }

    #[test]
    fn empty_protocols_serves_all() {
        let p = provider(&[]);
        assert!(p.serves("anthropic"));
        assert!(p.serves("codex"));
    }

    #[test]
    fn single_protocol_restricts() {
        let p = provider(&["anthropic"]);
        assert!(p.serves("anthropic"));
        assert!(!p.serves("codex"));
    }

    #[test]
    fn multi_protocol_serves_both() {
        let p = provider(&["anthropic", "codex"]);
        assert!(p.serves("anthropic"));
        assert!(p.serves("codex"));
        assert!(!p.serves("other"));
    }

    #[test]
    fn protocol_aliases_and_casing_are_tolerated() {
        assert!(provider(&["Codex"]).serves("codex"));
        assert!(provider(&["openai"]).serves("codex"));
        assert!(provider(&["messages"]).serves("anthropic"));
        assert!(!provider(&["codex"]).serves("anthropic"));
    }

    #[test]
    fn unknown_protocols_are_reported() {
        assert_eq!(
            provider(&["codex"]).unknown_protocols(),
            Vec::<String>::new()
        );
        assert_eq!(provider(&["grpc"]).unknown_protocols(), vec!["grpc"]);
        assert_eq!(
            provider(&["", "  "]).unknown_protocols(),
            Vec::<String>::new()
        );
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
