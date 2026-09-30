//! Outbound client identity for both upstream families.
//!
//! The upstream expects an official client shape. Which shape depends on the
//! **family**:
//!
//! | family | identity |
//! |---|---|
//! | GPT / Codex | `codex-tui` (or `codex_cli_rs`) User-Agent + `originator` + `Version` + `x-codex-*` |
//! | Claude | `claude-cli/<ver> (external, cli)` + `x-app: cli` + `x-stainless-*` |
//!
//! Identity is a **policy choice** ([`IdentityProfile`], validated by
//! `proxy-common`); the synthesis below is the mechanism. TLS fingerprinting is
//! not handled here — see [`crate::transport`].

use proxy_common::auth::UpstreamAuth;
use proxy_common::config::{AccountFamily, IdentityProfile};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Default Codex CLI version advertised to the upstream.
pub const DEFAULT_CLI_VERSION: &str = "0.153.3";
/// Default Claude Code CLI version (mirrors the Go implementation's built-in).
pub const DEFAULT_CLAUDE_CLI_VERSION: &str = "2.1.258";
/// Session-level beta features the real Codex client negotiates by default.
pub const DEFAULT_BETA_FEATURES: &str = "remote_compaction_v2";
/// Beta value Claude subscription credentials **must** advertise for inference.
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// Anthropic API version header value.
pub const CLAUDE_API_VERSION: &str = "2023-06-01";

/// `x-stainless-package-version` pool (mirrors the Go implementation).
const STAINLESS_SDK_VERSIONS: &[&str] = &["0.112.1", "0.68.0", "0.65.0", "0.63.1", "0.60.0"];
/// `x-stainless-runtime-version` pool.
const STAINLESS_NODE_RUNTIMES: &[&str] =
    &["v26.3.0", "v22.14.0", "v22.11.0", "v20.18.1", "v20.17.0"];
/// `x-stainless-os` pool.
const STAINLESS_OSES: &[&str] = &["MacOS", "Linux", "Windows"];

/// `originator` value for a Codex profile; empty for others.
fn originator_of(profile: IdentityProfile) -> &'static str {
    match profile {
        IdentityProfile::CodexTui => "codex-tui",
        IdentityProfile::CodexCliRs => "codex_cli_rs",
        _ => "",
    }
}

/// Resolved identity settings for one account.
#[derive(Debug, Clone)]
pub struct Identity {
    pub profile: IdentityProfile,
    /// Codex CLI version.
    pub client_version: String,
    /// Claude Code CLI version.
    pub claude_version: String,
    pub os_name: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
    pub beta_features: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            profile: IdentityProfile::default(),
            client_version: DEFAULT_CLI_VERSION.to_string(),
            claude_version: DEFAULT_CLAUDE_CLI_VERSION.to_string(),
            os_name: "Mac OS".to_string(),
            os_version: "15.5.0".to_string(),
            arch: "arm64".to_string(),
            terminal: "xterm-256color".to_string(),
            beta_features: DEFAULT_BETA_FEATURES.to_string(),
        }
    }
}

impl Identity {
    /// Build from a configured profile name, falling back to the family default.
    ///
    /// Configuration validation rejects a profile belonging to the *other*
    /// family; the guard is repeated here so the mechanism can never emit a
    /// mismatched fingerprint (a Codex User-Agent on an Anthropic endpoint) even
    /// if it is constructed by hand.
    pub fn from_profile_name(raw: Option<&str>, family: AccountFamily) -> Self {
        let parsed = raw.and_then(IdentityProfile::parse);
        let profile = match parsed {
            Some(profile) if profile.matches_family(family) => profile,
            Some(_) | None => Self::default_profile(family),
        };
        Self {
            profile,
            ..Default::default()
        }
    }

    /// The natural identity profile for a family.
    pub fn default_profile(family: AccountFamily) -> IdentityProfile {
        match family {
            AccountFamily::Gpt => IdentityProfile::CodexTui,
            AccountFamily::Claude => IdentityProfile::ClaudeCode,
        }
    }

    /// Codex User-Agent:
    /// `{originator}/{version} ({os} {osver}; {arch}) {terminal} ({originator}; {version})`
    pub fn codex_user_agent(&self) -> String {
        let originator = originator_of(self.profile);
        if originator.is_empty() {
            return String::new();
        }
        format!(
            "{}/{} ({} {}; {}) {} ({}; {})",
            originator,
            self.client_version,
            self.os_name,
            self.os_version,
            self.arch,
            self.terminal,
            originator,
            self.client_version
        )
    }

    /// Claude Code User-Agent: `claude-cli/<version> (external, cli)`.
    pub fn claude_user_agent(&self) -> String {
        format!("claude-cli/{} (external, cli)", self.claude_version)
    }

