use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::AppConfig;
use crate::error::ConfigResult;
use crate::loader::load_config;
use crate::persist::persist_config;
use crate::pricing::{BillingSnapshot, ResolvedRoute};
use crate::routing;

/// Thread-safe configuration store with hot-reload support.
#[derive(Clone)]
pub struct ConfigStore {
    path: PathBuf,
    config: Arc<RwLock<AppConfig>>,
}

impl ConfigStore {
    /// Open the config store, loading from the given path.
    /// Creates a default config if the file does not exist.
    pub async fn open(path: impl Into<PathBuf>) -> ConfigResult<Self> {
        let path: PathBuf = path.into();
        let config = load_config(&path).await?;
        Ok(Self {
            path,
            config: Arc::new(RwLock::new(config)),
        })
    }

    /// Get a snapshot of the current config.
    pub async fn get(&self) -> AppConfig {
        self.config.read().await.clone()
    }

    /// Reload config from disk.
    pub async fn reload(&self) -> ConfigResult<AppConfig> {
        let config = load_config(&self.path).await?;
        let mut guard = self.config.write().await;
        *guard = config.clone();
        Ok(config)
    }

    /// Atomically update the in-memory config and persist to disk.
    pub async fn update<F>(&self, updater: F) -> ConfigResult<AppConfig>
    where
        F: FnOnce(&mut AppConfig) -> ConfigResult<()>,
    {
        let mut guard = self.config.write().await;
        updater(&mut guard)?;

        // Validate before persisting
        let errors = guard.validate();
        if !errors.is_empty() {
            return Err(crate::error::ConfigError::Validation(errors.join("; ")));
        }

        drop(guard);

        // Persist to disk
        let config = self.get().await;
        persist_config(&self.path, &config).await?;

        Ok(config)
    }

    /// Persist current config to disk without updating.
    pub async fn persist(&self) -> ConfigResult<()> {
        let config = self.get().await;
        persist_config(&self.path, &config).await
    }

    /// Resolve route for a request model.
    pub async fn resolve_route(&self, request_model: &str) -> ConfigResult<ResolvedRoute> {
        let config = self.config.read().await;
        routing::resolve_route(&config, request_model)
    }

    /// Resolve a request model against a specific upstream.
    pub async fn resolve_route_for(
        &self,
        upstream_name: &str,
        request_model: &str,
    ) -> ConfigResult<ResolvedRoute> {
        let config = self.config.read().await;
        routing::resolve_route_for(&config, upstream_name, request_model)
    }

    /// Resolve billing snapshot for a provider and model.
    pub async fn resolve_billing(
        &self,
        provider: &str,
        model: &str,
    ) -> ConfigResult<BillingSnapshot> {
        let config = self.config.read().await;
        routing::resolve_billing(&config, provider, model)
    }
}
