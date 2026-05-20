use crate::config::ProxyConfig;

impl ProxyConfig {
    /// Self-healing migration: fix active_upstream if it doesn't match any existing upstream.
    pub fn migrate(&mut self) {
        if !self.upstreams.iter().any(|u| u.name == self.active_upstream) {
            self.active_upstream = self
                .upstreams
                .first()
                .map(|u| u.name.clone())
                .unwrap_or_default();
        }
    }
}
