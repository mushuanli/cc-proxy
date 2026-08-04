use crate::config::AppConfig;
use crate::error::ConfigResult;
use crate::pricing::{BillingSnapshot, ResolvedRoute};
use crate::upstream::UpstreamConfig;

/// Resolve the route for a given request model.
///
/// Flow:
/// 1. Find active upstream by name
/// 2. Match tier rules: high → mid → low → default
/// 3. Translate logical model → provider model via ModelPricing
pub fn resolve_route(config: &AppConfig, request_model: &str) -> ConfigResult<ResolvedRoute> {
    resolve_route_for(config, &config.proxy.active_upstream, request_model)
}

/// Resolve a route using an explicitly selected upstream.
pub fn resolve_route_for(
    config: &AppConfig,
    upstream_name: &str,
    request_model: &str,
) -> ConfigResult<ResolvedRoute> {
    let upstream = config
        .proxy
        .upstreams
        .iter()
        .find(|u| u.name == upstream_name)
        .ok_or_else(|| {
            crate::error::ConfigError::NotFound(format!("upstream '{}' not found", upstream_name))
        })?;

    let (provider_name, configured_model) = resolve_tier(upstream, request_model);

    if provider_name.is_empty() {
        return Err(crate::error::ConfigError::NotFound(format!(
            "no route for model '{}' in upstream '{}'",
            request_model, upstream.name
        )));
    }

    // Translate logical model to provider-specific model name
    let resolved_model = translate_model(&config.model_pricing, &provider_name, &configured_model);

    // Use global active_effort as override; fall back to upstream config
    let effort = if !config.proxy.active_effort.is_empty() && config.proxy.active_effort != "auto" {
        Some(config.proxy.active_effort.clone())
    } else {
        upstream.effort.clone()
    };

    Ok(ResolvedRoute {
        upstream: upstream.name.clone(),
        provider: provider_name,
        configured_model,
        resolved_model,
        effort,
    })
}

/// Match the tier rules for a request model.
fn resolve_tier(upstream: &UpstreamConfig, request_model: &str) -> (String, String) {
    let lower = request_model.to_lowercase();
    let def = upstream.default.as_ref();

    for (rule, label) in [
        (&upstream.high, "opus"),
        (&upstream.mid, "sonnet"),
        (&upstream.low, "haiku"),
    ] {
        if let Some(ref r) = rule {
            if r.matches(&lower, label) {
                return (r.provider_or(def), r.model.clone());
            }
        }
    }

    if let Some(ref d) = upstream.default {
        return (d.provider.clone(), d.model.clone());
    }

    (String::new(), String::new())
}

/// Translate a logical model id to a provider-specific model name.
fn translate_model(
    model_pricing: &[crate::pricing::ModelPricing],
    provider: &str,
    model: &str,
) -> String {
    if model.is_empty() {
        return model.to_string();
    }

    // If model is a known logical id, use its provider mapping
    if let Some(mp) = model_pricing.iter().find(|mp| mp.id == model) {
        if let Some(name) = mp.model_name_for_provider(provider) {
            return name;
        }
    }

    // Pass-through: use the model string as-is
    model.to_string()
}

