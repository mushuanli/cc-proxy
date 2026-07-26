use std::path::Path;

use crate::config::AppConfig;
use crate::error::{ConfigError, ConfigResult};
use crate::pricing::ModelPricing;
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
        content.parse().map_err(ConfigError::TomlEdit)?
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

    atomic_write(path, &serialized).await
}

async fn atomic_write(path: &Path, content: &str) -> ConfigResult<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", ulid::Ulid::new()));
    tokio::fs::write(&tmp_path, content).await?;
    let file = tokio::fs::File::open(&tmp_path).await?;
    file.sync_all().await?;
    drop(file);
    if let Err(error) = tokio::fs::rename(&tmp_path, path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error.into());
    }
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

    tbl.insert(
        "active_upstream",
        toml_edit::value(proxy.active_upstream.as_str()),
    );
    tbl.insert(
        "active_proxy_upstream",
        toml_edit::value(proxy.active_proxy_upstream.as_str()),
    );
    tbl.insert(
        "active_effort",
        toml_edit::value(proxy.active_effort.as_str()),
    );
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

        let def = u.default.as_ref();
        let def_provider = def.map(|d| d.provider.as_str());

        for (tier, rule) in [
            ("high", u.high.as_ref()),
            ("mid", u.mid.as_ref()),
            ("low", u.low.as_ref()),
        ] {
            if let Some(r) = rule {
                if let Some(d) = def {
                    if r.provider == d.provider && r.model == d.model {
                        continue;
                    }
                }
                ut.insert(tier, tier_rule_to_item(r, def_provider));
            }
        }
        if let Some(ref default) = u.default {
            ut.insert("default", tier_rule_to_item(default, None));
        }
        if let Some(ref effort) = u.effort {
            ut.insert("effort", toml_edit::value(effort.as_str()));
        }
        upstreams_arr.push(ut);
    }
    tbl.insert("upstreams", toml_edit::Item::ArrayOfTables(upstreams_arr));

    doc["proxy"] = toml_edit::Item::Table(tbl);
}

fn tier_rule_to_item(rule: &TierRule, def_provider: Option<&str>) -> toml_edit::Item {
    let mut tbl = toml_edit::Table::new();
    if def_provider.map_or(true, |dp| rule.provider != dp) {
        tbl.insert("provider", toml_edit::value(rule.provider.as_str()));
    }
    tbl.insert("model", toml_edit::value(rule.model.as_str()));
    toml_edit::Item::Table(tbl)
}

fn write_server_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let mut tbl = toml_edit::Table::new();
    tbl.insert(
        "listen_address",
        toml_edit::value(config.server.listen_address.as_str()),
    );
    tbl.insert(
        "http_port",
        toml_edit::value(config.server.http_port as i64),
    );
    tbl.insert(
        "proxy_port",
        toml_edit::value(config.server.proxy_port as i64),
    );
    if let Some(ref token) = config.server.auth_token {
        tbl.insert("auth_token", toml_edit::value(token.as_str()));
    }
    tbl.insert(
        "ws_include_bodies",
        toml_edit::value(config.server.ws_include_bodies),
    );
    doc["server"] = toml_edit::Item::Table(tbl);
}

fn write_logging_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let mut tbl = toml_edit::Table::new();
    tbl.insert("level", toml_edit::value(config.logging.level.as_str()));
    doc["logging"] = toml_edit::Item::Table(tbl);
}
