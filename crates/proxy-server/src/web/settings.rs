use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use proxy_common::{ConfigStore, ProviderInfo, UpstreamInfo, WsMessage};
use serde_json::json;

use crate::AppState;

/// Map an error message to the best HTTP status code.
fn err_response(msg: &str) -> axum::response::Response {
    let code = if msg.contains("not found") || msg.contains("NotFound") {
        StatusCode::NOT_FOUND
    } else if msg.contains("validation") || msg.contains("invalid") || msg.contains("Validation") {
        StatusCode::BAD_REQUEST
    } else if msg.contains("duplicate") || msg.contains("conflict") {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (code, Json(json!({"error": msg}))).into_response()
}

// ── Model Pricing ──

pub async fn list_pricing(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.model_pricing.len();
    tracing::info!("[api] list_pricing: {} entries", count);
    Json(json!(config.model_pricing)).into_response()
}

pub async fn add_pricing(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let result = state
        .config
        .update(move |c| {
            let mp: proxy_common::ModelPricing = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.model_pricing.push(mp);
            Ok(())
        })
        .await;
    match result {
        Ok(config) => {
            state.events.publish(upstream_changed(&state.config).await);
            let id = config
                .model_pricing
                .last()
                .map(|p| p.id.as_str())
                .unwrap_or("?");
            tracing::info!("[api] add_pricing: id={}", id);
            Json(json!({"ok": true, "model_pricing": config.model_pricing})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_pricing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_id = id.clone();
    // When a provider mapping is removed, upstream tier rules that reference it
    // would become dangling. The caller must resolve those references first:
    // action=remove → drop them; action=reassign → rewrite them to the target provider.
    let action = q.get("action").map(|s| s.as_str()).unwrap_or("");
    let target = q
        .get("target")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let result = state
        .config
        .update(move |c| {
            let mp: proxy_common::ModelPricing = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            let removed: Vec<String> = c
                .model_pricing
                .iter()
                .find(|p| p.id == id)
                .map(|old| {
                    old.providers
                        .keys()
                        .filter(|k| !mp.providers.contains_key(*k))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for prov in &removed {
                if !mapping_refs_exist(&c.proxy.upstreams, &id, prov) {
                    continue;
                }
                if action == "remove" {
                    clear_mapping_refs(&mut c.proxy.upstreams, &id, prov);
                } else if action == "reassign" {
                    if !mp.providers.contains_key(&target)
                        || !c.proxy.providers.iter().any(|p| p.name == target)
                    {
                        return Err(proxy_common::ConfigError::Validation(format!(
                            "target provider '{}' has no mapping for model '{}'",
                            target, id
                        )));
                    }
                    rewrite_mapping_refs(&mut c.proxy.upstreams, &id, prov, &target);
                } else {
                    return Err(proxy_common::ConfigError::Validation(format!(
                        "mapping '{}/{}' is referenced by upstreams; remove references first",
                        id, prov
                    )));
                }
            }
            c.model_pricing.retain(|p| p.id != id);
            c.model_pricing.push(mp);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_pricing: id={}", log_id);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

/// True if any upstream tier rule references the (model id, provider) mapping.
fn mapping_refs_exist(upstreams: &[proxy_common::UpstreamConfig], id: &str, prov: &str) -> bool {
    upstreams.iter().any(|u| {
        [&u.high, &u.mid, &u.low, &u.default].iter().any(|r| {
            r.as_ref()
                .map_or(false, |r| r.model == id && r.provider == prov)
        })
    })
}

/// Rewrite every upstream rule referencing (id, prov) to use `to` as provider.
fn rewrite_mapping_refs(
    upstreams: &mut [proxy_common::UpstreamConfig],
    id: &str,
    prov: &str,
    to: &str,
) {
    for u in upstreams {
        for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
            if let Some(r) = rule {
                if r.model == id && r.provider == prov {
                    r.provider = to.to_string();
                }
            }
        }
    }
}

/// Drop the provider reference in every rule matching (id, prov).
fn clear_mapping_refs(upstreams: &mut [proxy_common::UpstreamConfig], id: &str, prov: &str) {
    for u in upstreams {
        for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
            if let Some(r) = rule {
                if r.model == id && r.provider == prov {
                    r.provider.clear();
                }
            }
        }
    }
}

pub async fn delete_pricing(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let log_id = id.clone();
    // action=reassign → rewrite every upstream rule referencing this pricing id
    // to the target pricing id; action=remove → drop those rules and delete.
    let action = q.get("action").map(|s| s.as_str()).unwrap_or("");
    let target = q
        .get("target")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let result = state
        .config
        .update(move |c| {
            if action == "remove" {
                for u in &mut c.proxy.upstreams {
                    clear_pricing_rules(u, &id);
                }
            } else if action == "reassign" {
                if target.is_empty() {
                    return Err(proxy_common::ConfigError::Validation(
                        "reassign requires a target model pricing".into(),
                    ));
                }
                if !c.model_pricing.iter().any(|mp| mp.id == target) {
                    return Err(proxy_common::ConfigError::Validation(format!(
                        "target model pricing '{}' not found",
                        target
                    )));
                }
                for u in &mut c.proxy.upstreams {
                    rewrite_pricing_rules(u, &id, &target);
                }
            }
            c.model_pricing.retain(|p| p.id != id);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_pricing: id={}", log_id);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_pricing failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

/// List the upstream tier rules that reference a model pricing id.
pub async fn pricing_refs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let config = state.config.get().await;
    let mut upstreams: Vec<serde_json::Value> = Vec::new();
    for u in &config.proxy.upstreams {
        for (tier, rule) in [
            ("high", u.high.as_ref()),
            ("mid", u.mid.as_ref()),
            ("low", u.low.as_ref()),
            ("default", u.default.as_ref()),
        ] {
            if let Some(r) = rule {
                if r.model == id {
                    upstreams
                        .push(json!({"upstream": u.name, "tier": tier, "provider": r.provider}));
                }
            }
        }
    }
    Json(json!({"model_pricing": id, "upstreams": upstreams})).into_response()
}

/// Rewrite every upstream rule whose model references `from` to `to`.
fn rewrite_pricing_rules(u: &mut proxy_common::UpstreamConfig, from: &str, to: &str) {
    for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
        if let Some(r) = rule {
            if r.model == from {
                r.model = to.to_string();
            }
        }
    }
}

/// Drop every upstream rule referencing the pricing id (blank the model).
fn clear_pricing_rules(u: &mut proxy_common::UpstreamConfig, id: &str) {
    for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
        if let Some(r) = rule {
            if r.model == id {
                r.model.clear();
            }
        }
    }
}

// ── Providers ──

pub async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.proxy.providers.len();
    tracing::info!("[api] list_providers: {} entries", count);
    let infos: Vec<serde_json::Value> = config
        .proxy
        .providers
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "url": p.url,
                "has_token": p.token.is_some(),
                "proxy": p.proxy,
                "protocols": p.protocols,
                "codex_url": p.codex_url,
                "account": p.account,
                "models_url": p.models_url,
                "models_kind": p.models_kind,
                // What would be used if neither override is set, so the UI can
                // show the effective choice instead of an empty field.
                "catalog_kind": p
                    .catalog_kind(
                        p.account
                            .as_deref()
                            .and_then(|name| config.proxy.accounts.iter().find(|a| a.name == name))
                            .map(|a| a.family),
                    )
                    .as_str(),
            })
        })
        .collect();
    Json(json!(infos)).into_response()
}

pub async fn add_provider(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "name is required"})),
        )
            .into_response();
    }
    let url = body.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let codex_url = body
        .get("codex_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    let token = body.get("token").and_then(|v| v.as_str()).map(String::from);
    let provider_proxy = body.get("proxy").and_then(|v| v.as_str()).map(String::from);
    let protocols = body
        .get("protocols")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let account = body
        .get("account")
        .and_then(|v| v.as_str())
        .map(String::from);
    // Catalog overrides: absent = keep the inferred kind and derived URL.
    let models_url = body
        .get("models_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from);
    let models_kind = body
        .get("models_kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from);
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.providers.push(proxy_common::Provider {
                name,
                url: url.into(),
                codex_url,
                token,
                proxy: provider_proxy,
                protocols,
                account,
                models_url,
                models_kind,
            });
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] add_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            if let Some(p) = c.proxy.providers.iter_mut().find(|p| p.name == name) {
                if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
                    p.url = url.into();
                }
                if body.get("codex_url").is_some() {
                    p.codex_url = body
                        .get("codex_url")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
                if body.get("token").is_some() {
                    p.token = body.get("token").and_then(|v| v.as_str()).map(String::from);
                }
                if body.get("proxy").is_some() {
                    p.proxy = body.get("proxy").and_then(|v| v.as_str()).map(|s| {
                        if s.is_empty() {
                            String::new()
                        } else {
                            s.to_string()
                        }
                    });
                }
                if body.get("protocols").is_some() {
                    p.protocols = body
                        .get("protocols")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                if body.get("account").is_some() {
                    p.account = body
                        .get("account")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
                // Catalog overrides: present-but-blank clears them, which puts the
                // kind back to inference and the URL back to the derived default.
                if body.get("models_url").is_some() {
                    p.models_url = body
                        .get("models_url")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(String::from);
                }
                if body.get("models_kind").is_some() {
                    p.models_kind = body
                        .get("models_kind")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(String::from);
                }
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let log_name = name.clone();
    // action=reassign → rewrite all references to the target provider;
    // action=remove → drop all references (upstream tier rules + pricing keys) and delete the provider.
    let action = q.get("action").map(|s| s.as_str()).unwrap_or("");
    let target = q
        .get("target")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let result = state
        .config
        .update(move |c| {
            if action == "remove" {
                purge_provider(&mut c.model_pricing, &name);
                for u in &mut c.proxy.upstreams {
                    clear_provider_rules(u, &name);
                }
            } else if action == "reassign" {
                if target.is_empty() {
                    return Err(proxy_common::ConfigError::Validation(
                        "reassign requires a target provider".into(),
                    ));
                }
                if !c.proxy.providers.iter().any(|p| p.name == target) {
                    return Err(proxy_common::ConfigError::Validation(format!(
                        "target provider '{}' not found",
                        target
                    )));
                }
                rewrite_provider_rules(&mut c.model_pricing, &name, &target);
                for u in &mut c.proxy.upstreams {
                    rewrite_upstream_rules(u, &name, &target);
                }
            }
            c.proxy.providers.retain(|p| p.name != name);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_provider: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_provider failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

/// Remove references to `provider` from the config (upstream tier rules + pricing).
pub async fn provider_refs(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let config = state.config.get().await;
    let mut upstreams: Vec<serde_json::Value> = Vec::new();
    for u in &config.proxy.upstreams {
        for (tier, rule) in [
            ("high", u.high.as_ref()),
            ("mid", u.mid.as_ref()),
            ("low", u.low.as_ref()),
            ("default", u.default.as_ref()),
        ] {
            if let Some(r) = rule {
                if r.provider == name {
                    upstreams.push(json!({"upstream": u.name, "tier": tier}));
                }
            }
        }
    }
    let pricing: Vec<String> = config
        .model_pricing
        .iter()
        .filter(|mp| mp.providers.contains_key(&name))
        .map(|mp| mp.id.clone())
        .collect();
    Json(json!({"provider": name, "upstreams": upstreams, "model_pricing": pricing}))
        .into_response()
}

/// Rewrite every reference to `from` in upstream tier rules to `to`.
fn rewrite_upstream_rules(u: &mut proxy_common::UpstreamConfig, from: &str, to: &str) {
    for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
        if let Some(r) = rule {
            if r.provider == from {
                r.provider = to.to_string();
            }
        }
    }
}

/// Clear every tier rule referencing `provider` (leave model untouched so routing still works).
fn clear_provider_rules(u: &mut proxy_common::UpstreamConfig, provider: &str) {
    for rule in [&mut u.high, &mut u.mid, &mut u.low, &mut u.default] {
        if let Some(r) = rule {
            if r.provider == provider {
                r.provider.clear();
            }
        }
    }
}

/// Rewrite the pricing provider key `from` → `to`.
fn rewrite_provider_rules(pricing: &mut [proxy_common::ModelPricing], from: &str, to: &str) {
    for mp in pricing {
        if let Some(names) = mp.providers.remove(from) {
            mp.providers
                .entry(to.to_string())
                .or_default()
                .extend(names);
        }
    }
}

/// Drop the pricing provider key `provider`.
fn purge_provider(pricing: &mut [proxy_common::ModelPricing], provider: &str) {
    for mp in pricing {
        mp.providers.remove(provider);
    }
}

// ── Upstreams ──

pub async fn list_upstreams(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let count = config.proxy.upstreams.len();
    tracing::info!(
        "[api] list_upstreams: {} upstreams, active={}",
        count,
        config.proxy.active_upstream
    );
    let active = &config.proxy.active_upstream;
    Json(json!({
        "active_upstream": config.proxy.active_upstream,
        "active_codex_upstream": config.proxy.active_codex_upstream,
        "active_plan": config.proxy.active_plan,
        "active_proxy_upstream": config.proxy.active_proxy_upstream,
        "active_effort": config.proxy.active_effort,
        "http_proxy": config.proxy.http_proxy,
        "upstreams": config.proxy.upstreams.iter().map(|u| {
            json!({
                "name": u.name,
                "active": u.name == *active,
                "codex_active": u.name == config.proxy.active_codex_upstream,
                "proxy_active": u.name == config.proxy.active_proxy_upstream,
                "high": u.high,
                "mid": u.mid,
                "low": u.low,
                "default": u.default,
                "effort": u.effort,
            })
        }).collect::<Vec<_>>(),
        "providers": config.proxy.providers.iter().map(|p| json!({"name": p.name, "url": p.url, "has_token": p.token.is_some(), "proxy": p.proxy})).collect::<Vec<_>>(),
        "model_pricing": config.model_pricing,
    })).into_response()
}

pub async fn add_upstream(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let result = state
        .config
        .update(move |c| {
            let u: proxy_common::UpstreamConfig = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.proxy.upstreams.push(u);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] add_upstream: name={}", name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] add_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn update_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            let u: proxy_common::UpstreamConfig = serde_json::from_value(body)
                .map_err(|e| proxy_common::ConfigError::Validation(e.to_string()))?;
            c.proxy.upstreams.retain(|x| x.name != name);
            c.proxy.upstreams.push(u);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] update_upstream: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn delete_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            if c.proxy.upstreams.len() <= 1 {
                return Err(proxy_common::ConfigError::Validation(
                    "cannot delete last upstream".into(),
                ));
            }
            let was_active = c.proxy.active_upstream == name;
            let was_proxy_active = c.proxy.active_proxy_upstream == name;
            // Leaving these dangling would be rejected by validation, so deleting
            // the codex-active upstream must not be a dead end.
            let was_codex_active = c.proxy.active_codex_upstream == name;
            c.proxy.upstreams.retain(|u| u.name != name);
            if was_codex_active {
                c.proxy.active_codex_upstream = c
                    .proxy
                    .upstreams
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_default();
            }
            if was_active {
                c.proxy.active_upstream = c
                    .proxy
                    .upstreams
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_default();
            }
            if was_proxy_active {
                c.proxy.active_proxy_upstream = c
                    .proxy
                    .upstreams
                    .first()
                    .map(|u| u.name.clone())
                    .unwrap_or_default();
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] delete_upstream: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] delete_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn activate_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let target = q.get("target").map(|s| s.as_str()).unwrap_or("anthropic");
    let is_codex = target == "codex";
    let result = state
        .config
        .update(move |c| {
            if is_codex {
                c.proxy.active_codex_upstream = name;
            } else {
                c.proxy.active_upstream = name;
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] activate_upstream: {}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] activate_upstream failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

pub async fn activate_proxy_upstream(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_proxy_upstream = name;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] activate proxy upstream: {}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(&e.to_string()),
    }
}

// ── Effort ──

pub async fn get_effort(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    tracing::info!("[api] get_effort: effort={}", config.proxy.active_effort);
    Json(json!({"effort": config.proxy.active_effort})).into_response()
}

pub async fn set_effort(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let effort = body
        .get("effort")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let valid = ["auto", "low", "medium", "high", "xhigh", "max", "ultracode"];
    if !valid.contains(&effort.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid effort: {}", effort)})),
        )
            .into_response();
    }
    let log_effort = effort.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_effort = effort;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] set_effort: {}", log_effort);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] set_effort failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Retention ──

pub async fn get_retention(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    tracing::info!(
        "[api] get_retention: retention_hours={}, max_sessions={}, delete_after_days={}",
        config.proxy.request_retention_hours,
        config.proxy.session_max_count,
        config.proxy.session_delete_after_days,
    );
    Json(json!({
        "request_retention_hours": config.proxy.request_retention_hours,
        "session_max_count": config.proxy.session_max_count,
        "session_delete_after_days": config.proxy.session_delete_after_days,
    }))
    .into_response()
}

pub async fn update_retention(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let result = state
        .config
        .update(move |c| {
            if let Some(v) = body.get("request_retention_hours").and_then(|v| v.as_u64()) {
                c.proxy.request_retention_hours = v as u32;
            }
            if let Some(v) = body.get("session_max_count").and_then(|v| v.as_u64()) {
                c.proxy.session_max_count = v as u32;
            }
            if let Some(v) = body
                .get("session_delete_after_days")
                .and_then(|v| v.as_u64())
            {
                c.proxy.session_delete_after_days = v as u32;
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            tracing::info!("[api] update_retention: updated");
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::warn!("[api] update_retention failed: {}", e);
            err_response(&e.to_string())
        }
    }
}

// ── Capture ──

pub async fn toggle_capture(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    state.capture.set_enabled(enabled);
    tracing::info!("[api] toggle_capture: enabled={}", enabled);
    Json(json!({"ok": true, "enabled": enabled})).into_response()
}

pub async fn capture_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let enabled = state.capture.is_enabled();
    tracing::info!("[api] capture_status: enabled={}", enabled);
    Json(json!({"enabled": enabled})).into_response()
}

// ── Global proxy ──

pub async fn get_global_proxy(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    Json(json!({"http_proxy": config.proxy.http_proxy})).into_response()
}

pub async fn set_global_proxy(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let proxy_val = body
        .get("http_proxy")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });

    let result = state
        .config
        .update(move |c| {
            c.proxy.http_proxy = proxy_val;
            Ok(())
        })
        .await;

    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(&e.to_string()),
    }
}

// ── Clear ──

pub async fn clear_all(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.events.publish(WsMessage::Cleared);
    tracing::info!("[api] clear_all");
    Json(json!({"ok": true})).into_response()
}

// ── Hook (session lifecycle signals from proxy-hook-agent) ──

pub async fn hook_event(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let event_name = body
        .get("hook_event_name")
        .or_else(|| body.get("hookEventName"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if matches!(event_name, "Stop" | "SessionEnd") {
        if let Some(raw_sid) = body
            .get("session_id")
            .or_else(|| body.get("sessionId"))
            .and_then(|v| v.as_str())
        {
            if let Ok(sid) = proxy_common::SessionId::new(raw_sid.to_string()) {
                if let Err(e) = state
                    .store
                    .session_stop(&sid, chrono::Utc::now().timestamp_millis())
                    .await
                {
                    tracing::warn!("[api] failed to stop session {}: {}", sid, e);
                }
            }
        }
    }

    // Ingest hook observations for session timeline correlation.
    if let Some(raw_sid) = body
        .get("session_id")
        .or_else(|| body.get("sessionId"))
        .and_then(|v| v.as_str())
    {
        let parser = proxy_session::HookParser::default();
        let hook_input = body.get("hook_input").unwrap_or(&body);
        let observations = parser.parse_hook_event(raw_sid, event_name, &body, hook_input);
        for obs in observations {
            if let Err(e) = state.session.record_observation(&obs) {
                tracing::warn!("[api] failed to record hook observation: {}", e);
            }
        }
    }
    Json(json!({"ok": true})).into_response()
}

// ── Persisted summaries ──

pub async fn summarize(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let session_ids = match parse_summary_session_ids(&body) {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    generate_summaries(&state, Some(&session_ids)).await
}

pub async fn summarize_all(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    generate_summaries(&state, None).await
}

fn parse_summary_session_ids(
    body: &serde_json::Value,
) -> Result<Vec<proxy_common::SessionId>, axum::response::Response> {
    let values = body
        .get("session_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| bad_request("expected session_ids array"))?;
    let ids = values
        .iter()
        .map(parse_summary_session_id)
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err(bad_request("no valid session_ids"));
    }
    Ok(ids)
}

fn parse_summary_session_id(
    value: &serde_json::Value,
) -> Result<proxy_common::SessionId, axum::response::Response> {
    let raw = value
        .as_str()
        .ok_or_else(|| bad_request("invalid session_id type"))?;
    proxy_common::SessionId::new(raw.to_string()).map_err(|error| bad_request(&error))
}

fn bad_request(error: &str) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
}

/// Kick off a background archive pass and return a pollable job id.
async fn generate_summaries(
    state: &Arc<AppState>,
    session_ids: Option<&[proxy_common::SessionId]>,
) -> axum::response::Response {
    let config = state.config.get().await;
    let options = proxy_store::ArchiveOptions {
        task_retention_hours: config.proxy.request_retention_hours,
        force: false,
        cleanup: false,
    };
    let ids: Option<Vec<proxy_common::SessionId>> = session_ids.map(|ids| ids.to_vec());
    let (job_id, job) = state.summary_jobs.start();
    let store = state.store.clone();
    tokio::spawn(async move {
        let result = store.archive_create(ids.as_deref(), options).await;
        job.finish(result);
    });
    tracing::info!("[api] summary job {job_id} started");
    (StatusCode::ACCEPTED, Json(json!({"job_id": job_id}))).into_response()
}

/// Poll the status of a background summary job.
pub async fn summary_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.summary_jobs.get(id) {
        Some(job) => (StatusCode::OK, Json(job.snapshot())).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("summary job {id} not found")})),
        )
            .into_response(),
    }
}

