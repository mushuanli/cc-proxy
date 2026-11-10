use std::path::Path;

use crate::config::AppConfig;
use crate::error::{ConfigError, ConfigResult};
use crate::pricing::ModelPricing;
use crate::upstream::TierRule;

/// Persist the current AppConfig using the canonical flat array layout.
pub async fn persist_config(path: &Path, config: &AppConfig) -> ConfigResult<()> {
    let content = if path.exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        String::new()
    };

    let previous: toml_edit::DocumentMut = if content.is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        content.parse().map_err(ConfigError::TomlEdit)?
    };
    let serialized = format_config(&previous, config);
    atomic_write(path, &serialized).await
}

/// Canonical flat layout; keep unknown root fields and sections when saving.
fn format_config(previous: &toml_edit::DocumentMut, config: &AppConfig) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    write_routing_settings(&mut doc, config);
    write_model_pricing(&mut doc, &config.model_pricing);
    write_server_section(&mut doc, config);
    write_logging_section(&mut doc, config);
    for (key, item) in previous.iter() {
        if !doc.contains_key(key)
            && !matches!(
                key,
                "proxy"
                    | "session_retention_days"
                    | "api_target"
                    | "accounts"
                    | "model_pricing"
                    | "http_proxy"
            )
        {
            doc.insert(key, item.clone());
        }
    }
    add_group_comments(&mut doc);
    doc.to_string()
}

fn add_group_comments(doc: &mut toml_edit::DocumentMut) {
    for (key, comment) in [
        (
            "active_upstream",
            "# 默认路由与运行参数（http_proxy 才是出站网络代理）\n",
        ),
        ("retry_count", "\n# 请求策略\n"),
        ("request_retention_hours", "\n# 数据保留\n"),
        ("providers", "\n# 服务商：连接地址、认证与协议\n"),
        ("accounts", "\n# 可选账号：provider 通过 account 引用\n"),
        (
            "upstreams",
            "\n# 路由方案：default 为兜底，high / mid / low 为可选覆盖\n",
        ),
        (
            "model_pricing",
            "\n# 模型映射与价格：[输入、输出、缓存写入、缓存读取]，USD / 百万 token\n",
        ),
    ] {
        if let Some(mut key) = doc.key_mut(key) {
            key.leaf_decor_mut().set_prefix(comment);
        }
    }
}

/// One inline table per line inside a regular TOML array.
fn table_array(tables: toml_edit::ArrayOfTables) -> toml_edit::Item {
    let mut array = toml_edit::Array::new();
    for table in tables {
        let mut value = toml_edit::Value::InlineTable(table.into_inline_table());
        value.decor_mut().set_prefix("\n    ");
        array.push_formatted(value);
    }
    if !array.is_empty() {
        array.set_trailing_comma(true);
        array.set_trailing("\n");
    }
    toml_edit::value(array)
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
            let mut providers: Vec<_> = mp.providers.iter().collect();
            providers.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (k, v) in providers {
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
    doc["model_pricing"] = table_array(arr);
}

fn write_routing_settings(doc: &mut toml_edit::DocumentMut, config: &AppConfig) {
    let proxy = &config.proxy;
    let mut tbl = toml_edit::Table::new();

    tbl.insert(
        "active_upstream",
        toml_edit::value(proxy.active_upstream.as_str()),
    );
    tbl.insert("active_plan", toml_edit::value(proxy.active_plan.as_str()));
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
        if let Some(ref models_url) = p.models_url {
            pt.insert("models_url", toml_edit::value(models_url.as_str()));
        }
        if let Some(ref models_kind) = p.models_kind {
            pt.insert("models_kind", toml_edit::value(models_kind.as_str()));
        }
        providers_arr.push(pt);
    }
    tbl.insert("providers", table_array(providers_arr));

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
    tbl.insert("accounts", table_array(accounts_arr));

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
    tbl.insert("upstreams", table_array(upstreams_arr));

    for (key, value) in tbl {
        doc.insert(&key, value);
    }
}

