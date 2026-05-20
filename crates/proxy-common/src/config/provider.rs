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
}
