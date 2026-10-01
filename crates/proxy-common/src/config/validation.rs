use crate::config::{
    AccountConfig, AccountFamily, AppConfig, IdentityProfile, Impersonation, ModelsKind, Provider,
    AUTO_PROXY_UPSTREAM, FORBID_PROXY_UPSTREAM,
};
use crate::protocol::WireProtocol;

/// Effort values accepted by `active_effort` (`auto` is always accepted).
/// Reasoning levels accepted by `active_effort` / `UpstreamConfig.effort`.
///
/// `low`…`ultra` are the values the upstream manifest actually advertises
/// (`supported_reasoning_levels[].effort`); `ultracode` is cc-proxy's own
/// composite level (xhigh + workflow orchestration) and is kept alongside them.
const VALID_EFFORTS: &[&str] = &[
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
    "ultracode",
];

impl AppConfig {
    /// Validate the configuration, returning a list of human-readable errors.
    ///
    /// Each rule lives in its own `validate_*` helper so this function reads as
    /// a table of contents; rules never mutate the configuration.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        self.validate_active_upstreams(&mut errors);
        self.validate_upstream_tiers(&mut errors);
        self.validate_pricing(&mut errors);
        self.validate_names(&mut errors);
        self.validate_accounts(&mut errors);
        self.validate_providers(&mut errors);
        self.validate_effort(&mut errors);
        self.validate_transport(&mut errors);
        self.validate_server(&mut errors);
        errors
    }

    /// `active_*` selectors must name an existing upstream.
    fn validate_active_upstreams(&self, errors: &mut Vec<String>) {
        for (field, value) in [("active_upstream", self.proxy.active_upstream.as_str())] {
            if !value.is_empty() && !self.proxy.upstreams.iter().any(|u| u.name == value) {
                errors.push(format!("{field} '{value}' not found in upstreams"));
            }
        }
        // A plan is a connection, not an upstream: it must name a real account.
        let plan = self.proxy.active_plan.trim();
        if !plan.is_empty() && !self.proxy.accounts.iter().any(|a| a.name == plan) {
            errors.push(format!(
                "active_plan '{plan}' not found in accounts (it names a [[proxy.accounts]] entry, not an upstream)"
            ));
        }

        let proxy_upstream = self.proxy.active_proxy_upstream.as_str();
        if !proxy_upstream.is_empty()
            && proxy_upstream != AUTO_PROXY_UPSTREAM
            && proxy_upstream != FORBID_PROXY_UPSTREAM
            && !self
                .proxy
                .upstreams
                .iter()
                .any(|u| u.name == proxy_upstream)
        {
            errors.push(format!(
                "active_proxy_upstream '{proxy_upstream}' not found in upstreams"
            ));
        }
    }

    /// Every tier rule must name an existing provider and a mapped model.
    fn validate_upstream_tiers(&self, errors: &mut Vec<String>) {
        for upstream in &self.proxy.upstreams {
            let rules = [
                ("high", &upstream.high),
                ("mid", &upstream.mid),
                ("low", &upstream.low),
                ("default", &upstream.default),
            ];
            for (tier, rule_opt) in rules {
                let Some(rule) = rule_opt else { continue };
                if rule.provider.is_empty() {
                    continue;
                }
                if !self.proxy.providers.iter().any(|p| p.name == rule.provider) {
                    errors.push(format!(
                        "upstream '{}' {tier}: provider '{}' not found",
                        upstream.name, rule.provider
                    ));
                    continue;
                }
                if rule.model.is_empty() {
                    continue;
                }
                if let Some(mp) = self.model_pricing.iter().find(|mp| mp.id == rule.model) {
                    if !mp.providers.contains_key(&rule.provider) {
                        errors.push(format!(
                            "upstream '{}' {tier}: logical model '{}' has no mapping for provider '{}'",
                            upstream.name, rule.model, rule.provider
                        ));
                    }
                }
            }
        }
    }

    /// Price arrays must be 0/2/4 long and non-negative.
    fn validate_pricing(&self, errors: &mut Vec<String>) {
        for mp in &self.model_pricing {
            let len = mp.price.len();
            if len != 0 && len != 2 && len != 4 {
                errors.push(format!(
                    "model_pricing '{}': price must have 0, 2, or 4 elements, got {len}",
                    mp.id
                ));
            }
            for (i, &p) in mp.price.iter().enumerate() {
                if p < 0.0 {
                    errors.push(format!(
                        "model_pricing '{}': price[{}] must be >= 0, got {}",
                        mp.id, i, p
                    ));
                }
            }
        }
    }

    /// Names are identity keys: they must be non-empty, canonical (no surrounding
    /// whitespace) and unique. Account names are compared trimmed at runtime, so
    /// accepting `" work"` here would make a reference resolve differently before
    /// and after a reload.
    fn validate_names(&self, errors: &mut Vec<String>) {
        let providers: Vec<&str> = self
            .proxy
            .providers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let upstreams: Vec<&str> = self
            .proxy
            .upstreams
            .iter()
            .map(|u| u.name.as_str())
            .collect();
        let accounts: Vec<&str> = self
            .proxy
            .accounts
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let pricing: Vec<&str> = self.model_pricing.iter().map(|mp| mp.id.as_str()).collect();

        for (kind, names) in [
            ("provider", &providers),
            ("upstream", &upstreams),
            ("account", &accounts),
        ] {
            if has_duplicates(names) {
                errors.push(format!("duplicate {kind} names found"));
            }
            for name in names.iter() {
                if name.trim().is_empty() {
                    errors.push(format!("{kind} with empty name found"));
                } else if name.trim() != *name {
                    errors.push(format!(
                        "{kind} name '{name}' must not have leading or trailing whitespace"
                    ));
                }
            }
        }
        if has_duplicates(&pricing) {
            errors.push("duplicate model_pricing ids found".to_string());
        }
    }

    /// Accounts: credentials sufficient for the mode, and valid identity names.
    fn validate_accounts(&self, errors: &mut Vec<String>) {
        for account in &self.proxy.accounts {
            let label = account.name.trim();
            if let Some(problem) = account.credential_problem() {
                errors.push(format!("account '{label}': {problem}"));
            }
            if account.persist && account.family == AccountFamily::Claude {
                errors.push(format!(
                    "account '{label}': persist is only supported for family = gpt"
                ));
            }
            Self::validate_account_identity(account, label, errors);
        }
    }

    /// `identity` / `impersonate` are policy names; an unknown value must be an
    /// error rather than a silent fall-back inside the mechanism.
    fn validate_account_identity(account: &AccountConfig, label: &str, errors: &mut Vec<String>) {
        if let Some(raw) = account
            .identity
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            match IdentityProfile::parse(raw) {
                Some(profile) if profile.matches_family(account.family) => {}
                Some(profile) => errors.push(format!(
                    "account '{label}': identity '{}' belongs to the other family, not {}",
                    profile.as_str(),
                    account.family.as_str()
                )),
                None => errors.push(format!(
                    "account '{label}': unknown identity '{raw}' (expected one of {})",
                    IdentityProfile::accepted_names()
                )),
            }
        }
        if let Some(raw) = account
            .impersonate
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if Impersonation::parse(raw).is_none() {
                errors.push(format!(
                    "account '{label}': unknown impersonate profile '{raw}' (expected one of {})",
                    Impersonation::accepted_names()
                ));
            }
        }
        // A version is sent verbatim as `Version`, in the User-Agent and as the
        // manifest's `client_version`, so anything implausible there is a
        // configuration error rather than something to discover at runtime.
        if let Some(raw) = account
            .cli_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let plausible = raw.len() <= 32
                && raw
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            if !plausible {
                errors.push(format!(
                    "account '{label}': cli_version '{raw}' must look like a version, e.g. 0.159.2"
                ));
            }
        }
    }

    /// Providers: known protocol names, no mixed credential mechanisms, and an
    /// account whose family agrees with the protocols the provider serves.
    fn validate_providers(&self, errors: &mut Vec<String>) {
        for provider in &self.proxy.providers {
            let label = provider.name.trim();
            for unknown in provider.unknown_protocols() {
                errors.push(format!(
                    "provider '{label}': unknown protocol '{unknown}' (expected one of {})",
                    WireProtocol::accepted_names()
                ));
            }
            if provider
                .account
                .as_deref()
                .map(str::trim)
                .is_some_and(|account| account.is_empty())
            {
                errors.push(format!("provider '{label}' has an empty account"));
            }
            self.validate_provider_credentials(provider, label, errors);
            self.validate_provider_catalog(provider, label, errors);
        }
    }

    /// Catalog overrides: a known kind name, and an absolute http(s) URL.
    fn validate_provider_catalog(
        &self,
        provider: &Provider,
        label: &str,
        errors: &mut Vec<String>,
    ) {
        if let Some(raw) = provider
            .models_kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
        {
            if ModelsKind::parse(raw).is_none() {
                errors.push(format!(
                    "provider '{label}': unknown models_kind '{raw}' (expected one of {})",
                    ModelsKind::accepted_names()
                ));
            }
        }
        if let Some(raw) = provider
            .models_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            if !raw.starts_with("http://") && !raw.starts_with("https://") {
                errors.push(format!(
                    "provider '{label}': models_url '{raw}' must be an absolute http(s) URL"
                ));
            }
        }
    }

    fn validate_provider_credentials(
        &self,
        provider: &Provider,
        label: &str,
        errors: &mut Vec<String>,
    ) {
        let Some(name) = provider
            .account
            .as_deref()
            .map(str::trim)
            .filter(|account| !account.is_empty())
        else {
            return;
        };
        // Mixing mechanisms silently would be ambiguous; make the operator choose.
        if provider
            .token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
        {
            errors.push(format!(
                "provider '{label}' sets both token and account; remove one"
            ));
        }
        let Some(account) = self.proxy.accounts.iter().find(|a| a.name.trim() == name) else {
            errors.push(format!(
                "provider '{label}' references unknown account '{name}'"
            ));
            return;
        };
        // A Claude credential on a codex-only provider (or vice versa) sends a
        // fingerprint the upstream will never accept; catch it here.
        if provider.protocols.is_empty() {
            return;
        }
        let account_protocol = account.family.protocol();
        if !provider.serves(account_protocol.as_str()) {
            errors.push(format!(
                "provider '{label}' serves [{}] but account '{name}' is a {} account ({})",
                provider.protocols.join(", "),
                account.family.as_str(),
                account_protocol.as_str()
            ));
        }
    }

    /// `active_effort` must be a known tier or `auto`.
    fn validate_effort(&self, errors: &mut Vec<String>) {
        let effort = self.proxy.active_effort.as_str();
        if !effort.is_empty() && effort != "auto" && !VALID_EFFORTS.contains(&effort) {
            errors.push(format!(
                "invalid active_effort '{effort}': must be one of: auto, {}",
                VALID_EFFORTS.join(", ")
            ));
        }
    }

    /// Proxy URLs must carry a supported scheme.
    fn validate_transport(&self, errors: &mut Vec<String>) {
        if let Some(url) = self.proxy.http_proxy.as_deref() {
            if !url.is_empty() && !is_valid_proxy_url(url) {
                errors.push(format!(
                    "invalid global http_proxy '{url}': must be http://, https://, or socks5:// URL"
                ));
            }
        }
        for p in &self.proxy.providers {
            if let Some(proxy) = p.proxy.as_deref() {
                if !proxy.is_empty() && !is_valid_proxy_url(proxy) {
                    errors.push(format!(
                        "provider '{}': invalid proxy '{}': must be http://, https://, or socks5:// URL",
                        p.name, proxy
                    ));
                }
            }
        }
    }

    fn validate_server(&self, errors: &mut Vec<String>) {
        if let Some(token) = self.server.auth_token.as_deref() {
            if !(16..=256).contains(&token.len())
                || !token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                errors.push(
                    "server.auth_token must be 16-256 ASCII letters, digits, '-' or '_'"
                        .to_string(),
                );
            }
        }
        self.validate_listen_address(errors);
        self.validate_listen_ports(errors);
        self.validate_cors_origins(errors);
    }

    /// The bind address must be an IP literal.
    ///
    /// Host names are **not** resolved, so `"localhost"` used to pass the
    /// non-loopback auth check and then fail inside `TcpListener::bind` with a
    /// cryptic `invalid IP address syntax`. `"0.0.0.0"` / `"::"` are the
    /// "all interfaces" spellings and are accepted here.
    fn validate_listen_address(&self, errors: &mut Vec<String>) {
        let address = self.server.listen_address.trim();
        if address.is_empty() {
            errors.push("server.listen_address must not be empty".to_string());
        } else if address.parse::<std::net::IpAddr>().is_err() {
            errors.push(format!(
                "server.listen_address '{address}' must be an IP literal \
                 (127.0.0.1, 0.0.0.0 for all IPv4 interfaces, ::1, :: for all IPv6, \
                 or a specific address); host names are not resolved"
            ));
        }
    }

    /// CORS entries are browser origins: scheme + host + optional port, nothing
    /// else. A bare `host:port` is the common mistake, so say what is missing.
    fn validate_cors_origins(&self, errors: &mut Vec<String>) {
        for raw in &self.server.cors_origins {
            let origin = raw.trim().trim_end_matches('/');
            if origin == "*" {
                continue;
            }
            if origin.is_empty() {
                errors.push("server.cors_origins contains an empty entry".to_string());
                continue;
            }
            if !(origin.starts_with("http://") || origin.starts_with("https://")) {
                errors.push(format!(
                    "server.cors_origins entry '{raw}' must include the scheme, e.g. http://192.168.31.10:3000"
                ));
                continue;
            }
            let authority = origin
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            if authority.is_empty() || authority.contains(['/', '?', '#']) {
                errors.push(format!(
                    "server.cors_origins entry '{raw}' must be an origin only (scheme://host[:port]), with no path"
                ));
            }
        }
    }

    /// Two listeners, so the ports must be usable and must not collide.
    fn validate_listen_ports(&self, errors: &mut Vec<String>) {
        let (http, proxy) = (self.server.http_port, self.server.proxy_port);
        if http == proxy {
            errors.push(format!(
                "server.http_port and server.proxy_port must differ (both {http})"
            ));
        }
        for (field, port) in [("http_port", http), ("proxy_port", proxy)] {
            if port == 0 {
                errors.push(format!(
                    "server.{field} must be 1-65535; 0 would let the OS pick a port the dashboard cannot report"
                ));
            }
        }
    }
}

