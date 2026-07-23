use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

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
    /// Serializes the complete clone → validate → persist → swap transaction.
    update_lock: Arc<Mutex<()>>,
}

impl ConfigStore {
    /// Open the config store, loading from the given path.
    /// Creates a default config if the file does not exist.
    pub async fn open(path: impl Into<PathBuf>) -> ConfigResult<Self> {
        let path: PathBuf = path.into();
        let config = load_config(&path).await?;
        validate_config(&config)?;
        Ok(Self {
            path,
            config: Arc::new(RwLock::new(config)),
            update_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Get a snapshot of the current config.
    pub async fn get(&self) -> AppConfig {
        self.config.read().await.clone()
    }

    /// Reload config from disk.
    pub async fn reload(&self) -> ConfigResult<AppConfig> {
        let _update_guard = self.update_lock.lock().await;
        let config = load_config(&self.path).await?;
        validate_config(&config)?;
        let mut guard = self.config.write().await;
        *guard = config.clone();
        Ok(config)
    }

    /// Atomically update the in-memory config and persist to disk.
    /// Uses clone-validate-persist-swap pattern to prevent partial mutations.
    pub async fn update<F>(&self, updater: F) -> ConfigResult<AppConfig>
    where
        F: FnOnce(&mut AppConfig) -> ConfigResult<()>,
    {
        let _update_guard = self.update_lock.lock().await;

        // 1. Clone current config under read lock
        let mut candidate = self.config.read().await.clone();

        // 2. Apply mutation to the clone
        updater(&mut candidate)?;

        // 3. Validate the clone (not the live config)
        validate_config(&candidate)?;

        // 4. Persist clone to disk atomically
        persist_config(&self.path, &candidate).await?;

        // 5. Swap in-memory state only after successful persist
        *self.config.write().await = candidate.clone();

        Ok(candidate)
    }

    /// Persist current config to disk without updating.
    pub async fn persist(&self) -> ConfigResult<()> {
        let _update_guard = self.update_lock.lock().await;
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

fn validate_config(config: &AppConfig) -> ConfigResult<()> {
    let errors = config.validate();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(crate::error::ConfigError::Validation(errors.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cc-proxy-config-{label}-{}.toml",
            ulid::Ulid::new()
        ))
    }

    #[tokio::test]
    async fn validation_failure_does_not_mutate_live_config() {
        let path = temp_config_path("rollback");
        let store = ConfigStore::open(&path).await.unwrap();
        let before = store.get().await;
        let result = store
            .update(|candidate| {
                candidate.proxy.active_effort = "definitely-invalid".into();
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            store.get().await.proxy.active_effort,
            before.proxy.active_effort
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn concurrent_updates_are_serialized_without_lost_fields() {
        let path = temp_config_path("concurrent");
        let store = ConfigStore::open(&path).await.unwrap();
        let a = store.clone();
        let b = store.clone();
        let (ra, rb) = tokio::join!(
            a.update(|candidate| {
                candidate.logging.level = "debug".into();
                Ok(())
            }),
            b.update(|candidate| {
                candidate.server.http_port = 54321;
                Ok(())
            })
        );
        ra.unwrap();
        rb.unwrap();
        let live = store.get().await;
        assert_eq!(live.logging.level, "debug");
        assert_eq!(live.server.http_port, 54321);
        let disk = load_config(&path).await.unwrap();
        assert_eq!(disk.logging.level, "debug");
        assert_eq!(disk.server.http_port, 54321);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn persistence_failure_does_not_swap_memory() {
        let missing_parent =
            std::env::temp_dir().join(format!("cc-proxy-missing-parent-{}", ulid::Ulid::new()));
        let path = missing_parent.join("config.toml");
        let store = ConfigStore::open(&path).await.unwrap();
        let result = store
            .update(|candidate| {
                candidate.logging.level = "trace".into();
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert_ne!(store.get().await.logging.level, "trace");
    }
}
