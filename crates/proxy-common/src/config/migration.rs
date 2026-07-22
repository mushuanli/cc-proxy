use crate::config::ProxyConfig;

impl ProxyConfig {
    /// Self-healing migration: fix active_upstream if it doesn't match any existing upstream.
    pub fn migrate(&mut self) {
        if !self
            .upstreams
            .iter()
            .any(|u| u.name == self.active_upstream)
        {
            self.active_upstream = self
                .upstreams
                .first()
                .map(|u| u.name.clone())
                .unwrap_or_default();
        }
        if self.active_proxy_upstream != "__auto__"
            && !self
                .upstreams
                .iter()
                .any(|u| u.name == self.active_proxy_upstream)
        {
            self.active_proxy_upstream = self.active_upstream.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{AppConfig, UpstreamConfig};

    #[test]
    fn legacy_config_inherits_relay_upstream_for_proxy() {
        let mut config = AppConfig::default();
        config.proxy.upstreams.push(UpstreamConfig {
            name: "default".into(),
            high: None,
            mid: None,
            low: None,
            default: None,
            effort: None,
        });
        config.proxy.active_upstream = "default".into();
        config.proxy.migrate();
        assert_eq!(config.proxy.active_proxy_upstream, "default");
    }

    #[test]
    fn auto_proxy_upstream_survives_migration() {
        let mut config = AppConfig::default();
        config.proxy.upstreams.push(UpstreamConfig {
            name: "default".into(),
            high: None,
            mid: None,
            low: None,
            default: None,
            effort: None,
        });
        config.proxy.active_upstream = "default".into();
        config.proxy.active_proxy_upstream = "__auto__".into();

        config.proxy.migrate();

        assert_eq!(config.proxy.active_proxy_upstream, "__auto__");
    }
}