/// Check that a proxy URL starts with a valid scheme.
fn is_valid_proxy_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("socks5://")
}

fn has_duplicates<T: PartialEq>(items: &[T]) -> bool {
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if items[i] == items[j] {
                return true;
            }
        }
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AccountFamily, AccountMode};

    #[test]
    fn empty_config_validates() {
        let config = AppConfig::default();
        assert!(config.validate().is_empty());
    }

    #[test]
    fn duplicate_provider_detected() {
        let mut config = AppConfig::default();
        config.proxy.providers.push(crate::provider::Provider {
            name: "test".into(),
            url: "https://a.com".into(),
            token: None,
            proxy: None,
            codex_url: None,
            protocols: vec![],
            account: None,
            models_url: None,
            models_kind: None,
        });
        config.proxy.providers.push(crate::provider::Provider {
            name: "test".into(),
            url: "https://b.com".into(),
            token: None,
            proxy: None,
            codex_url: None,
            protocols: vec![],
            account: None,
            models_url: None,
            models_kind: None,
        });
        let errors = config.validate();
        assert!(!errors.is_empty());
    }

    #[test]
    fn bad_price_length_detected() {
        let mut config = AppConfig::default();
        config.model_pricing.push(crate::pricing::ModelPricing {
            id: "test".into(),
            price: vec![1.0],
            providers: std::collections::HashMap::new(),
        });
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("price must have")));
    }

    fn provider_with_account(
        name: &str,
        account: &str,
        token: Option<&str>,
    ) -> crate::provider::Provider {
        crate::provider::Provider {
            name: name.into(),
            url: "https://chatgpt.com/backend-api/codex".into(),
            codex_url: None,
            token: token.map(String::from),
            proxy: None,
            protocols: vec!["codex".into()],
            account: Some(account.into()),
            models_url: None,
            models_kind: None,
        }
    }

    fn gpt_plan_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            refresh_token: Some("rt-1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn account_reference_must_exist() {
        let mut config = AppConfig::default();
        config
            .proxy
            .providers
            .push(provider_with_account("p", "ghost", None));
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("unknown account 'ghost'")),
            "{errors:?}"
        );
    }

    #[test]
    fn plan_account_without_credentials_is_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "work".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            ..Default::default()
        });
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("requires one of")),
            "{errors:?}"
        );
    }

    /// The point of this change: every family × mode combination is valid.
    #[test]
    fn all_four_family_mode_combinations_validate() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(gpt_plan_account("gpt-plan"));
        config.proxy.accounts.push(AccountConfig {
            name: "gpt-key".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::ApiKey,
            api_key: Some("sk-gpt".into()),
            ..Default::default()
        });
        config.proxy.accounts.push(AccountConfig {
            name: "claude-plan".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            refresh_token: Some("sk-ant-ort01-x".into()),
            ..Default::default()
        });
        config.proxy.accounts.push(AccountConfig {
            name: "claude-key".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::ApiKey,
            api_key: Some("sk-ant-api03-x".into()),
            ..Default::default()
        });
        assert_eq!(config.validate(), Vec::<String>::new());
    }

    #[test]
    fn claude_plan_rejects_a_codex_auth_json() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "claude".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            auth_json: Some("~/.codex/auth.json".into()),
            ..Default::default()
        });
        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("not supported for family = claude")),
            "{errors:?}"
        );
    }

    #[test]
    fn token_and_account_are_mutually_exclusive() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(gpt_plan_account("work"));
        config
            .proxy
            .providers
            .push(provider_with_account("p", "work", Some("sk-also-set")));
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("both token and account")),
            "{errors:?}"
        );
    }

    #[test]
    fn duplicate_account_names_are_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(gpt_plan_account("dup"));
        config.proxy.accounts.push(gpt_plan_account("dup"));
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("duplicate account names")),
            "{errors:?}"
        );
    }

    #[test]
    fn claude_persist_is_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "claude".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            refresh_token: Some("sk-ant-ort01-x".into()),
            persist: true,
            ..Default::default()
        });
        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("persist is only supported")),
            "{errors:?}"
        );
    }

    // ── rules added by the review ──

    #[test]
    fn unknown_protocol_names_are_rejected() {
        let mut config = AppConfig::default();
        config.proxy.providers.push(Provider {
            name: "p".into(),
            url: "https://x".into(),
            token: None,
            proxy: None,
            protocols: vec!["grpc".into()],
            codex_url: None,
            account: None,
            models_url: None,
            models_kind: None,
        });
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("unknown protocol 'grpc'")),
            "{errors:?}"
        );
    }

    #[test]
    fn mismatched_account_family_is_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "claude-key".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::ApiKey,
            api_key: Some("sk-ant-api03-x".into()),
            ..Default::default()
        });
        config
            .proxy
            .providers
            .push(provider_with_account("p", "claude-key", None));
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("is a claude account")),
            "{errors:?}"
        );
    }

    #[test]
    fn unknown_identity_and_impersonate_are_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "work".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            refresh_token: Some("rt".into()),
            identity: Some("codex-tui-2".into()),
            impersonate: Some("chrom".into()),
            ..Default::default()
        });
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("unknown identity")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("unknown impersonate")),
            "{errors:?}"
        );
    }

    #[test]
    fn cors_origins_must_be_origins() {
        fn errors_for(origins: &[&str]) -> Vec<String> {
            let mut config = AppConfig::default();
            config.server.cors_origins = origins.iter().map(|o| o.to_string()).collect();
            config.validate()
        }

        for good in ["*", "http://192.168.31.10:3000", "https://ui.example.com"] {
            assert!(errors_for(&[good]).is_empty(), "{good}");
        }
        // A trailing slash is what people paste; browsers never send one.
        assert!(errors_for(&["http://192.168.31.10:3000/"]).is_empty());

        for bad in [
            "192.168.31.10:3000",
            "http://host/path",
            "http://",
            "ftp://host",
            "",
        ] {
            let errors = errors_for(&[bad]);
            assert!(
                errors.iter().any(|e| e.contains("cors_origins")),
                "{bad}: {errors:?}"
            );
        }

        let errors = errors_for(&["192.168.31.10:3000"]);
        assert!(
            errors.iter().any(|e| e.contains("must include the scheme")),
            "the common mistake should say what is missing: {errors:?}"
        );
    }

    #[test]
    fn a_malformed_cli_version_is_rejected_but_a_real_one_is_not() {
        fn errors_for(version: &str) -> Vec<String> {
            let mut config = AppConfig::default();
            config.proxy.accounts.push(AccountConfig {
                name: "work".into(),
                family: AccountFamily::Gpt,
                mode: AccountMode::Plan,
                refresh_token: Some("rt".into()),
                cli_version: Some(version.into()),
                ..Default::default()
            });
            config.validate()
        }

        let errors = errors_for("not a version");
        assert!(
            errors.iter().any(|e| e.contains("cli_version")),
            "{errors:?}"
        );

        for ok in ["0.159.2", "1.0.0-rc1", "0.160"] {
            let errors = errors_for(ok);
            assert!(
                !errors.iter().any(|e| e.contains("cli_version")),
                "{ok}: {errors:?}"
            );
        }
    }

    #[test]
    fn cross_family_identity_is_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "c".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            refresh_token: Some("sk-ant-ort01-x".into()),
            identity: Some("codex_tui".into()),
            ..Default::default()
        });
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("other family")),
            "{errors:?}"
        );
    }

    #[test]
    fn padded_names_are_rejected() {
        let mut config = AppConfig::default();
        config.proxy.accounts.push(gpt_plan_account(" work"));
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("whitespace")),
            "{errors:?}"
        );
    }

    #[test]
    fn listen_address_must_be_an_ip_literal() {
        let mut config = AppConfig::default();
        for good in ["127.0.0.1", "0.0.0.0", "::1", "::", "192.168.1.5"] {
            config.server.listen_address = good.into();
            assert!(
                config.validate().is_empty(),
                "'{good}' should be accepted: {:?}",
                config.validate()
            );
        }
        // Not resolved, so it must be rejected here rather than at bind time.
        for bad in ["localhost", "example.com", "127.0.0.1:5000", ""] {
            config.server.listen_address = bad.into();
            assert!(
                config
                    .validate()
                    .iter()
                    .any(|error| error.contains("listen_address")),
                "'{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn the_two_listen_ports_must_differ() {
        let mut config = AppConfig::default();
        config.server.proxy_port = config.server.http_port;
        assert!(
            config
                .validate()
                .iter()
                .any(|error| error.contains("must differ")),
            "{:?}",
            config.validate()
        );
    }

    #[test]
    fn port_zero_is_rejected() {
        let mut config = AppConfig::default();
        config.server.http_port = 0;
        assert!(
            config
                .validate()
                .iter()
                .any(|error| error.contains("must be 1-65535")),
            "{:?}",
            config.validate()
        );
    }

    #[test]
    fn empty_provider_name_is_rejected() {
        let mut config = AppConfig::default();
        config.proxy.providers.push(Provider {
            name: "  ".into(),
            url: "https://x".into(),
            token: None,
            proxy: None,
            protocols: vec![],
            codex_url: None,
            account: None,
            models_url: None,
            models_kind: None,
        });
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("empty name")),
            "{errors:?}"
        );
    }
}