// ── Cleanup ──

/// Trigger cleanup of old tasks past retention for all archived sessions.
pub async fn trigger_cleanup(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let config = state.config.get().await;
    let retention_hours = config.proxy.request_retention_hours;
    let delete_after_days = config.proxy.session_delete_after_days;
    let max_sessions = config.proxy.session_max_count;
    if retention_hours == 0 && delete_after_days == 0 && max_sessions == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "all retention policies are disabled"})),
        )
            .into_response();
    }

    let deleted_requests = if retention_hours > 0 {
        match state.store.cleanup_tasks(retention_hours as u64).await {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::error!("[api] task cleanup failed: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    } else {
        0
    };
    match state
        .store
        .cleanup_sessions(delete_after_days as u64, max_sessions as u64)
        .await
    {
        Ok(deleted_sessions) => {
            tracing::info!(
                "[api] cleanup: {} tasks, {} sessions deleted",
                deleted_requests,
                deleted_sessions
            );
            Json(json!({
                "ok": true,
                "deleted": deleted_requests,
                "deleted_requests": deleted_requests,
                "deleted_sessions": deleted_sessions
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("[api] cleanup failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

// ── Helper ──

async fn upstream_changed(config: &ConfigStore) -> WsMessage {
    let c = config.get().await;
    let active = &c.proxy.active_upstream;
    WsMessage::UpstreamChanged {
        active_upstream: active.clone(),
        active_codex_upstream: c.proxy.active_codex_upstream.clone(),
        active_proxy_upstream: c.proxy.active_proxy_upstream.clone(),
        active_plan: c.proxy.active_plan.clone(),
        upstreams: c
            .proxy
            .upstreams
            .iter()
            .map(|u| UpstreamInfo {
                name: u.name.clone(),
                active: u.name == *active,
                codex_active: u.name == c.proxy.active_codex_upstream,
                proxy_active: u.name == c.proxy.active_proxy_upstream,
                high: u.high.as_ref().map(|t| t.into()),
                mid: u.mid.as_ref().map(|t| t.into()),
                low: u.low.as_ref().map(|t| t.into()),
                default: u.default.as_ref().map(|t| t.into()),
                effort: u.effort.clone(),
            })
            .collect(),
        providers: c
            .proxy
            .providers
            .iter()
            .map(|p| ProviderInfo {
                name: p.name.clone(),
                url: p.url.clone(),
                has_token: p.token.is_some(),
                proxy: p.proxy.clone(),
                protocols: p.protocols.clone(),
                codex_url: p.codex_url.clone(),
                account: p.account.clone(),
            })
            .collect(),
        active_effort: c.proxy.active_effort.clone(),
        model_pricing: c.model_pricing.clone(),
        http_proxy: c.proxy.http_proxy.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// planx accounts
//
// Secrets are never returned: `list_accounts` reports `has_api_key` /
// `has_refresh_token` / `has_access_token` booleans instead. Quota is opt-in via
// `?probe=1` because it performs one upstream call per account.
// ─────────────────────────────────────────────────────────────────────────────

/// Publish the existing upstream-changed event so the planx registry reloads.
///
/// Accounts are part of the upstream configuration, so this reuses the event the
/// provider/upstream editors already emit — no new WS message type, no risk to
/// the frontend's message handling.
async fn accounts_changed(state: &Arc<AppState>) {
    state.events.publish(upstream_changed(&state.config).await);
}

pub async fn list_accounts(
    State(state): State<Arc<AppState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let config = state.config.get().await;
    let probe = q.get("probe").is_some_and(|v| v == "1" || v == "true");

    let quotas: std::collections::HashMap<String, serde_json::Value> = if probe {
        state
            .planx
            .probe_all()
            .await
            .into_iter()
            .map(|(name, quota)| (name, json!(quota)))
            .collect()
    } else {
        state
            .planx
            .accounts()
            .into_iter()
            .filter_map(|account| {
                account
                    .cached_quota()
                    .map(|quota| (account.name().to_string(), json!(quota)))
            })
            .collect()
    };

    let infos: Vec<serde_json::Value> = config
        .proxy
        .accounts
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "family": a.family.as_str(),
                "mode": a.mode.as_str(),
                "has_api_key": a.api_key.is_some(),
                "has_refresh_token": a.refresh_token.is_some(),
                "has_access_token": a.access_token.is_some(),
                "auth_json": a.auth_json,
                "account_id": a.account_id,
                "persist": a.persist,
                "identity": a.identity,
                "impersonate": a.impersonate,
                "cli_version": a.cli_version,
                // Drift against the local Codex CLI: a stale advertised version
                // silently shrinks the upstream model manifest.
                "cli_version_stale": state
                    .planx
                    .account(&a.name)
                    .and_then(|acc| acc.stale_cli_version().map(str::to_string)),
                // Whether this *build* can honour `impersonate` at all. Setting a
                // profile on a build without the feature is otherwise a silent
                // no-op, which looks exactly like "impersonation didn't help".
                "impersonate_supported": proxy_planx::IMPERSONATION_COMPILED,
                "problem": a.credential_problem(),
                "live": state.planx.account(&a.name).is_some(),
                "quota": quotas.get(&a.name),
            })
        })
        .collect();
    tracing::info!(
        "[api] list_accounts: {} entries (probe={})",
        infos.len(),
        probe
    );
    Json(json!(infos)).into_response()
}

/// Build an [`proxy_common::AccountConfig`] from a JSON request body.
///
/// `name` is the only required field; everything else falls back to the
/// family/mode defaults and is then patched in.
fn account_from_body(body: &serde_json::Value) -> Result<proxy_common::AccountConfig, String> {
    let name = text_field(body, "name").ok_or_else(|| "name is required".to_string())?;
    let mut account = proxy_common::AccountConfig {
        name,
        ..Default::default()
    };
    apply_account_patch(&mut account, body)?;
    Ok(account)
}

/// Patch semantics — the contract the dashboard relies on.
///
/// `GET /api/accounts` never returns secrets, so a client physically cannot echo
/// them back. Every field is therefore optional: an **absent** key keeps the
/// stored value, a **present** key overwrites it (present-but-blank clears it).
/// Without this, editing one non-secret field would wipe the credential.
fn apply_account_patch(
    account: &mut proxy_common::AccountConfig,
    body: &serde_json::Value,
) -> Result<(), String> {
    if let Some(raw) = body.get("family").and_then(|v| v.as_str()) {
        account.family = proxy_common::AccountFamily::parse(raw)
            .ok_or_else(|| format!("unknown family '{raw}' (expected gpt or claude)"))?;
    }
    if let Some(raw) = body.get("mode").and_then(|v| v.as_str()) {
        account.mode = proxy_common::AccountMode::parse(raw)
            .ok_or_else(|| format!("unknown mode '{raw}' (expected api_key or plan)"))?;
    }
    for (key, slot) in [
        ("api_key", &mut account.api_key),
        ("auth_json", &mut account.auth_json),
        ("refresh_token", &mut account.refresh_token),
        ("access_token", &mut account.access_token),
        ("account_id", &mut account.account_id),
        ("identity", &mut account.identity),
        ("impersonate", &mut account.impersonate),
        ("cli_version", &mut account.cli_version),
    ] {
        if body.get(key).is_some() {
            *slot = text_field(body, key);
        }
    }
    if let Some(persist) = body.get("persist").and_then(|v| v.as_bool()) {
        account.persist = persist;
    }
    Ok(())
}

/// A trimmed, non-empty string field, or `None` for absent/null/blank.
fn text_field(body: &serde_json::Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

pub async fn add_account(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let account = match account_from_body(&body) {
        Ok(account) => account,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": error})),
            )
                .into_response()
        }
    };
    let log_name = account.name.clone();
    let result = state
        .config
        .update(move |c| {
            if c.proxy.accounts.iter().any(|a| a.name == account.name) {
                return Err(proxy_common::ConfigError::Duplicate(format!(
                    "account '{}'",
                    account.name
                )));
            }
            c.proxy.accounts.push(account);
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            accounts_changed(&state).await;
            tracing::info!("[api] add_account: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(error) => err_response(&error.to_string()),
    }
}

pub async fn update_account(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            // Editing an account is a merge, never a replace; see apply_account_patch.
            let Some(slot) = c.proxy.accounts.iter_mut().find(|a| a.name == name) else {
                return Err(proxy_common::ConfigError::NotFound(format!(
                    "account '{name}'"
                )));
            };
            apply_account_patch(slot, &body).map_err(proxy_common::ConfigError::Validation)
        })
        .await;
    match result {
        Ok(_) => {
            accounts_changed(&state).await;
            tracing::info!("[api] update_account: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(error) => err_response(&error.to_string()),
    }
}

pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            // Refuse while referenced: silently dropping the credential would
            // turn the provider into an unauthenticated passthrough.
            let referenced: Vec<String> = c
                .proxy
                .providers
                .iter()
                .filter(|p| p.account.as_deref() == Some(name.as_str()))
                .map(|p| p.name.clone())
                .collect();
            if !referenced.is_empty() {
                return Err(proxy_common::ConfigError::Validation(format!(
                    "account '{name}' is still referenced by provider(s): {}",
                    referenced.join(", ")
                )));
            }
            let before = c.proxy.accounts.len();
            c.proxy.accounts.retain(|a| a.name != name);
            if c.proxy.accounts.len() == before {
                return Err(proxy_common::ConfigError::NotFound(format!(
                    "account '{name}'"
                )));
            }
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            accounts_changed(&state).await;
            tracing::info!("[api] delete_account: name={}", log_name);
            Json(json!({"ok": true})).into_response()
        }
        Err(error) => err_response(&error.to_string()),
    }
}

/// Force an immediate quota probe for one account.
///
/// Goes through the registry, so it uses the same endpoints, credentials and
/// identity as the scheduled probe.
pub async fn probe_account(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(quota) = state.planx.probe_one(&name).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("account '{name}' is not active")})),
        )
            .into_response();
    };
    Json(json!({"ok": quota.error.is_none(), "quota": quota})).into_response()
}

