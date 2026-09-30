use crate::config::{ProxyConfig, AUTO_PROXY_UPSTREAM, FORBID_PROXY_UPSTREAM};

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
        if self.active_proxy_upstream != AUTO_PROXY_UPSTREAM
            && self.active_proxy_upstream != FORBID_PROXY_UPSTREAM
            && !self
                .upstreams
                .iter()
                .any(|u| u.name == self.active_proxy_upstream)
        {
            self.active_proxy_upstream = self.active_upstream.clone();
        }
        // The codex selector was not persisted for a long time, so a stale value
        // only became possible once it was written to disk. Repair it like the
        // others instead of failing startup validation or 502-ing every Codex
        // request. An empty value already means "use active_upstream".
        if !self.active_codex_upstream.is_empty()
            && !self
                .upstreams
                .iter()
                .any(|u| u.name == self.active_codex_upstream)
        {
            self.active_codex_upstream = self.active_upstream.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{AppConfig, UpstreamConfig};

    #[test]
    fn legacy_config_inherits_relay_upstream_for_proxy() {
        let mut config = AppConfig::default();
        config.proxy.active_proxy_upstream = String::new();
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
    fn a_dangling_codex_upstream_is_repaired() {
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
        config.proxy.active_codex_upstream = "ghost".into();

        config.proxy.migrate();

        assert_eq!(config.proxy.active_codex_upstream, "default");
        assert!(config.validate().is_empty());
    }

    #[test]
    fn an_empty_codex_upstream_is_left_alone() {
        // Empty already means "fall back to active_upstream" at request time.
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
        assert_eq!(config.proxy.active_codex_upstream, "");
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
        config.proxy.active_proxy_upstream = crate::AUTO_PROXY_UPSTREAM.into();

        config.proxy.migrate();

        assert_eq!(
            config.proxy.active_proxy_upstream,
            crate::AUTO_PROXY_UPSTREAM
        );
    }

    #[test]
    fn forbid_proxy_upstream_survives_migration() {
        let mut config = AppConfig::default();
        config.proxy.migrate();

        assert_eq!(
            config.proxy.active_proxy_upstream,
            crate::FORBID_PROXY_UPSTREAM
        );
    }
}
