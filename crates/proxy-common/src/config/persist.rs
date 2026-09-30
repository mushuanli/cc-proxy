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
    doc.remove("model_pricing");
    if pricing.is_empty() {
        return;
    }
    let mut arr = toml_edit::ArrayOfTables::new();
    for mp in pricing {
        let mut tbl = toml_edit::Table::new();
        tbl.insert("id", toml_edit::value(mp.id.as_str()));
        tbl.insert(
            "price",
            toml_edit::value(toml_edit::Value::Array(mp.price.iter().copied().collect())),
        );
        if !mp.providers.is_empty() {
            let mut providers_tbl = toml_edit::Table::new();
            for (k, v) in &mp.providers {
                let mut names_arr = toml_edit::Array::new();
                for n in v {
                    names_arr.push(n.as_str());
                }
                providers_tbl.insert(
                    k.as_str(),
                    toml_edit::value(toml_edit::Value::Array(names_arr)),
                );
            }
            tbl.insert("providers", toml_edit::Item::Table(providers_tbl));
        }
        arr.push(tbl);
    }
    doc["model_pricing"] = toml_edit::Item::ArrayOfTables(arr);
}

fn write_proxy_section(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let proxy = &config.proxy;
    let mut tbl = toml_edit::Table::new();

    tbl.insert(
        "active_upstream",
        toml_edit::value(proxy.active_upstream.as_str()),
    );
    // Codex-specific upstream. Must be written or a dashboard edit silently
    // drops it from disk (memory keeps it until restart).
    tbl.insert(
        "active_codex_upstream",
        toml_edit::value(proxy.active_codex_upstream.as_str()),
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
        // Protocol allowlist and the Codex endpoint were previously not written,
        // so any dashboard edit erased them from config.toml.
        if !p.protocols.is_empty() {
            let mut protocols = toml_edit::Array::new();
            for protocol in &p.protocols {
                protocols.push(protocol.as_str());
            }
            pt.insert("protocols", toml_edit::value(protocols));
        }
        if let Some(ref codex_url) = p.codex_url {
            pt.insert("codex_url", toml_edit::value(codex_url.as_str()));
        }
        if let Some(ref token) = p.token {
            pt.insert("token", toml_edit::value(token.as_str()));
        }
        if let Some(ref proxy_val) = p.proxy {
            pt.insert("proxy", toml_edit::value(proxy_val.as_str()));
        }
        if let Some(ref account) = p.account {
            pt.insert("account", toml_edit::value(account.as_str()));
        }
        providers_arr.push(pt);
    }
    tbl.insert("providers", toml_edit::Item::ArrayOfTables(providers_arr));

    // Upstream accounts array (family × mode)
    let mut accounts_arr = toml_edit::ArrayOfTables::new();
    for account in &proxy.accounts {
        let mut at = toml_edit::Table::new();
        at.insert("name", toml_edit::value(account.name.as_str()));
        at.insert("family", toml_edit::value(account.family.as_str()));
        at.insert("mode", toml_edit::value(account.mode.as_str()));
        if let Some(ref api_key) = account.api_key {
            at.insert("api_key", toml_edit::value(api_key.as_str()));
        }
        if let Some(ref path) = account.auth_json {
            at.insert("auth_json", toml_edit::value(path.as_str()));
        }
        if let Some(ref refresh_token) = account.refresh_token {
            at.insert("refresh_token", toml_edit::value(refresh_token.as_str()));
        }
        if let Some(ref access_token) = account.access_token {
            at.insert("access_token", toml_edit::value(access_token.as_str()));
        }
        if let Some(ref account_id) = account.account_id {
            at.insert("account_id", toml_edit::value(account_id.as_str()));
        }
        if account.persist {
            at.insert("persist", toml_edit::value(true));
        }
        if let Some(ref identity) = account.identity {
            at.insert("identity", toml_edit::value(identity.as_str()));
        }
        if let Some(ref impersonate) = account.impersonate {
            at.insert("impersonate", toml_edit::value(impersonate.as_str()));
        }
        if let Some(ref version) = account.cli_version {
            at.insert("cli_version", toml_edit::value(version.as_str()));
        }
        accounts_arr.push(at);
    }
    if !accounts_arr.is_empty() {
        tbl.insert("accounts", toml_edit::Item::ArrayOfTables(accounts_arr));
    }

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::loader::load_config;
    use crate::config::{AccountConfig, AccountFamily, AccountMode, AppConfig, Provider};

    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cc-proxy-persist-{tag}-{}.toml", ulid::Ulid::new()))
    }

    /// Config carrying every provider-protocol field that used to be dropped.
    fn codex_route_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.proxy.active_codex_upstream = "codex-pool".into();
        // The selector must resolve, or `migrate()` (which `load_config` runs)
        // repairs it and the round-trip assertion no longer sees it.
        config.proxy.upstreams.push(crate::config::UpstreamConfig {
            name: "codex-pool".into(),
            high: None,
            mid: None,
            low: None,
            default: None,
            effort: None,
        });
        config.proxy.providers.push(Provider {
            name: "p1".into(),
            url: "https://api.example.com".into(),
            token: Some("sk-1".into()),
            proxy: None,
            protocols: vec!["codex".into()],
            codex_url: Some("https://api.example.com/v1".into()),
            account: None,
        });
        config
    }

    #[tokio::test]
    async fn proxy_section_round_trips_codex_fields() {
        let path = temp_config_path("codex-fields");
        persist_config(&path, &codex_route_config()).await.unwrap();

        let reloaded = load_config(&path).await.unwrap();
        assert_eq!(reloaded.proxy.active_codex_upstream, "codex-pool");
        assert_eq!(reloaded.proxy.providers.len(), 1);
        assert_eq!(
            reloaded.proxy.providers[0].protocols,
            vec!["codex".to_string()]
        );
        assert_eq!(
            reloaded.proxy.providers[0].codex_url.as_deref(),
            Some("https://api.example.com/v1")
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Regression: a dashboard edit rewrites the whole proxy section. Before the
    /// fix this erased `active_codex_upstream` / `protocols` / `codex_url`.
    #[tokio::test]
    async fn repeated_persist_is_stable_and_keeps_codex_fields() {
        let path = temp_config_path("twice");
        let config = codex_route_config();

        persist_config(&path, &config).await.unwrap();
        let first = tokio::fs::read_to_string(&path).await.unwrap();

        // Second write = the `ConfigStore::update` path used by the dashboard.
        persist_config(&path, &config).await.unwrap();
        let second = tokio::fs::read_to_string(&path).await.unwrap();

        assert_eq!(first, second, "persist must be idempotent");
        for needle in ["active_codex_upstream", "protocols", "codex_url"] {
            assert!(
                second.contains(needle),
                "{needle} missing after re-persist:\n{second}"
            );
        }

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn empty_protocols_are_omitted() {
        let path = temp_config_path("empty-protocols");
        let mut config = AppConfig::default();
        config.proxy.providers.push(Provider {
            name: "plain".into(),
            url: "https://api.example.com".into(),
            token: None,
            proxy: None,
            protocols: vec![],
            codex_url: None,
            account: None,
        });
        persist_config(&path, &config).await.unwrap();

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw.contains("protocols"),
            "empty protocols should be omitted"
        );
        assert!(
            !raw.contains("codex_url"),
            "absent codex_url should be omitted"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn accounts_round_trip_for_both_families_and_modes() {
        let path = temp_config_path("accounts");
        let mut config = AppConfig::default();
        config.proxy.accounts.push(AccountConfig {
            name: "gpt-plan".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            auth_json: Some("~/.codex/auth.json".into()),
            persist: true,
            identity: Some("codex_tui".into()),
            cli_version: Some("0.159.2".into()),
            ..Default::default()
        });
        config.proxy.accounts.push(AccountConfig {
            name: "claude-plan".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::Plan,
            refresh_token: Some("sk-ant-ort01-x".into()),
            ..Default::default()
        });
        config.proxy.accounts.push(AccountConfig {
            name: "claude-key".into(),
            family: AccountFamily::Claude,
            mode: AccountMode::ApiKey,
            api_key: Some("sk-ant-api03-x".into()),
            ..Default::default()
        });
        config.proxy.providers.push(Provider {
            name: "planx".into(),
            url: "https://chatgpt.com/backend-api/codex".into(),
            token: None,
            proxy: None,
            protocols: vec!["codex".into()],
            codex_url: None,
            account: Some("gpt-plan".into()),
        });
        persist_config(&path, &config).await.unwrap();

        let reloaded = load_config(&path).await.unwrap();
        assert_eq!(reloaded.proxy.accounts.len(), 3);

        let gpt = &reloaded.proxy.accounts[0];
        assert_eq!(gpt.family, AccountFamily::Gpt);
        assert_eq!(gpt.mode, AccountMode::Plan);
        assert!(gpt.persist);
        assert_eq!(gpt.identity.as_deref(), Some("codex_tui"));

        let claude_plan = &reloaded.proxy.accounts[1];
        assert_eq!(claude_plan.family, AccountFamily::Claude);
        assert_eq!(claude_plan.mode, AccountMode::Plan);
        assert_eq!(claude_plan.refresh_token.as_deref(), Some("sk-ant-ort01-x"));

        let claude_key = &reloaded.proxy.accounts[2];
        assert_eq!(claude_key.mode, AccountMode::ApiKey);
        assert_eq!(claude_key.api_key.as_deref(), Some("sk-ant-api03-x"));

        assert_eq!(
            reloaded.proxy.providers[0].account.as_deref(),
            Some("gpt-plan")
        );

        let _ = tokio::fs::remove_file(&path).await;
    }
}
