//! Upstream account configuration.
//!
//! **Policy only** — pure data describing which account exists, which upstream
//! family it belongs to, and which credential mode it uses. All behaviour
//! (OAuth refresh, client identity, quota probing) lives in the `proxy-planx`
//! crate behind the [`crate::auth::PlanAuthProvider`] trait.
//!
//! # Two families × two modes
//!
//! | family | `mode = "api_key"` | `mode = "plan"` |
//! |---|---|---|
//! | `gpt` (ChatGPT / Codex) | static API key | ChatGPT/Codex subscription OAuth |
//! | `claude` (Anthropic) | static API key | Claude Pro/Max subscription OAuth |
//!
//! Both families support both modes; the family only decides *how* the
//! credential is presented on the wire (header name, required betas) and which
//! client identity is synthesised.

use serde::{Deserialize, Serialize};

/// Upstream family: which provider the account belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountFamily {
    /// ChatGPT / Codex (`chatgpt.com/backend-api/codex`).
    #[default]
    Gpt,
    /// Anthropic / Claude Code (`api.anthropic.com`).
    Claude,
}

impl AccountFamily {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "gpt" | "openai" | "codex" | "chatgpt" => Some(AccountFamily::Gpt),
            "claude" | "anthropic" => Some(AccountFamily::Claude),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AccountFamily::Gpt => "gpt",
            AccountFamily::Claude => "claude",
        }
    }

    /// The wire protocol this family's credentials can be used against.
    pub fn protocol(&self) -> crate::protocol::WireProtocol {
        match self {
            AccountFamily::Gpt => crate::protocol::WireProtocol::Codex,
            AccountFamily::Claude => crate::protocol::WireProtocol::Anthropic,
        }
    }
}

/// How the account authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountMode {
    /// Static API key (no refresh, no identity synthesis).
    ApiKey,
    /// Subscription account (OAuth; refreshable, identity synthesised).
    #[default]
    Plan,
}

impl AccountMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "api_key" | "apikey" | "key" | "token" => Some(AccountMode::ApiKey),
            "" | "plan" | "oauth" | "subscription" => Some(AccountMode::Plan),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AccountMode::ApiKey => "api_key",
            AccountMode::Plan => "plan",
        }
    }
}

/// One upstream account, referenced by name from `Provider::account`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Referenced by `Provider::account`.
    pub name: String,

    #[serde(default)]
    pub family: AccountFamily,

    #[serde(default)]
    pub mode: AccountMode,

    // ── mode = api_key ──
    /// Static API key. Claude keys go to `x-api-key`, GPT keys follow the
    /// legacy `sk-` → Bearer / otherwise `x-api-key` rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    // ── mode = plan ──
    /// Codex CLI `auth.json` path (GPT only). Supports a leading `~/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_json: Option<String>,

    /// Refreshable refresh token.
    /// Claude: `sk-ant-ort01-…`; GPT: the `tokens.refresh_token` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Long-lived access token with no refresh token.
    /// Claude: a `claude setup-token` value (`sk-ant-oat01-…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    /// Workspace / organisation UUID override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Write rotated credentials back to `auth_json` after a refresh (GPT only).
    #[serde(default)]
    pub persist: bool,

    /// Outbound identity profile; family-specific names accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,

    /// TLS/HTTP2 impersonation profile (`off` / `chrome` / …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
}

impl AccountConfig {
    /// Path to the `auth.json` to read, if this account uses one.
    pub fn auth_json_path(&self) -> Option<&str> {
        self.auth_json.as_deref().filter(|p| !p.trim().is_empty())
    }

    /// Whether rotated credentials should be written back to disk.
    pub fn should_persist(&self) -> bool {
        self.persist && self.family == AccountFamily::Gpt && self.auth_json_path().is_some()
    }

    fn filled(value: &Option<String>) -> bool {
        value.as_deref().is_some_and(|v| !v.trim().is_empty())
    }

    /// Whether the configured fields are sufficient for the declared mode.
    ///
    /// Returns a human-readable reason when they are not.
    pub fn credential_problem(&self) -> Option<String> {
        match self.mode {
            AccountMode::ApiKey => {
                if Self::filled(&self.api_key) {
                    None
                } else {
                    Some("mode = api_key requires api_key".to_string())
                }
            }
            AccountMode::Plan => {
                let has_any = Self::filled(&self.auth_json)
                    || Self::filled(&self.refresh_token)
                    || Self::filled(&self.access_token);
                if !has_any {
                    return Some(
                        "mode = plan requires one of auth_json / refresh_token / access_token"
                            .to_string(),
                    );
                }
                if self.family == AccountFamily::Claude && Self::filled(&self.auth_json) {
                    return Some(
                        "auth_json is a Codex CLI file and is not supported for family = claude; \
                         use refresh_token or access_token"
                            .to_string(),
                    );
                }
                None
            }
        }
    }

