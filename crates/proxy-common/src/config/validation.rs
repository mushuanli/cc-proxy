use crate::config::AppConfig;

impl AppConfig {
    /// Validate the configuration, returning a list of human-readable errors.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // 1. active_upstream must exist
        if !self.proxy.active_upstream.is_empty()
            && !self
                .proxy
                .upstreams
                .iter()
                .any(|u| u.name == self.proxy.active_upstream)
        {
            errors.push(format!(
                "active_upstream '{}' not found in upstreams",
                self.proxy.active_upstream
            ));
        }
        if !self.proxy.active_proxy_upstream.is_empty()
            && self.proxy.active_proxy_upstream != "__auto__"
            && !self
                .proxy
                .upstreams
                .iter()
                .any(|u| u.name == self.proxy.active_proxy_upstream)
        {
            errors.push(format!(
                "active_proxy_upstream '{}' not found in upstreams",
                self.proxy.active_proxy_upstream
            ));
        }

        // 2. Validate each upstream's tier rules
        for upstream in &self.proxy.upstreams {
            let rules = [
                ("high", &upstream.high),
                ("mid", &upstream.mid),
                ("low", &upstream.low),
                ("default", &upstream.default),
            ];
            for (tier, rule_opt) in rules {
                let Some(rule) = rule_opt else { continue };
                if rule.provider.is_empty() {
                    continue;
                }

                // Provider must exist
                if !self.proxy.providers.iter().any(|p| p.name == rule.provider) {
                    errors.push(format!(
                        "upstream '{}' {tier}: provider '{}' not found",
                        upstream.name, rule.provider
                    ));
                    continue;
                }

                // If model is a logical id, it must have a provider mapping
                if rule.model.is_empty() {
                    continue;
                }
                if let Some(mp) = self.model_pricing.iter().find(|mp| mp.id == rule.model) {
                    if !mp.providers.contains_key(&rule.provider) {
                        errors.push(format!(
                            "upstream '{}' {tier}: logical model '{}' has no mapping for provider '{}'",
                            upstream.name, rule.model, rule.provider
                        ));
                    }
                }
            }
        }

        // 3. Price array length must be 2 or 4
        for mp in &self.model_pricing {
            let len = mp.price.len();
            if len != 0 && len != 2 && len != 4 {
                errors.push(format!(
                    "model_pricing '{}': price must have 0, 2, or 4 elements, got {len}",
                    mp.id
                ));
            }
            // Prices must be >= 0
            for (i, &p) in mp.price.iter().enumerate() {
                if p < 0.0 {
                    errors.push(format!(
                        "model_pricing '{}': price[{}] must be >= 0, got {}",
                        mp.id, i, p
                    ));
                }
            }
        }

        // 4. Duplicate checks
        let provider_names: Vec<&str> = self
            .proxy
            .providers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        if has_duplicates(&provider_names) {
            errors.push("duplicate provider names found".to_string());
        }

        let upstream_names: Vec<&str> = self
            .proxy
            .upstreams
            .iter()
            .map(|u| u.name.as_str())
            .collect();
        if has_duplicates(&upstream_names) {
            errors.push("duplicate upstream names found".to_string());
        }

        let pricing_ids: Vec<&str> = self.model_pricing.iter().map(|mp| mp.id.as_str()).collect();
        if has_duplicates(&pricing_ids) {
            errors.push("duplicate model_pricing ids found".to_string());
        }

        // 5. Effort validation
        if !self.proxy.active_effort.is_empty() && self.proxy.active_effort != "auto" {
            let valid_efforts = ["low", "medium", "high", "xhigh", "max", "ultracode"];
            if !valid_efforts.contains(&self.proxy.active_effort.as_str()) {
                errors.push(format!(
                    "invalid active_effort '{}': must be one of: auto, {}",
                    self.proxy.active_effort,
                    valid_efforts.join(", ")
                ));
            }
        }

        // 6. Proxy URL validation
        if let Some(ref url) = self.proxy.http_proxy {
            if !url.is_empty() && !is_valid_proxy_url(url) {
                errors.push(format!(
                    "invalid global http_proxy '{}': must be http://, https://, or socks5:// URL",
                    url
                ));
            }
        }
        for p in &self.proxy.providers {
            if let Some(ref proxy) = p.proxy {
                if !proxy.is_empty() && !is_valid_proxy_url(proxy) {
                    errors.push(format!(
                        "provider '{}': invalid proxy '{}': must be http://, https://, or socks5:// URL",
                        p.name, proxy
                    ));
                }
            }
        }

        errors
    }
}

/// Check that a proxy URL starts with a valid scheme.
fn is_valid_proxy_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("socks5://")
}

fn has_duplicates<T: PartialEq>(items: &[T]) -> bool {
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if items[i] == items[j] {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_validates() {
        let config = AppConfig::default();
        assert!(config.validate().is_empty());
    }

    #[test]
    fn duplicate_provider_detected() {
        let mut config = AppConfig::default();
        config.proxy.providers.push(crate::provider::Provider {
            name: "test".into(),
            url: "https://a.com".into(),
            token: None,
            proxy: None,
        });
        config.proxy.providers.push(crate::provider::Provider {
            name: "test".into(),
            url: "https://b.com".into(),
            token: None,
            proxy: None,
        });
        let errors = config.validate();
        assert!(!errors.is_empty());
    }

    #[test]
    fn bad_price_length_detected() {
        let mut config = AppConfig::default();
        config.model_pricing.push(crate::pricing::ModelPricing {
            id: "test".into(),
            price: vec![1.0],
            providers: std::collections::HashMap::new(),
        });
        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("price must have")));
    }
}
