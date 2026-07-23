use serde::{Deserialize, Serialize};

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
                active_proxy_upstream: default_proxy_upstream(),
                active_effort: String::new(),
                http_proxy: None,
                providers: Vec::new(),
                upstreams: Vec::new(),
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
                mcp_proxy_port: 9999,
                auth_token: None,
                mcp_destination: None,
                ws_include_bodies: false,
            },
            logging: LoggingConfig {
                level: "info".into(),
            },
        }
    }
}

/// Proxy behavior settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub active_upstream: String,
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
    #[serde(default = "default_mcp_proxy_port")]
    pub mcp_proxy_port: u16,
    /// Auth token for Dashboard/WebSocket. Required when listen_address is not loopback.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// MCP forwarding destination (persisted across restarts).
    #[serde(default)]
    pub mcp_destination: Option<String>,
    /// Include prompt/response bodies in WebSocket events (off by default).
    #[serde(default)]
    pub ws_include_bodies: bool,
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
fn default_mcp_proxy_port() -> u16 {
    9999
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
pub use super::upstream::{TierRule, UpstreamConfig};
