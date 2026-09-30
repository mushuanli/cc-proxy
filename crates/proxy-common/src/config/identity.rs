//! Outbound-identity vocabulary: **policy names only**.
//!
//! These enums are the configuration surface — *which* identity profile and
//! *which* TLS impersonation profile an operator selected. They carry no
//! behaviour: the actual User-Agent synthesis lives in `proxy-planx::identity`
//! and the TLS/HTTP2 fingerprint mapping in `proxy-planx::transport`.
//!
//! Keeping the names here means an unknown value is a **configuration error**
//! reported by [`super::AppConfig::validate`], instead of a silent fall-back to
//! the family default inside the mechanism.

use serde::{Deserialize, Serialize};

use super::account::AccountFamily;

/// Built-in outbound identity profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityProfile {
    /// `codex-tui` — the shape validated against the upstream.
    #[default]
    CodexTui,
    /// `codex_cli_rs` — the Rust Codex CLI originator.
    CodexCliRs,
    /// `claude-cli/<ver> (external, cli)` plus `x-app` / `x-stainless-*`.
    ClaudeCode,
    /// Do not synthesise identity headers; forward what the client sent.
    Passthrough,
}

impl IdentityProfile {
    /// Every profile, for validation messages and UI lists.
    pub const ALL: &'static [IdentityProfile] = &[
        IdentityProfile::CodexTui,
        IdentityProfile::CodexCliRs,
        IdentityProfile::ClaudeCode,
        IdentityProfile::Passthrough,
    ];

    /// Parse a configured profile name leniently (`codex-tui`, `tui`, `claude`, …).
    pub fn parse(raw: &str) -> Option<Self> {
        match normalize(raw).as_str() {
            "" => Some(IdentityProfile::CodexTui),
            "codex" | "codexcli" | "codextui" | "tui" => Some(IdentityProfile::CodexTui),
            "codexclirs" | "clirs" | "rs" => Some(IdentityProfile::CodexCliRs),
            "claude" | "claudecode" | "claudecli" => Some(IdentityProfile::ClaudeCode),
            "passthrough" | "none" | "off" => Some(IdentityProfile::Passthrough),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            IdentityProfile::CodexTui => "codex_tui",
            IdentityProfile::CodexCliRs => "codex_cli_rs",
            IdentityProfile::ClaudeCode => "claude_code",
            IdentityProfile::Passthrough => "passthrough",
        }
    }

    /// Whether this profile belongs to the given family.
    ///
    /// A profile from the other family would produce a mismatched fingerprint
    /// (a Codex User-Agent on an Anthropic endpoint), so it is rejected by
    /// configuration validation rather than silently rewritten.
    pub fn matches_family(self, family: AccountFamily) -> bool {
        matches!(
            (self, family),
            (IdentityProfile::ClaudeCode, AccountFamily::Claude)
                | (
                    IdentityProfile::CodexTui | IdentityProfile::CodexCliRs,
                    AccountFamily::Gpt
                )
                | (IdentityProfile::Passthrough, _)
        )
    }

    /// Human-readable list of accepted names, for error messages.
    pub fn accepted_names() -> String {
        IdentityProfile::ALL
            .iter()
            .map(|profile| profile.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// TLS / HTTP2 fingerprint impersonation profiles.
///
/// Only effective when the binary is built with the `impersonate` cargo
/// feature; the name is validated either way so a typo is reported instead of
/// silently disabling impersonation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Impersonation {
    /// No impersonation (default).
    #[default]
    Off,
    /// Latest Chrome.
    Chrome,
    /// Chrome 142.
    Chrome142,
    Edge,
    Firefox,
    Safari,
}

impl Impersonation {
    /// Every profile, for validation messages and UI lists.
    pub const ALL: &'static [Impersonation] = &[
        Impersonation::Off,
        Impersonation::Chrome,
        Impersonation::Chrome142,
        Impersonation::Edge,
        Impersonation::Firefox,
        Impersonation::Safari,
    ];

    /// Parse a configured profile name leniently (`chrome-142`, `Safari26`, …).
    pub fn parse(raw: &str) -> Option<Self> {
        match normalize(raw).as_str() {
            "" | "off" | "none" | "false" | "0" => Some(Impersonation::Off),
            "chrome" | "chrome149" | "latest" => Some(Impersonation::Chrome),
            "chrome142" => Some(Impersonation::Chrome142),
            "edge" | "edge148" => Some(Impersonation::Edge),
            "firefox" | "firefox151" => Some(Impersonation::Firefox),
            "safari" | "safari26" => Some(Impersonation::Safari),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Impersonation::Off => "off",
            Impersonation::Chrome => "chrome",
            Impersonation::Chrome142 => "chrome142",
            Impersonation::Edge => "edge",
            Impersonation::Firefox => "firefox",
            Impersonation::Safari => "safari",
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, Impersonation::Off)
    }

    /// Human-readable list of accepted names, for error messages.
    pub fn accepted_names() -> String {
        Impersonation::ALL
            .iter()
            .map(|profile| profile.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

impl std::fmt::Display for Impersonation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lower-case and drop every non-alphanumeric character, so `codex-tui`,
/// `Codex_Tui` and `CODEXTUI` all match.
fn normalize(raw: &str) -> String {
    raw.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_names_round_trip() {
        for profile in IdentityProfile::ALL {
            assert_eq!(IdentityProfile::parse(profile.as_str()), Some(*profile));
        }
        assert_eq!(
            IdentityProfile::parse("Codex-TUI"),
            Some(IdentityProfile::CodexTui)
        );
        assert_eq!(IdentityProfile::parse(""), Some(IdentityProfile::CodexTui));
        assert_eq!(IdentityProfile::parse("gpt-5"), None);
    }

    #[test]
    fn impersonation_names_round_trip() {
        for profile in Impersonation::ALL {
            assert_eq!(Impersonation::parse(profile.as_str()), Some(*profile));
        }
        assert_eq!(
            Impersonation::parse("chrome-142"),
            Some(Impersonation::Chrome142)
        );
        assert_eq!(Impersonation::parse(""), Some(Impersonation::Off));
        assert_eq!(Impersonation::parse("netscape"), None);
    }

    #[test]
    fn cross_family_profiles_are_rejected_by_matches_family() {
        assert!(!IdentityProfile::CodexTui.matches_family(AccountFamily::Claude));
        assert!(!IdentityProfile::ClaudeCode.matches_family(AccountFamily::Gpt));
        assert!(IdentityProfile::CodexCliRs.matches_family(AccountFamily::Gpt));
        assert!(IdentityProfile::Passthrough.matches_family(AccountFamily::Gpt));
        assert!(IdentityProfile::Passthrough.matches_family(AccountFamily::Claude));
    }

    #[test]
    fn accepted_names_are_documented() {
        assert!(IdentityProfile::accepted_names().contains("codex_tui"));
        assert!(Impersonation::accepted_names().contains("chrome142"));
    }
}
