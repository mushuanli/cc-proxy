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
///
/// This is a single source of truth: the `User-Agent` (twice), the `version`
/// header and — for the model manifest — the `client_version` query parameter all
/// derive from it, so they can never disagree.
///
/// Keeping it current matters observably: the model manifest is version-gated.
/// With `0.153.3` the endpoint returned 7 models, with `0.159.2` it returned 10
/// (adding `gpt-6-sol` / `gpt-6.1-sol` / `gpt-6-luna`).
pub const DEFAULT_CLI_VERSION: &str = "0.159.2";
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

/// The platform fields the client advertises inside its `User-Agent`.
///
/// The real client reports **the machine it runs on**. Observed on this host:
///
/// ```text
/// codex-tui/0.159.2 (Debian n/a; x86_64) xterm-256color (codex-tui; 0.159.2)
/// ```
///
/// (Captured from the same client: `codex exec` prints the identical platform
/// fields as `codex_exec/… (Debian n/a; x86_64) dumb (codex_exec; …)` — only the
/// originator and `TERM` differ, and the TUI shape is the one we impersonate.)
///
/// Sending a fabricated `Mac OS 15.5.0; arm64` from a Linux container is a
/// needless inconsistency, so the distribution, its version, the architecture
/// and `TERM` are read from the environment.
///
/// `TERM` falls back to `xterm-256color`, not to `dumb`: cc-proxy impersonates
/// an *interactive* client (long-lived session, like the TUI), and the real TUI
/// **refuses to start** when `TERM=dumb` ("Refusing to start the interactive TUI
/// because TERM is set to "dumb""), so a TUI-shaped client advertising `dumb`
/// would contradict itself. `dumb` is what the non-interactive `codex exec`
/// path reports when it is not attached to a terminal.
#[derive(Debug, Clone)]
pub struct Platform {
    pub os_name: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
}

impl Platform {
    pub fn detect() -> Self {
        Self {
            os_name: os_name(),
            os_version: os_version(),
            arch: arch_name(),
            terminal: usable_terminal(),
        }
    }
}

/// `TERM` if it names a real terminal, else `xterm-256color`.
///
/// `dumb` is rejected on purpose: the real TUI refuses to start under it
/// ("Refusing to start the interactive TUI because TERM is set to \"dumb\""),
/// so a TUI-shaped client that advertised `dumb` would contradict itself. A
/// supervised daemon frequently inherits exactly that value.
fn usable_terminal() -> String {
    std::env::var("TERM")
        .ok()
        .map(|term| term.trim().to_string())
        .filter(|term| !term.is_empty() && term != "dumb")
        .unwrap_or_else(|| "xterm-256color".to_string())
}

/// Distribution name, capitalised (`debian` → `Debian`), else the OS family.
fn os_name() -> String {
    if let Some(id) = os_release_field("ID") {
        let mut chars = id.chars();
        if let Some(first) = chars.next() {
            return format!("{}{}", first.to_ascii_uppercase(), chars.as_str());
        }
    }
    match std::env::consts::OS {
        "macos" => "Mac OS".to_string(),
        "windows" => "Windows".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => "unknown".to_string(),
            }
        }
    }
}

/// The client does not report a distribution version: two independent captures
/// of 0.159.2 on this host — the interactive TUI and `codex exec` — both sent
/// `(Debian n/a; x86_64)`, even though `/etc/os-release` carries a `VERSION_ID`.
/// Mirroring the client beats inventing a more specific value, so this stays
/// `n/a` until a real capture shows otherwise.
fn os_version() -> String {
    "n/a".to_string()
}

/// `x86_64` / `aarch64`, matching what the real client prints.
fn arch_name() -> String {
    match std::env::consts::ARCH {
        "x86" => "x86".to_string(),
        "arm" => "arm".to_string(),
        other => other.to_string(),
    }
}