/// `POST /api/providers/:name/models` — read an upstream's model catalog.
///
/// The catalog is an *admin* view: it answers "what does this upstream offer",
/// which is not the same list the client-facing `/v1/models` returns (that one is
/// the configured routing table, see `AppConfig::declared_models`). Credentials
/// come from the named account when the provider has one, else from its token.
pub async fn provider_models(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let config = state.config.get().await;
    let Some(provider) = config
        .proxy
        .providers
        .iter()
        .find(|p| p.name == name)
        .cloned()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": format!("provider '{name}' does not exist")})),
        )
            .into_response();
    };

    let account = provider
        .account
        .as_deref()
        .map(str::trim)
        .filter(|account| !account.is_empty())
        .and_then(|account| state.planx.account(account));
    let account_family = account.as_ref().map(|account| account.family());
    let kind = provider.catalog_kind(account_family);
    let declared = config.declared_models();

    // A manual catalog is a legitimate answer, not an error: the gateway may not
    // publish one at all, in which case the configured models *are* the catalog.
    if !kind.fetches() {
        return Json(json!({
            "ok": true,
            "kind": kind.as_str(),
            "manual": true,
            "models": [],
            "declared": declared,
            "only_upstream": [],
            "only_local": declared,
            "error": serde_json::Value::Null,
        }))
        .into_response();
    }

    let auth = match account.as_ref() {
        Some(account) => match account.current_auth() {
            Some(auth) => auth,
            None => {
                return Json(json!({
                    "ok": false,
                    "kind": kind.as_str(),
                    "error": format!("account '{}' has no usable credential", account.name()),
                }))
                .into_response()
            }
        },
        None => match provider
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(token) => proxy_common::UpstreamAuth::Static(token.to_string()),
            None => {
                return Json(json!({
                    "ok": false,
                    "kind": kind.as_str(),
                    "error": format!("provider '{name}' has neither an account nor a token"),
                }))
                .into_response()
            }
        },
    };
    let client_version = account
        .as_ref()
        .map(|account| account.identity().client_version.clone())
        .unwrap_or_default();
    let impersonation = account
        .as_ref()
        .map(|account| account.impersonation())
        .unwrap_or(proxy_common::Impersonation::Off);
    let proxy = provider
        .proxy
        .as_deref()
        .or(config.proxy.http_proxy.as_deref());
    let client = match proxy_planx::transport::maintenance_client(proxy) {
        Ok(client) => client,
        Err(error) => {
            return Json(json!({
                "ok": false,
                "kind": kind.as_str(),
                "error": format!("could not build a maintenance client: {error}"),
            }))
            .into_response()
        }
    };

    let url = provider.catalog_url(account_family);
    let catalog = proxy_planx::probe::fetch_models(proxy_planx::probe::ModelsRequest {
        client: &client,
        kind,
        url: &url,
        auth: &auth,
        impersonation,
        client_version: &client_version,
    })
    .await;

    // Drift, both ways: what the upstream has that we do not route, and what we
    // route that the upstream did not mention.
    let upstream: Vec<String> = catalog.models.iter().map(|m| m.id.clone()).collect();
    let only_upstream: Vec<&String> = upstream
        .iter()
        .filter(|id| !declared.contains(id))
        .collect();
    let only_local: Vec<&String> = declared
        .iter()
        .filter(|id| !upstream.contains(id))
        .collect();

    Json(json!({
        "ok": catalog.error.is_none(),
        "kind": catalog.kind.as_str(),
        "manual": false,
        "url": catalog.url,
        "fetched_at": catalog.fetched_at,
        "models": catalog.models,
        "declared": declared,
        "only_upstream": only_upstream,
        "only_local": only_local,
        "error": catalog.error,
    }))
    .into_response()
}

