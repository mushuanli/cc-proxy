use std::path::Path;

use crate::config::AppConfig;
use crate::error::{ConfigError, ConfigResult};
use crate::pricing::ModelPricing;
use crate::provider::Provider;
use crate::upstream::TierRule;

/// Persist the current AppConfig to config.toml using toml_edit for format preservation.
pub async fn persist_config(path: &Path, config: &AppConfig) -> ConfigResult<()> {
    let content = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = if content.is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content
            .parse()
            .map_err(|e| ConfigError::TomlEdit(toml_edit::TomlError::from(e)))?
    };

    // Write model_pricing array
    write_model_pricing(&mut doc, &config.model_pricing);

    // Write proxy section
    write_proxy_section(&mut doc, config);

    // Write server section
    write_server_section(&mut doc, config);

    // Write logging section
    write_logging_section(&mut doc, config);

    // Remove legacy keys
    doc.remove("session_retention_days");
    doc.remove("api_target");

    let serialized = doc.to_string();
    tokio::fs::write(path, &serialized).await?;

    Ok(())
}

fn write_model_pricing(doc: &mut toml_edit::DocumentMut, pricing: &[ModelPricing]) {
    let mut arr = toml_edit::Array::new();
    for mp in pricing {
        let mut tbl = toml_edit::InlineTable::new();
        tbl.insert("id", mp.id.as_str().into());

        let mut price_arr = toml_edit::Array::new();
        for &p in &mp.price {
            price_arr.push(p);
        }
        tbl.insert("price", toml_edit::Value::Array(price_arr));

        if !mp.providers.is_empty() {
            let mut pt = toml_edit::InlineTable::new();
            for (k, v) in &mp.providers {
                let mut names_arr = toml_edit::Array::new();
                for n in v {
                    names_arr.push(n.as_str());
                }
                pt.insert(k.as_str(), toml_edit::Value::Array(names_arr));
            }
            tbl.insert("providers", toml_edit::Value::InlineTable(pt));
        }

        arr.push(tbl);
    }
    doc["model_pricing"] = toml_edit::value(arr);
}

fn write_proxy_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let proxy = &config.proxy;
    let mut tbl = toml_edit::Table::new();

    tbl.insert("active_upstream", toml_edit::value(proxy.active_upstream.as_str()));
    tbl.insert("active_effort", toml_edit::value(proxy.active_effort.as_str()));
    if let Some(ref hp) = proxy.http_proxy {
        tbl.insert("http_proxy", toml_edit::value(hp.as_str()));
    }
    tbl.insert("retry_count", toml_edit::value(proxy.retry_count as i64));
    tbl.insert(
        "request_timeout_secs",
        toml_edit::value(proxy.request_timeout_secs as i64),
    );
    tbl.insert(
        "request_retention_hours",
        toml_edit::value(proxy.request_retention_hours as i64),
    );
    tbl.insert(
        "session_max_count",
        toml_edit::value(proxy.session_max_count as i64),
    );
    tbl.insert(
        "session_delete_after_days",
        toml_edit::value(proxy.session_delete_after_days as i64),
    );

    // Providers array
    let mut providers_arr = toml_edit::ArrayOfTables::new();
    for p in &proxy.providers {
        let mut pt = toml_edit::Table::new();
        pt.insert("name", toml_edit::value(p.name.as_str()));
        pt.insert("url", toml_edit::value(p.url.as_str()));
        if let Some(ref token) = p.token {
            pt.insert("token", toml_edit::value(token.as_str()));
        }
        if let Some(ref proxy_val) = p.proxy {
            pt.insert("proxy", toml_edit::value(proxy_val.as_str()));
        }
        providers_arr.push(pt);
    }
    tbl.insert("providers", toml_edit::Item::ArrayOfTables(providers_arr));

    // Upstreams array
    let mut upstreams_arr = toml_edit::ArrayOfTables::new();
    for u in &proxy.upstreams {
        let mut ut = toml_edit::Table::new();
        ut.insert("name", toml_edit::value(u.name.as_str()));
        if let Some(ref high) = u.high {
            ut.insert("high", tier_rule_to_item(high));
        }
        if let Some(ref mid) = u.mid {
            ut.insert("mid", tier_rule_to_item(mid));
        }
        if let Some(ref low) = u.low {
            ut.insert("low", tier_rule_to_item(low));
        }
        if let Some(ref default) = u.default {
            ut.insert("default", tier_rule_to_item(default));
        }
        if let Some(ref effort) = u.effort {
            ut.insert("effort", toml_edit::value(effort.as_str()));
        }
        upstreams_arr.push(ut);
    }
    tbl.insert("upstreams", toml_edit::Item::ArrayOfTables(upstreams_arr));

    doc["proxy"] = toml_edit::Item::Table(tbl);
}

fn tier_rule_to_item(rule: &TierRule) -> toml_edit::Item {
    let mut tbl = toml_edit::Table::new();
    let mut kw_arr = toml_edit::Array::new();
    for k in &rule.keywords {
        kw_arr.push(k.as_str());
    }
    tbl.insert("keywords", toml_edit::value(toml_edit::Value::Array(kw_arr)));
    tbl.insert("provider", toml_edit::value(rule.provider.as_str()));
    tbl.insert("model", toml_edit::value(rule.model.as_str()));
    toml_edit::Item::Table(tbl)
}

fn write_server_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let mut tbl = toml_edit::Table::new();
    tbl.insert(
        "listen_address",
        toml_edit::value(config.server.listen_address.as_str()),
    );
    tbl.insert("http_port", toml_edit::value(config.server.http_port as i64));
    tbl.insert("proxy_port", toml_edit::value(config.server.proxy_port as i64));
    tbl.insert(
        "mcp_proxy_port",
        toml_edit::value(config.server.mcp_proxy_port as i64),
    );
    doc["server"] = toml_edit::Item::Table(tbl);
}

fn write_logging_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let mut tbl = toml_edit::Table::new();
    tbl.insert("level", toml_edit::value(config.logging.level.as_str()));
    doc["logging"] = toml_edit::Item::Table(tbl);
}

/// Persist a single model pricing change.
pub async fn persist_model_pricing(path: &Path, pricing: &[ModelPricing]) -> ConfigResult<()> {
    let content = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = if content.is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content
            .parse()
            .map_err(|e| ConfigError::TomlEdit(toml_edit::TomlError::from(e)))?
    };

    write_model_pricing(&mut doc, pricing);

    tokio::fs::write(path, doc.to_string()).await?;
    Ok(())
}

/// Persist providers changes.
pub async fn persist_providers(path: &Path, providers: &[Provider]) -> ConfigResult<()> {
    let content = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = if content.is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content
            .parse()
            .map_err(|e| ConfigError::TomlEdit(toml_edit::TomlError::from(e)))?
    };

    let mut providers_arr = toml_edit::ArrayOfTables::new();
    for p in providers {
        let mut pt = toml_edit::Table::new();
        pt.insert("name", toml_edit::value(p.name.as_str()));
        pt.insert("url", toml_edit::value(p.url.as_str()));
        if let Some(ref token) = p.token {
            pt.insert("token", toml_edit::value(token.as_str()));
        }
        if let Some(ref proxy_val) = p.proxy {
            pt.insert("proxy", toml_edit::value(proxy_val.as_str()));
        }
        providers_arr.push(pt);
    }

    doc["proxy"]["providers"] = toml_edit::Item::ArrayOfTables(providers_arr);
    tokio::fs::write(path, doc.to_string()).await?;
    Ok(())
}