fn tier_rule_to_item(rule: &TierRule, def_provider: Option<&str>) -> toml_edit::Item {
    let mut tbl = toml_edit::Table::new();
    if !rule.provider.is_empty() && def_provider.map_or(true, |dp| rule.provider != dp) {
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
    if !config.server.cors_origins.is_empty() {
        let mut origins = toml_edit::Array::new();
        for origin in &config.server.cors_origins {
            origins.push(origin.as_str());
        }
        tbl.insert("cors_origins", toml_edit::value(origins));
    }
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
        config.proxy.active_plan = "gpt-plan".into();
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
            models_url: None,
            models_kind: None,
        });
        config
    }

    #[tokio::test]
    async fn proxy_section_round_trips_codex_fields() {
        let path = temp_config_path("codex-fields");
        persist_config(&path, &codex_route_config()).await.unwrap();

        let reloaded = load_config(&path).await.unwrap();
        assert_eq!(reloaded.proxy.active_plan, "gpt-plan");
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
    /// fix this erased `protocols` / `codex_url`.
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
        for needle in ["protocols", "codex_url"] {
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
            models_url: None,
            models_kind: None,
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
            models_url: None,
            models_kind: None,
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

#[cfg(test)]
mod flat_tests {
    use super::*;
    use crate::config::ConfigStore;
    use serde_json::json;

    fn flat_config() -> AppConfig {
        let mut base = serde_json::to_value(AppConfig::default()).unwrap();
        let extra = json!({
            "active_upstream":"woagent",
            "providers":[{"name":"woagent","url":"https://ai.woagent.net","token":"sk-test\"\\\nsecret","protocols":["chat_completions"],"proxy":""}],
            "accounts":[{"name":"demo","family":"gpt","mode":"api_key","api_key":"sk-demo"}],
            "upstreams":[{"name":"woagent","high":{"provider":"","model":"other"},"default":{"provider":"woagent","model":"KM-kimi-k2.6"}}],
            "model_pricing":[{"id":"KM-kimi-k2.6","price":[0.0,0.0],"providers":{"woagent":["KM-kimi-k2.6"],"provider.with.dot":["alias"]}}]
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }
    #[test]
    fn root_arrays_preserve_all_values_and_escape_nested_keys_and_secrets() {
        let config = flat_config();
        let output = format_config(&toml_edit::DocumentMut::new(), &config);
        let doc: toml_edit::DocumentMut = output.parse().unwrap();
        assert!(!doc.contains_key("proxy"));
        for key in ["providers", "accounts", "upstreams", "model_pricing"] {
            assert!(doc[key].is_array(), "{key} should use a regular array");
            assert!(!output.contains(&format!("[[{key}]]")));
        }
        assert!(!output.contains("[upstreams.default]"));
        let reloaded: AppConfig = toml::from_str(&output).unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(&reloaded).unwrap()
        );
        let second = format_config(&doc, &reloaded);
        assert_eq!(
            output, second,
            "load/save must not change layout or mapping order"
        );
    }
    #[tokio::test]
    async fn dashboard_save_and_reload_keep_flat_arrays_and_route_selection() {
        let path = std::env::temp_dir().join(format!("cc-proxy-flat-{}.toml", ulid::Ulid::new()));
        let config = flat_config();
        persist_config(&path, &config).await.unwrap();
        let store = ConfigStore::open(&path).await.unwrap();
        store
            .update(|c| {
                c.proxy.request_timeout_secs = 77;
                Ok(())
            })
            .await
            .unwrap();
        let reloaded = store.reload().await.unwrap();
        assert_eq!(reloaded.proxy.active_upstream, "woagent");
        assert_eq!(reloaded.proxy.request_timeout_secs, 77);
        assert_eq!(
            reloaded.proxy.providers[0].token,
            config.proxy.providers[0].token
        );
        let route = store.resolve_route("anything").await.unwrap();
        assert_eq!(route.provider, "woagent");
        assert_eq!(route.resolved_model, "KM-kimi-k2.6");
        let output = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!output.contains("[proxy]"));
        assert!(output.contains("providers = [\n"));
        tokio::fs::remove_file(path).await.unwrap();
    }
    #[test]
    fn unknown_sections_survive_without_restoring_cleared_optional_fields() {
        let previous: toml_edit::DocumentMut =
            "http_proxy = 'http://old:8080'\n[custom]\nflag = true\n"
                .parse()
                .unwrap();
        let output = format_config(&previous, &AppConfig::default());
        let doc: toml_edit::DocumentMut = output.parse().unwrap();
        assert!(!doc.contains_key("http_proxy"));
        assert_eq!(doc["custom"]["flag"].as_bool(), Some(true));
        assert_eq!(output, format_config(&doc, &AppConfig::default()));
        assert!(doc["accounts"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;
    #[test]
    fn shipped_template_uses_valid_flat_arrays_and_routes_to_woagent() {
        let template = include_str!("../../../../config.toml.template");
        let config: AppConfig = toml::from_str(template).unwrap();
        assert!(config.validate().is_empty(), "{:?}", config.validate());
        assert_eq!(config.proxy.active_upstream, "woagent");
        let doc: toml_edit::DocumentMut = template.parse().unwrap();
        assert!(!doc.contains_key("proxy"));
        for key in ["providers", "accounts", "upstreams", "model_pricing"] {
            assert!(doc[key].is_array());
        }
    }
}
