use std::path::Path;

use crate::config::AppConfig;
use crate::error::{ConfigError, ConfigResult};

/// Load AppConfig from a TOML file, falling back to defaults if the file is missing.
pub async fn load_config(path: &Path) -> ConfigResult<AppConfig> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => {
            let table: toml::Table = toml::from_str(&content)?;
            if table.contains_key("proxy") {
                return Err(ConfigError::Validation("[proxy] has been removed; put routing settings, providers, accounts and upstreams at the top level".into()));
            }
            let mut config: AppConfig = table.try_into()?;
            config.proxy.migrate();
            Ok(config)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("config file '{}' not found, using defaults", path.display());
            Ok(AppConfig::default())
        }
        Err(e) => Err(ConfigError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_missing_config_returns_defaults() {
        let config = load_config(Path::new("/nonexistent/config.toml")).await;
        assert!(config.is_ok());
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    #[tokio::test]
    async fn removed_proxy_wrapper_is_rejected_instead_of_silently_ignored() {
        let path =
            std::env::temp_dir().join(format!("cc-proxy-old-wrapper-{}.toml", ulid::Ulid::new()));
        tokio::fs::write(&path, "[proxy]\nactive_upstream = 'old'\n")
            .await
            .unwrap();
        let result = load_config(&path).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("[proxy] has been removed"));
        tokio::fs::remove_file(path).await.unwrap();
    }
}
