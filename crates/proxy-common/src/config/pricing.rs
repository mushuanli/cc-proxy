use std::collections::HashMap;

use crate::models::PriceRates;
use serde::{Deserialize, Serialize};

/// Global pricing definition for a logical model family.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub price: Vec<f64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub providers: HashMap<String, Vec<String>>,
}

impl ModelPricing {
    pub fn price_input(&self) -> f64 {
        self.price.first().copied().unwrap_or(0.0)
    }

    pub fn price_output(&self) -> f64 {
        self.price.get(1).copied().unwrap_or(0.0)
    }

    pub fn price_cache_write(&self) -> f64 {
        self.price
            .get(2)
            .copied()
            .unwrap_or_else(|| self.price_input() * 1.25)
    }

    pub fn price_cache_read(&self) -> f64 {
        self.price
            .get(3)
            .copied()
            .unwrap_or_else(|| self.price_input() * 0.1)
    }

    /// Returns the first model name for this provider, or None if not supported.
    pub fn model_name_for_provider(&self, provider: &str) -> Option<String> {
        self.providers.get(provider).map(|names| {
            names
                .first()
                .map(|n| {
                    if n.is_empty() {
                        self.id.clone()
                    } else {
                        n.clone()
                    }
                })
                .unwrap_or_else(|| self.id.clone())
        })
    }

    /// Returns true if the given name matches the logical id or any provider model name.
    pub fn matches_name(&self, name: &str) -> bool {
        if self.id == name {
            return true;
        }
        self.providers
            .values()
            .any(|names| names.iter().any(|n| !n.is_empty() && n == name))
    }

    /// Convert the f64 prices to integer PriceRates (micro-USD / 1M tokens).
    pub fn to_price_rates(&self) -> PriceRates {
        let to_microusd = |v: f64| -> i64 { (v * 1_000_000.0).round() as i64 };
        PriceRates {
            input_microusd: to_microusd(self.price_input()),
            output_microusd: to_microusd(self.price_output()),
            cache_write_microusd: to_microusd(self.price_cache_write()),
            cache_read_microusd: to_microusd(self.price_cache_read()),
        }
    }
}

/// Result of route resolution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedRoute {
    pub upstream: String,
    pub provider: String,
    /// The logical model or provider model from the tier rule.
    pub configured_model: String,
    /// The actual model name sent to the provider.
    pub resolved_model: String,
    pub effort: Option<String>,
}

/// Billing snapshot frozen at task creation time.
///
/// Re-exported from proxy_core for convenience.
pub use crate::models::BillingSnapshot;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_opus() -> ModelPricing {
        let mut providers = HashMap::new();
        providers.insert("anthropic".into(), vec!["claude-opus-4-6".into()]);
        providers.insert("rdsec".into(), vec!["claude-4.6-opus".into()]);
        providers.insert("cloudapi".into(), vec![]);

        ModelPricing {
            id: "claude-opus".into(),
            price: vec![5.0, 25.0, 6.25, 0.5],
            providers,
        }
    }

    #[test]
    fn model_name_for_provider_works() {
        let mp = make_opus();
        assert_eq!(
            mp.model_name_for_provider("anthropic"),
            Some("claude-opus-4-6".into())
        );
        assert_eq!(
            mp.model_name_for_provider("rdsec"),
            Some("claude-4.6-opus".into())
        );
        assert_eq!(
            mp.model_name_for_provider("cloudapi"),
            Some("claude-opus".into())
        );
        assert_eq!(mp.model_name_for_provider("deepseek"), None);
    }

    #[test]
    fn matches_name_works() {
        let mp = make_opus();
        assert!(mp.matches_name("claude-opus"));
        assert!(mp.matches_name("claude-4.6-opus"));
        assert!(!mp.matches_name("claude-sonnet"));
    }

    #[test]
    fn price_defaults_work() {
        let mp = ModelPricing {
            id: "m".into(),
            price: vec![4.0, 20.0],
            providers: HashMap::new(),
        };
        assert_eq!(mp.price_input(), 4.0);
        assert_eq!(mp.price_output(), 20.0);
        assert_eq!(mp.price_cache_write(), 5.0);
        assert_eq!(mp.price_cache_read(), 0.4);
    }

    #[test]
    fn to_price_rates_converts_to_microusd() {
        let mp = ModelPricing {
            id: "test".into(),
            price: vec![3.0, 15.0],
            providers: HashMap::new(),
        };
        let rates = mp.to_price_rates();
        assert_eq!(rates.input_microusd, 3_000_000);
        assert_eq!(rates.output_microusd, 15_000_000);
        assert_eq!(rates.cache_write_microusd, 3_750_000); // 3.0 * 1.25
        assert_eq!(rates.cache_read_microusd, 300_000); // 3.0 * 0.1
    }
}