/// Resolve billing snapshot for a given provider and model.
pub fn resolve_billing(
    config: &AppConfig,
    provider: &str,
    model: &str,
) -> ConfigResult<BillingSnapshot> {
    // Find matching ModelPricing by model name (logical id or provider model name)
    let mp = config
        .model_pricing
        .iter()
        .find(|mp| mp.matches_name(model))
        .ok_or_else(|| {
            crate::error::ConfigError::NotFound(format!(
                "no pricing found for model '{}' (provider: '{}')",
                model, provider
            ))
        })?;

    let rates = mp.to_price_rates();

    Ok(BillingSnapshot {
        pricing_model_id: mp.id.clone(),
        provider: provider.to_string(),
        resolved_model: translate_model(&config.model_pricing, provider, model),
        rates,
        currency: "USD".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> AppConfig {
        let mut config = AppConfig::default();

        config.proxy.providers.push(crate::provider::Provider {
            name: "anthropic".into(),
            url: "https://api.anthropic.com".into(),
            token: Some("sk-test".into()),
            proxy: None,
            protocols: vec![],
        });

        config.proxy.upstreams.push(UpstreamConfig {
            name: "default".into(),
            high: Some(crate::upstream::TierRule {
                provider: "anthropic".into(),
                model: "claude-opus".into(),
            }),
            mid: None,
            low: None,
            default: Some(crate::upstream::TierRule {
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
            }),
            effort: None,
        });

        config.proxy.active_upstream = "default".into();

        let mut providers = std::collections::HashMap::new();
        providers.insert("anthropic".into(), vec!["claude-sonnet-4-6".into()]);
        config.model_pricing.push(crate::pricing::ModelPricing {
            id: "claude-sonnet".into(),
            price: vec![3.0, 15.0],
            providers,
        });

        config
    }

    #[test]
    fn resolve_route_high_tier() {
        let config = make_config();
        let route = resolve_route(&config, "claude-opus-4-6").unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.configured_model, "claude-opus");
    }

    #[test]
    fn resolve_route_default_tier() {
        let config = make_config();
        let route = resolve_route(&config, "claude-haiku").unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.configured_model, "claude-sonnet");
        assert_eq!(route.resolved_model, "claude-sonnet-4-6");
    }

    #[test]
    fn active_effort_overrides_upstream_and_auto_falls_back() {
        let mut config = make_config();
        config.proxy.upstreams[0].effort = Some("medium".into());
        config.proxy.active_effort = "high".into();
        assert_eq!(
            resolve_route(&config, "claude-haiku")
                .unwrap()
                .effort
                .as_deref(),
            Some("high")
        );
        config.proxy.active_effort = "auto".into();
        assert_eq!(
            resolve_route(&config, "claude-haiku")
                .unwrap()
                .effort
                .as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn resolve_route_for_explicit_upstream() {
        let mut config = make_config();
        config.proxy.upstreams.push(UpstreamConfig {
            name: "transparent".into(),
            high: None,
            mid: None,
            low: None,
            default: Some(crate::upstream::TierRule {
                provider: "anthropic".into(),
                model: "raw-model".into(),
            }),
            effort: None,
        });
        let route = resolve_route_for(&config, "transparent", "client-model").unwrap();
        assert_eq!(route.upstream, "transparent");
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.resolved_model, "raw-model");
    }

    #[test]
    fn transparent_upstream_can_route_without_configured_model() {
        let mut config = make_config();
        config.proxy.upstreams.push(UpstreamConfig {
            name: "transparent".into(),
            high: None,
            mid: None,
            low: None,
            default: Some(crate::upstream::TierRule {
                provider: "anthropic".into(),
                model: String::new(),
            }),
            effort: None,
        });
        let route = resolve_route_for(&config, "transparent", "wire-model").unwrap();
        assert_eq!(route.provider, "anthropic");
        assert!(route.resolved_model.is_empty());
    }

    #[test]
    fn resolve_billing_works() {
        let config = make_config();
        let billing = resolve_billing(&config, "anthropic", "claude-sonnet-4-6").unwrap();
        assert_eq!(billing.pricing_model_id, "claude-sonnet");
        assert_eq!(billing.rates.input_microusd, 3_000_000);
        assert_eq!(billing.rates.output_microusd, 15_000_000);
    }

    #[test]
    fn tier_inherits_provider_from_default() {
        let mut config = make_config();
        config.proxy.upstreams[0].high = Some(crate::upstream::TierRule {
            provider: String::new(), // inherit from default
            model: "claude-opus".into(),
        });
        let route = resolve_route(&config, "claude-opus-4-6").unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.configured_model, "claude-opus");
    }

    #[test]
    fn default_empty_model_passthrough() {
        let mut config = make_config();
        config.proxy.upstreams[0].default = Some(crate::upstream::TierRule {
            provider: "anthropic".into(),
            model: String::new(), // transparent: no model override
        });
        // Haiku doesn't match opus tier, falls through to default
        let route = resolve_route(&config, "claude-haiku").unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.configured_model, ""); // empty → passthrough
    }
}