    /// Whether this account needs a network refresh at some point.
    pub fn is_refreshable(&self) -> bool {
        self.mode == AccountMode::Plan && Self::filled(&self.refresh_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_gpt() -> AccountConfig {
        AccountConfig {
            name: "gpt-work".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            auth_json: Some("~/.codex/auth.json".into()),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_gpt_plan() {
        let account = AccountConfig::default();
        assert_eq!(account.family, AccountFamily::Gpt);
        assert_eq!(account.mode, AccountMode::Plan);
        assert_eq!(account.family.as_str(), "gpt");
        assert_eq!(account.mode.as_str(), "plan");
    }

    #[test]
    fn family_parsing_is_lenient() {
        assert_eq!(AccountFamily::parse("GPT"), Some(AccountFamily::Gpt));
        assert_eq!(AccountFamily::parse("chatgpt"), Some(AccountFamily::Gpt));
        assert_eq!(
            AccountFamily::parse("anthropic"),
            Some(AccountFamily::Claude)
        );
        assert_eq!(AccountFamily::parse("gemini"), None);
    }

    #[test]
    fn mode_parsing_is_lenient() {
        assert_eq!(AccountMode::parse("api_key"), Some(AccountMode::ApiKey));
        assert_eq!(AccountMode::parse("apikey"), Some(AccountMode::ApiKey));
        assert_eq!(AccountMode::parse(""), Some(AccountMode::Plan));
        assert_eq!(AccountMode::parse("oauth"), Some(AccountMode::Plan));
        assert_eq!(AccountMode::parse("magic"), None);
    }

    #[test]
    fn api_key_mode_requires_a_key() {
        let account = AccountConfig {
            name: "k".into(),
            mode: AccountMode::ApiKey,
            ..Default::default()
        };
        assert!(account
            .credential_problem()
            .unwrap()
            .contains("requires api_key"));
    }

    #[test]
    fn api_key_mode_accepts_both_families() {
        for family in [AccountFamily::Gpt, AccountFamily::Claude] {
            let account = AccountConfig {
                name: "k".into(),
                family,
                mode: AccountMode::ApiKey,
                api_key: Some("sk-test".into()),
                ..Default::default()
            };
            assert_eq!(account.credential_problem(), None, "{family:?}");
        }
    }

    #[test]
    fn gpt_plan_accepts_any_of_the_three_sources() {
        for account in [
            plan_gpt(),
            AccountConfig {
                refresh_token: Some("rt".into()),
                ..plan_gpt()
            },
            AccountConfig {
                access_token: Some("at".into()),
                ..plan_gpt()
            },
        ] {
            assert_eq!(account.credential_problem(), None);
        }
    }

    #[test]
    fn claude_plan_rejects_a_codex_auth_json() {
        let account = AccountConfig {
            name: "c".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            auth_json: Some("~/.codex/auth.json".into()),
            ..Default::default()
        };
        assert!(account
            .credential_problem()
            .unwrap()
            .contains("not supported for family = claude"));
    }

    #[test]
    fn claude_plan_accepts_refresh_or_setup_token() {
        for token in [
            ("refresh_token", "sk-ant-ort01-x"),
            ("access_token", "sk-ant-oat01-x"),
        ] {
            let mut account = AccountConfig {
                name: "c".into(),
                family: AccountFamily::Claude,
                mode: AccountMode::Plan,
                ..Default::default()
            };
            match token.0 {
                "refresh_token" => account.refresh_token = Some(token.1.into()),
                _ => account.access_token = Some(token.1.into()),
            }
            assert_eq!(account.credential_problem(), None, "{}", token.0);
        }
    }

    #[test]
    fn plan_mode_without_any_source_is_rejected() {
        let account = AccountConfig {
            name: "empty".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            ..Default::default()
        };
        assert!(account
            .credential_problem()
            .unwrap()
            .contains("requires one of"));
    }

    #[test]
    fn persist_requires_gpt_and_a_path() {
        let mut account = plan_gpt();
        assert!(!account.should_persist());
        account.persist = true;
        assert!(account.should_persist());

        // Claude has no portable credentials file to write back to.
        account.family = AccountFamily::Claude;
        assert!(!account.should_persist());
    }

    #[test]
    fn refreshable_only_for_plan_with_refresh_token() {
        let mut account = plan_gpt();
        assert!(!account.is_refreshable());
        account.refresh_token = Some("rt".into());
        assert!(account.is_refreshable());
        account.mode = AccountMode::ApiKey;
        assert!(!account.is_refreshable());
    }

    #[test]
    fn legacy_config_without_new_fields_deserializes() {
        let account: AccountConfig = serde_json::from_str(r#"{ "name": "work" }"#).unwrap();
        assert_eq!(account.name, "work");
        assert_eq!(account.family, AccountFamily::Gpt);
        assert_eq!(account.mode, AccountMode::Plan);
    }
}