/// Read one `KEY=value` from `/etc/os-release` (Linux only).
fn os_release_field(key: &str) -> Option<String> {
    let raw = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in raw.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            let value = value.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `true` when `candidate` is a strictly newer dotted version than `current`.
///
/// Used to notice that the local Codex CLI moved on while we kept advertising an
/// older version — which is exactly how a stale `client_version` silently costs
/// models from the upstream manifest.
pub fn version_is_newer(candidate: &str, current: &str) -> bool {
    let parse = |raw: &str| -> Vec<u64> {
        raw.trim()
            .split(['.', '-', '+'])
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (mut a, mut b) = (parse(candidate), parse(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

impl Default for Identity {
    fn default() -> Self {
        let platform = Platform::detect();
        Self {
            profile: IdentityProfile::default(),
            client_version: DEFAULT_CLI_VERSION.to_string(),
            claude_version: DEFAULT_CLAUDE_CLI_VERSION.to_string(),
            os_name: platform.os_name,
            os_version: platform.os_version,
            arch: platform.arch,
            terminal: platform.terminal,
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

    /// Advertise a specific CLI version instead of the built-in default.
    ///
    /// The version is what the upstream gates its model manifest on, so being
    /// able to move it without recompiling is the difference between "bump a
    /// setting" and "rebuild the binary".
    pub fn with_cli_version(mut self, version: Option<&str>) -> Self {
        if let Some(version) = version.map(str::trim).filter(|v| !v.is_empty()) {
            self.client_version = version.to_string();
        }
        self
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
    fn the_advertised_version_agrees_in_every_place_it_is_sent() {
        // A mismatch between the UA and the `version` header is a one-glance tell,
        // so pin all four placements (three in the UA, one header) to one value.
        let identity = gpt_identity();
        let ua = identity.codex_user_agent();
        assert_eq!(ua.matches(DEFAULT_CLI_VERSION).count(), 2, "{ua}");
        assert!(
            ua.contains(&format!("codex-tui/{DEFAULT_CLI_VERSION}")),
            "{ua}"
        );
        assert!(
            ua.ends_with(&format!("(codex-tui; {DEFAULT_CLI_VERSION})")),
            "{ua}"
        );

        let headers = identity.headers(AccountFamily::Gpt, "acct-1", Some("ws-1"));
        assert_eq!(get(&headers, "version"), Some(DEFAULT_CLI_VERSION));
        assert_eq!(get(&headers, "user-agent"), Some(ua.as_str()));
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
    fn the_default_profile_is_the_interactive_tui_shape() {
        // cc-proxy serves long-lived interactive sessions, so it impersonates the
        // TUI: originator `codex-tui`, which is what the real client sends for
        // `cli` and `vscode` threads (23 of them in the local history) versus a
        // single `codex_exec` from the one-shot `codex exec` entrypoint.
        let identity = Identity::default();
        assert_eq!(identity.profile, IdentityProfile::CodexTui);
        assert!(identity.codex_user_agent().starts_with("codex-tui/"));
        assert!(identity
            .codex_user_agent()
            .ends_with("(codex-tui; 0.159.2)"));

        let headers = identity.headers(AccountFamily::Gpt, "acct", None);
        assert_eq!(get(&headers, "originator"), Some("codex-tui"));
    }

    #[test]
    fn a_dumb_terminal_is_never_advertised() {
        // The real TUI refuses to run under TERM=dumb, so we must not claim it.
        let saved = std::env::var("TERM").ok();
        std::env::set_var("TERM", "dumb");
        assert_eq!(usable_terminal(), "xterm-256color");
        std::env::set_var("TERM", "screen-256color");
        assert_eq!(usable_terminal(), "screen-256color");
        match saved {
            Some(value) => std::env::set_var("TERM", value),
            None => std::env::remove_var("TERM"),
        }
    }

    #[test]
    fn the_platform_string_is_taken_from_the_environment() {
        // No fabricated "Mac OS 15.5.0; arm64": the UA must describe this host,
        // like the real client's does.
        let platform = Platform::detect();
        assert!(!platform.os_name.is_empty());
        assert!(!platform.arch.is_empty());
        assert!(!platform.terminal.is_empty());

        let ua = gpt_identity().codex_user_agent();
        assert!(
            ua.contains(&format!(
                "({} {}; {})",
                platform.os_name, platform.os_version, platform.arch
            )),
            "{ua}"
        );
    }

    #[test]
    fn cli_version_can_be_overridden_without_recompiling() {
        let identity = Identity::default().with_cli_version(Some("9.9.9"));
        assert_eq!(identity.client_version, "9.9.9");
        assert!(identity.codex_user_agent().contains("codex-tui/9.9.9"));
        // Blank means "keep the built-in default".
        assert_eq!(
            Identity::default()
                .with_cli_version(Some("  "))
                .client_version,
            DEFAULT_CLI_VERSION
        );
        assert_eq!(
            Identity::default().with_cli_version(None).client_version,
            DEFAULT_CLI_VERSION
        );
    }

    #[test]
    fn version_comparison_is_numeric_not_lexicographic() {
        assert!(version_is_newer("0.159.2", "0.153.3"));
        assert!(version_is_newer("0.160.0", "0.159.2"));
        assert!(version_is_newer("1.0", "0.9.9"));
        assert!(
            version_is_newer("0.159.10", "0.159.9"),
            "10 > 9 numerically"
        );
        assert!(!version_is_newer("0.153.3", "0.159.2"));
        assert!(
            !version_is_newer("0.159.2", "0.159.2"),
            "equal is not newer"
        );
        assert!(
            version_is_newer("0.159.2", "0.159"),
            "shorter pads with zeros"
        );
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