/// Activate (or clear) the relay's **plan connection**.
///
/// A plan is a peer of an upstream: named after a `[[proxy.accounts]]` entry, it
/// bypasses providers and tiers, sends every request to that account's vendor
/// endpoint, and makes `GET /v1/models` report the plan's own models. An empty
/// name clears it and returns the relay to upstream/tier routing.
pub async fn activate_plan(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    if !name.is_empty() {
        let known = state
            .config
            .get()
            .await
            .proxy
            .accounts
            .iter()
            .any(|account| account.name == name);
        if !known {
            return err_response(&format!(
                "'{name}' is not a configured account; a plan names a [[proxy.accounts]] entry"
            ));
        }
    }

    let log_name = name.clone();
    let result = state
        .config
        .update(move |c| {
            c.proxy.active_plan = name;
            Ok(())
        })
        .await;
    match result {
        Ok(_) => {
            state.events.publish(upstream_changed(&state.config).await);
            tracing::info!("[api] activate_plan: name={log_name}");
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => err_response(&e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_common::{ModelPricing, TierRule, UpstreamConfig};
    use std::collections::HashMap;

    fn rule(provider: &str) -> TierRule {
        TierRule {
            provider: provider.into(),
            model: "claude-opus".into(),
        }
    }

    fn upstream(name: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            high: Some(rule("cloudapi")),
            mid: None,
            low: None,
            default: Some(rule("cloudapi")),
            effort: None,
        }
    }

    fn pricing(id: &str, providers: &[(&str, Vec<&str>)]) -> ModelPricing {
        let mut m = HashMap::new();
        for (k, names) in providers {
            m.insert(k.to_string(), names.iter().map(|s| s.to_string()).collect());
        }
        ModelPricing {
            id: id.into(),
            price: vec![],
            providers: m,
        }
    }

    #[test]
    fn rewrite_upstream_rules_changes_matching_provider() {
        let mut u = upstream("cloud-fable");
        rewrite_upstream_rules(&mut u, "cloudapi", "anthropic");
        assert_eq!(u.high.unwrap().provider, "anthropic");
        assert_eq!(u.default.unwrap().provider, "anthropic");
    }

    #[test]
    fn rewrite_upstream_rules_ignores_other_providers() {
        let mut u = upstream("cloud-fable");
        rewrite_upstream_rules(&mut u, "deepseek", "anthropic");
        assert_eq!(u.high.unwrap().provider, "cloudapi");
    }

    #[test]
    fn clear_upstream_rules_blanks_matching_provider() {
        let mut u = upstream("cloud-fable");
        clear_provider_rules(&mut u, "cloudapi");
        assert!(u.high.unwrap().provider.is_empty());
        assert!(u.default.unwrap().provider.is_empty());
    }

    #[test]
    fn rewrite_pricing_migrates_provider_key() {
        let mut mp = vec![pricing("claude-opus", &[("cloudapi", vec!["claude-opus"])])];
        rewrite_provider_rules(&mut mp, "cloudapi", "anthropic");
        assert!(mp[0].providers.contains_key("anthropic"));
        assert!(!mp[0].providers.contains_key("cloudapi"));
        assert_eq!(mp[0].providers["anthropic"], vec!["claude-opus"]);
    }

    #[test]
    fn purge_pricing_removes_provider_key() {
        let mut mp = vec![pricing("claude-opus", &[("cloudapi", vec![])])];
        purge_provider(&mut mp, "cloudapi");
        assert!(mp[0].providers.is_empty());
    }

    fn upstream_with_model(name: &str, model: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            high: None,
            mid: None,
            low: None,
            default: Some(TierRule {
                provider: "rdsec".into(),
                model: model.into(),
            }),
            effort: None,
        }
    }

    #[test]
    fn rewrite_pricing_rules_changes_model_reference() {
        let mut u = upstream_with_model("rdsec-kimi", "kimi");
        rewrite_pricing_rules(&mut u, "kimi", "claude-sonnet");
        assert_eq!(u.default.unwrap().model, "claude-sonnet");
    }

    #[test]
    fn rewrite_pricing_rules_ignores_other_models() {
        let mut u = upstream_with_model("rdsec-kimi", "kimi");
        rewrite_pricing_rules(&mut u, "claude-opus", "claude-sonnet");
        assert_eq!(u.default.unwrap().model, "kimi");
    }

    #[test]
    fn clear_pricing_rules_blanks_model_reference() {
        let mut u = upstream_with_model("rdsec-kimi", "kimi");
        clear_pricing_rules(&mut u, "kimi");
        assert!(u.default.unwrap().model.is_empty());
    }

    fn upstream_with_mapping(name: &str, prov: &str, model: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            high: None,
            mid: None,
            low: None,
            default: Some(TierRule {
                provider: prov.into(),
                model: model.into(),
            }),
            effort: None,
        }
    }

    #[test]
    fn mapping_refs_exist_detects_reference() {
        let us = [upstream_with_mapping("rdsec-kimi", "rdsec", "kimi")];
        assert!(mapping_refs_exist(&us, "kimi", "rdsec"));
        assert!(!mapping_refs_exist(&us, "kimi", "anthropic"));
        assert!(!mapping_refs_exist(&us, "claude-opus", "rdsec"));
    }

    #[test]
    fn rewrite_mapping_refs_changes_provider() {
        let mut us = [upstream_with_mapping("rdsec-kimi", "rdsec", "kimi")];
        rewrite_mapping_refs(&mut us, "kimi", "rdsec", "rdsec2");
        let d = us[0].default.as_ref().unwrap();
        assert_eq!(d.provider, "rdsec2");
        assert_eq!(d.model, "kimi");
    }

    #[test]
    fn clear_mapping_refs_blankes_provider() {
        let mut us = [upstream_with_mapping("rdsec-kimi", "rdsec", "kimi")];
        clear_mapping_refs(&mut us, "kimi", "rdsec");
        let d = us[0].default.as_ref().unwrap();
        assert!(d.provider.is_empty());
        assert_eq!(d.model, "kimi");
    }
}
