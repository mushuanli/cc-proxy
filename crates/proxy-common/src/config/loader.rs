use std::path::Path;

use crate::config::AppConfig;
use crate::error::{ConfigError, ConfigResult};

/// Load AppConfig from a TOML file, falling back to defaults if the file is missing.
pub async fn load_config(path: &Path) -> ConfigResult<AppConfig> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => {
            let mut config: AppConfig = toml::from_str(&content)?;
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
