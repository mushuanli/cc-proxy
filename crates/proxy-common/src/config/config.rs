use serde::{Deserialize, Serialize};

use super::account::AccountConfig;

pub const AUTO_PROXY_UPSTREAM: &str = "__auto__";
pub const FORBID_PROXY_UPSTREAM: &str = "__forbid__";

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_pricing: Vec<ModelPricing>,
    pub proxy: ProxyConfig,
    pub server: ServerConfig,
    pub logging: LoggingConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model_pricing: Vec::new(),
            proxy: ProxyConfig {
                active_upstream: String::new(),
                active_plan: String::new(),
                active_proxy_upstream: default_proxy_upstream(),
                active_effort: String::new(),
                http_proxy: None,
                providers: Vec::new(),
                upstreams: Vec::new(),
                accounts: Vec::new(),
                retry_count: 3,
                request_timeout_secs: 120,
                request_retention_hours: 8,
                session_max_count: 20,
                session_delete_after_days: 0,
            },
            server: ServerConfig {
                listen_address: "127.0.0.1".into(),
                http_port: 5000,
                proxy_port: 8888,
                auth_token: None,
                ws_include_bodies: false,
                cors_origins: Vec::new(),
            },
            logging: LoggingConfig {
                level: "info".into(),
            },
        }
    }
}

impl AppConfig {
    /// Model ids a client may ask for.
    ///
    /// Deliberately the *client-facing* list, not the upstream's own manifest: a
    /// requested id is matched against `model_pricing`, routed by tier and
    /// possibly bridged to another protocol, so what a client can actually use is
    /// what the configuration declares. The upstream manifest is a separate,
    /// admin-facing view (the dashboard fetches it on demand).
    ///
    /// Falls back to the models the tier rules name when no pricing matrix is
    /// configured, so a minimal config still answers.
    pub fn declared_models(&self) -> Vec<String> {
        fn push(out: &mut Vec<String>, raw: &str) {
            let id = raw.trim();
            if !id.is_empty() && !out.iter().any(|existing| existing == id) {
                out.push(id.to_string());
            }
        }

        let mut out = Vec::new();
        for entry in &self.model_pricing {
            push(&mut out, &entry.id);
        }
        if out.is_empty() {
            for upstream in &self.proxy.upstreams {
                let rules = [
                    upstream.high.as_ref(),
                    upstream.mid.as_ref(),
                    upstream.low.as_ref(),
                    upstream.default.as_ref(),
                ];
                for rule in rules.into_iter().flatten() {
                    push(&mut out, &rule.model);
                }
            }
        }
        out
    }
}

/// Where a request is sent: one of the relay's three target kinds.
///
/// `Upstream` and `Plan` are peers — both name *which upstream to talk to* — but
/// they resolve differently: an upstream goes through provider/tier routing (and
/// model translation), while a plan is a direct subscription connection where the
/// client's own model name is used as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayTarget<'a> {
    /// Transparent-proxy traffic, routed by the request URL or a named upstream.
    Proxy(&'a str),
    /// A subscription account's own endpoint; bypasses upstreams and tiers.
    Plan(&'a str),
    /// An ordinary upstream: provider + tier routing.
    Upstream(&'a str),
}

impl<'a> RelayTarget<'a> {
    /// The configured name this target points at.
    pub fn name(&self) -> &'a str {
        match self {
            RelayTarget::Proxy(name) | RelayTarget::Plan(name) | RelayTarget::Upstream(name) => {
                name
            }
        }
    }

    /// Whether this target bypasses provider/tier routing.
    pub fn is_plan(&self) -> bool {
        matches!(self, RelayTarget::Plan(_))
    }
}

impl ProxyConfig {
    /// Which target serves this request.
    ///
    /// Precedence, most specific first:
    /// 1. transparent-proxy traffic → `active_proxy_upstream`;
    /// 2. **a plan connection** (`active_plan` set) → that account's endpoint, for
    ///    every client protocol. The plan is deliberately consulted *before* the
    ///    upstream: a plan reached from an Anthropic client (Claude Code, via the
    ///    bridge) must use it, which a protocol-scoped slot could not express;
    /// 3. otherwise → `active_upstream`.
    pub fn select_target(&self, is_transparent: bool) -> RelayTarget<'_> {
        if is_transparent && !self.active_proxy_upstream.is_empty() {
            return RelayTarget::Proxy(&self.active_proxy_upstream);
        }
        if !self.active_plan.is_empty() {
            return RelayTarget::Plan(&self.active_plan);
        }
        RelayTarget::Upstream(&self.active_upstream)
    }
}

/// Proxy behavior settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub active_upstream: String,
    /// Active **plan connection**, named after a `[[proxy.accounts]]` entry.
    ///
    /// A plan is a peer of an upstream — both answer "which upstream do we talk
    /// to" — but it is defined by a *subscription account* rather than by
    /// providers and tiers: every request goes to that account's vendor endpoint,
    /// and no tier routing or model translation happens, so a client can ask for
    /// exactly the models `GET /v1/models` advertises for the plan.
    ///
    /// Empty (the default) = no plan connection, and every existing configuration
    /// behaves exactly as before.
    #[serde(default)]
    pub active_plan: String,
    #[serde(default = "default_proxy_upstream")]
    pub active_proxy_upstream: String,
    #[serde(default)]
    pub active_effort: String,

    /// Optional global HTTP proxy. All providers inherit this unless overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,

    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
    /// Upstream accounts usable as credentials, in either api-key or plan mode.
    /// Referenced by name from `Provider::account`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountConfig>,

    #[serde(default = "default_retry_count")]
    pub retry_count: u32,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,

    #[serde(default = "default_request_retention_hours")]
    pub request_retention_hours: u32,
    #[serde(default = "default_max_sessions")]
    pub session_max_count: u32,
    #[serde(default)]
    pub session_delete_after_days: u32,
}