    /// Deterministic installation id for the account (UUIDv4 shape, stable
    /// across restarts without persisting anything).
    pub fn installation_id(&self, account_seed: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("planx:install:v1:{account_seed}").as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes).to_string()
    }

    /// Pick a value from a pool deterministically from `(seed, tag)`.
    ///
    /// Stable per account across restarts and spread across accounts — the same
    /// property the Go implementation gets from its random-per-account pools,
    /// without persisting anything.
    fn pick<'a>(seed: &str, tag: &str, pool: &[&'a str]) -> &'a str {
        if pool.is_empty() {
            return "";
        }
        let mut hasher = Sha256::new();
        hasher.update(format!("planx:id:{tag}:{seed}").as_bytes());
        let digest = hasher.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        let index = u64::from_be_bytes(buf) as usize;
        pool[index % pool.len()]
    }

    /// Identity headers for a family, as the relay's auth seam value.
    ///
    /// **Account-level only.** Per-request session headers (`session-id`,
    /// `thread-id`, …) are deliberately excluded: they are per-request state
    /// while this material is cached per account, and `proxy-relay` already
    /// forwards whatever the downstream client sent, which keeps the Session
    /// view mapped 1:1 onto upstream threads.
    pub fn headers(
        &self,
        family: AccountFamily,
        account_seed: &str,
        account_id: Option<&str>,
    ) -> UpstreamAuth {
        if self.profile == IdentityProfile::Passthrough {
            return UpstreamAuth::Headers {
                set: Vec::new(),
                append: Vec::new(),
            };
        }
        let mut set = Vec::new();
        let mut append = Vec::new();
        match family {
            AccountFamily::Gpt => self.codex_headers(account_seed, account_id, &mut set),
            AccountFamily::Claude => self.claude_headers(account_seed, &mut set, &mut append),
        }
        UpstreamAuth::Headers { set, append }
    }

    fn codex_headers(
        &self,
        account_seed: &str,
        account_id: Option<&str>,
        set: &mut Vec<(String, String)>,
    ) {
        let mut push = |name: &str, value: String| set.push((name.to_string(), value));
        push("user-agent", self.codex_user_agent());
        push("originator", originator_of(self.profile).to_string());
        push("version", self.client_version.clone());
        push(
            "x-codex-installation-id",
            self.installation_id(account_seed),
        );
        if !self.beta_features.trim().is_empty() {
            push("x-codex-beta-features", self.beta_features.clone());
        }
        if let Some(account_id) = account_id.map(str::trim).filter(|s| !s.is_empty()) {
            push("chatgpt-account-id", account_id.to_string());
        }
    }

    fn claude_headers(
        &self,
        account_seed: &str,
        set: &mut Vec<(String, String)>,
        append: &mut Vec<(String, String)>,
    ) {
        let mut push = |name: &str, value: &str| set.push((name.to_string(), value.to_string()));
        push("user-agent", &self.claude_user_agent());
        push("x-app", "cli");
        push("anthropic-version", CLAUDE_API_VERSION);
        push("x-stainless-lang", "js");
        push(
            "x-stainless-package-version",
            Self::pick(account_seed, "sdk", STAINLESS_SDK_VERSIONS),
        );
        push(
            "x-stainless-os",
            Self::pick(account_seed, "os", STAINLESS_OSES),
        );
        push("x-stainless-arch", self.claude_arch(account_seed));
        push("x-stainless-runtime", "node");
        push(
            "x-stainless-runtime-version",
            Self::pick(account_seed, "node", STAINLESS_NODE_RUNTIMES),
        );
        // Subscription credentials are only accepted for inference when the OAuth
        // beta is advertised. Merged, never replaced: the client may already send
        // `claude-code-20250219` and friends.
        append.push(("anthropic-beta".to_string(), CLAUDE_OAUTH_BETA.to_string()));
    }

    fn claude_arch(&self, account_seed: &str) -> &'static str {
        match Self::pick(account_seed, "os", STAINLESS_OSES) {
            "MacOS" => Self::pick(account_seed, "arch", &["arm64", "x64"]),
            "Linux" => Self::pick(account_seed, "arch", &["x64", "arm64"]),
            _ => "x64",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt_identity() -> Identity {
        Identity::from_profile_name(None, AccountFamily::Gpt)
    }

    fn claude_identity() -> Identity {
        Identity::from_profile_name(None, AccountFamily::Claude)
    }

    /// Read a `set` header out of an identity result.
    fn get<'a>(auth: &'a UpstreamAuth, name: &str) -> Option<&'a str> {
        match auth {
            UpstreamAuth::Headers { set, .. } => set
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str()),
            UpstreamAuth::Static(_) => None,
        }
    }

    fn appended(auth: &UpstreamAuth) -> &[(String, String)] {
        match auth {
            UpstreamAuth::Headers { append, .. } => append,
            UpstreamAuth::Static(_) => &[],
        }
    }

    fn is_empty(auth: &UpstreamAuth) -> bool {
        match auth {
            UpstreamAuth::Headers { set, append } => set.is_empty() && append.is_empty(),
            UpstreamAuth::Static(_) => false,
        }
    }

    #[test]
    fn family_defaults_differ() {
        assert_eq!(gpt_identity().profile, IdentityProfile::CodexTui);
        assert_eq!(claude_identity().profile, IdentityProfile::ClaudeCode);
    }

    #[test]
    fn codex_user_agent_matches_originator() {
        let identity = gpt_identity();
        assert!(identity.codex_user_agent().starts_with("codex-tui/"));
        assert_eq!(originator_of(identity.profile), "codex-tui");
    }

    #[test]
    fn claude_user_agent_shape() {
        assert_eq!(
            claude_identity().claude_user_agent(),
            "claude-cli/2.1.258 (external, cli)"
        );
    }

    #[test]
    fn cross_family_profile_is_ignored() {
        // `codex_tui` on Claude would be a mismatched fingerprint.
        let identity = Identity::from_profile_name(Some("codex_tui"), AccountFamily::Claude);
        assert_eq!(identity.profile, IdentityProfile::ClaudeCode);

        let identity = Identity::from_profile_name(Some("claude_code"), AccountFamily::Gpt);
        assert_eq!(identity.profile, IdentityProfile::CodexTui);
    }

    #[test]
    fn passthrough_is_allowed_for_both_families() {
        for family in [AccountFamily::Gpt, AccountFamily::Claude] {
            let identity = Identity::from_profile_name(Some("passthrough"), family);
            assert_eq!(identity.profile, IdentityProfile::Passthrough);
            assert!(is_empty(&identity.headers(family, "seed", Some("acct"))));
        }
    }

    #[test]
    fn codex_headers_carry_identity_and_workspace() {
        let headers = gpt_identity().headers(AccountFamily::Gpt, "acct-1", Some("ws-1"));
        assert!(get(&headers, "user-agent")
            .unwrap()
            .starts_with("codex-tui/"));
        assert_eq!(get(&headers, "originator"), Some("codex-tui"));
        assert_eq!(get(&headers, "version"), Some(DEFAULT_CLI_VERSION));
        assert_eq!(
            get(&headers, "x-codex-beta-features"),
            Some(DEFAULT_BETA_FEATURES)
        );
        assert_eq!(get(&headers, "chatgpt-account-id"), Some("ws-1"));
        assert!(appended(&headers).is_empty(), "codex path appends nothing");
        assert_eq!(get(&headers, "session-id"), None, "no per-request state");
    }

    #[test]
    fn claude_headers_advertise_the_oauth_beta() {
        let headers = claude_identity().headers(AccountFamily::Claude, "acct-1", None);
        assert_eq!(
            get(&headers, "user-agent"),
            Some("claude-cli/2.1.258 (external, cli)")
        );
        assert_eq!(get(&headers, "x-app"), Some("cli"));
        assert_eq!(get(&headers, "anthropic-version"), Some(CLAUDE_API_VERSION));
        assert_eq!(get(&headers, "x-stainless-lang"), Some("js"));
        assert_eq!(get(&headers, "x-stainless-runtime"), Some("node"));
        assert!(
            get(&headers, "anthropic-beta").is_none(),
            "the beta must be merged, not set, so client betas survive"
        );
        assert_eq!(
            appended(&headers),
            [("anthropic-beta".to_string(), CLAUDE_OAUTH_BETA.to_string())]
        );
    }

    #[test]
    fn claude_stainless_values_come_from_the_pools_and_are_stable() {
        let first = claude_identity().headers(AccountFamily::Claude, "acct-1", None);
        let again = claude_identity().headers(AccountFamily::Claude, "acct-1", None);
        assert_eq!(first, again, "same seed must produce the same fingerprint");

        assert!(
            STAINLESS_SDK_VERSIONS.contains(&get(&first, "x-stainless-package-version").unwrap())
        );
        assert!(STAINLESS_OSES.contains(&get(&first, "x-stainless-os").unwrap()));
        assert!(
            STAINLESS_NODE_RUNTIMES.contains(&get(&first, "x-stainless-runtime-version").unwrap())
        );
    }

    #[test]
    fn claude_arch_is_consistent_with_os() {
        for seed in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let headers = claude_identity().headers(AccountFamily::Claude, seed, None);
            match get(&headers, "x-stainless-os").unwrap() {
                "MacOS" | "Linux" => {
                    assert!(matches!(
                        get(&headers, "x-stainless-arch").unwrap(),
                        "arm64" | "x64"
                    ));
                }
                "Windows" => assert_eq!(get(&headers, "x-stainless-arch"), Some("x64")),
                other => panic!("unexpected os {other}"),
            }
        }
    }

    #[test]
    fn installation_id_is_stable_and_v4() {
        let identity = gpt_identity();
        let first = identity.installation_id("acct-1");
        assert_eq!(first, identity.installation_id("acct-1"));
        assert_ne!(first, identity.installation_id("acct-2"));
        assert_eq!(Uuid::parse_str(&first).unwrap().get_version_num(), 4);
    }
}
