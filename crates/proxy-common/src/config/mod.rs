#[allow(clippy::module_inception)]
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod loader;
pub(crate) mod migration;
pub(crate) mod persist;
pub(crate) mod pricing;
pub(crate) mod provider;
pub(crate) mod routing;
pub(crate) mod store;
pub(crate) mod upstream;
pub(crate) mod validation;

// Only re-export what external callers actually use.
// Internal types (AppConfig, BillingSnapshot, etc.) stay accessible
// via crate::config::X but are not visible outside proxy-common.
pub(crate) use config::{AppConfig, ProxyConfig};
pub use config::{AUTO_PROXY_UPSTREAM, FORBID_PROXY_UPSTREAM};
pub use error::ConfigError;
pub use pricing::{ModelPricing, ResolvedRoute};
pub use provider::Provider;
pub use store::ConfigStore;
pub use upstream::{TierRule, UpstreamConfig};