fn default_proxy_upstream() -> String {
    FORBID_PROXY_UPSTREAM.into()
}

fn default_retry_count() -> u32 {
    3
}
fn default_request_timeout_secs() -> u64 {
    120
}
fn default_request_retention_hours() -> u32 {
    8
}
fn default_max_sessions() -> u32 {
    20
}

/// Network bind settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_address: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// Auth token for Dashboard/WebSocket. Required when listen_address is not loopback.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Include prompt/response bodies in WebSocket events (off by default).
    #[serde(default)]
    pub ws_include_bodies: bool,
    /// Browser origins allowed to call the proxy port, e.g.
    /// `["http://192.168.31.10:3000"]`; `["*"]` allows any.
    ///
    /// Empty (the default) sends no CORS headers at all. That is the safe
    /// default because the proxy port is unauthenticated: allowing `*` lets any
    /// page the operator visits spend their upstream quota.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cors_origins: Vec<String>,
}

fn default_listen_addr() -> String {
    "127.0.0.1".into()
}
fn default_http_port() -> u16 {
    5000
}
fn default_proxy_port() -> u16 {
    8888
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".into()
}

// Re-export types that are in their own modules for convenience
pub use super::pricing::ModelPricing;
pub use super::provider::Provider;
pub use super::upstream::UpstreamConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountConfig, AccountFamily, AccountMode, TierRule};

    fn upstream_with_tiers() -> UpstreamConfig {
        UpstreamConfig {
            name: "u".into(),
            high: Some(TierRule {
                provider: "p".into(),
                model: "gpt-6-sol".into(),
            }),
            mid: None,
            low: Some(TierRule {
                provider: "p".into(),
                model: "gpt-6-luna".into(),
            }),
            default: Some(TierRule {
                provider: "p".into(),
                model: "gpt-6-sol".into(),
            }),
            effort: None,
        }
    }

    #[test]
    fn declared_models_prefers_the_pricing_matrix() {
        let mut config = AppConfig::default();
        config.proxy.upstreams.push(upstream_with_tiers());
        config.model_pricing.push(ModelPricing {
            id: "claude-opus-4-1".into(),
            price: Vec::new(),
            providers: Default::default(),
        });
        config.model_pricing.push(ModelPricing {
            id: "  claude-sonnet-4-5  ".into(),
            price: Vec::new(),
            providers: Default::default(),
        });
        // Duplicates and blanks never show up twice (or at all).
        config.model_pricing.push(ModelPricing {
            id: "claude-opus-4-1".into(),
            price: Vec::new(),
            providers: Default::default(),
        });
        config.model_pricing.push(ModelPricing {
            id: "   ".into(),
            price: Vec::new(),
            providers: Default::default(),
        });

        assert_eq!(
            config.declared_models(),
            vec![
                "claude-opus-4-1".to_string(),
                "claude-sonnet-4-5".to_string()
            ]
        );
    }

    #[test]
    fn declared_models_falls_back_to_the_tier_rules() {
        let mut config = AppConfig::default();
        config.proxy.upstreams.push(upstream_with_tiers());
        // No pricing matrix: the models the tiers name, deduped, in tier order.
        assert_eq!(
            config.declared_models(),
            vec!["gpt-6-sol".to_string(), "gpt-6-luna".to_string()]
        );
    }

    #[test]
    fn declared_models_is_empty_when_nothing_is_configured() {
        assert!(AppConfig::default().declared_models().is_empty());
    }

    fn target_config(plan: &str, upstream: &str) -> ProxyConfig {
        let mut config = AppConfig::default();
        config.proxy.active_plan = plan.into();
        config.proxy.active_upstream = upstream.into();
        config.proxy.active_proxy_upstream = AUTO_PROXY_UPSTREAM.into();
        config.proxy
    }

    #[test]
    fn a_plan_connection_serves_every_client_protocol() {
        // The point of a plan target: one subscription serves everything, so the
        // client's protocol never decides whether the plan is used.
        let config = target_config("gpt-sub", "claude-pool");

        let target = config.select_target(false);
        assert_eq!(target, RelayTarget::Plan("gpt-sub"));
        assert!(target.is_plan());
        assert_eq!(target.name(), "gpt-sub");

        // Transparent traffic is still the proxy slot's business.
        assert_eq!(config.select_target(true), RelayTarget::Proxy("__auto__"));

        // No plan configured: the upstream, whatever the client speaks.
        let config = target_config("", "claude-pool");
        assert_eq!(
            config.select_target(false),
            RelayTarget::Upstream("claude-pool")
        );
    }

    #[test]
    fn a_plan_must_name_an_account_not_an_upstream() {
        let mut config = AppConfig::default();
        config.proxy.active_plan = "not-an-account".into();
        let errors = config.validate();
        assert!(
            errors.iter().any(|e| e.contains("active_plan")),
            "{errors:?}"
        );
    }

    #[test]
    fn accounts_are_not_models() {
        // Guard against a future refactor conflating the two lists: the model
        // list is about routable ids, never about credentials.
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "work".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            refresh_token: Some("rt".into()),
            ..Default::default()
        });
        assert!(config.declared_models().is_empty());
    }
}
