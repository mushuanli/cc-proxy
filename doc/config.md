# 配置体系

## 配置结构

```
AppConfig {
    model_pricing: Vec<ModelPricing>,   // 全局模型定价（独立于 Provider）
    proxy: ProxyConfig,
    server: ServerConfig,
    logging: LoggingConfig,
}
```

### ModelPricing — 逻辑模型定价

```rust
ModelPricing {
    id: String,                                  // 逻辑模型 ID（如 "claude-opus"）
    price: Vec<f64>,                             // [input, output] 或 [input, output, cache_write, cache_read] USD/百万 token
    providers: HashMap<String, Vec<String>>,      // Provider → 模型名列表；空 vec = 模型名等于 id；缺 key = 不支持
}
```

- `price` 只提供 2 个元素时，cache_write = input × 1.25，cache_read = input × 0.1
- `providers` 多个名字时，路由用第一个；反向匹配（按名查定价）匹配任意一个
- `model_name_for_provider(provider)` → 路由时获取 Provider 专属模型名
- `matches_name(name)` → 按逻辑 ID 或任意 Provider 模型名查找定价

### Provider — 云厂商端点

```rust
Provider {
    name: String,
    url: String,             // API base URL（如 https://api.anthropic.com）
    token: Option<String>,   // 以 "sk-" 开头用 Bearer，否则用 x-api-key
}
```

Provider 不再内嵌 models 字段。模型支持由 `ModelPricing.providers` 声明。

### TierRule — 分层路由规则

```rust
TierRule {
    keywords: Vec<String>,   // 触发关键词（大小写不敏感子串匹配）；空 = 默认 tier
    provider: String,        // 目标 Provider 名
    model: String,           // 逻辑 ID（如 "claude-opus"）或原始模型名；路由时通过 ModelPricing.providers 翻译
}
```

- `is_active()` — provider 非空且至少一个非空 keyword
- `matches(model_lower)` — 任意 keyword 是 model 的子串

### UpstreamConfig — 上游配置

```rust
UpstreamConfig {
    name: String,
    high: Option<TierRule>,
    mid: Option<TierRule>,
    low: Option<TierRule>,
    default: Option<TierRule>,
    effort: Option<String>,   // 切换到此 upstream 时自动应用；None = 不覆盖全局 effort
}
```

`resolve(request_model, session_id) -> (provider, model)` — 按 high → mid → low → default 顺序解析，返回 (provider_name, model_field)。model_field 需经 `ModelPricing.model_name_for_provider()` 翻译为最终模型名。

### ProxyConfig — 代理配置

```rust
ProxyConfig {
    active_upstream: String,
    active_effort: String,            // 默认 "auto"，可选 low/medium/high/xhigh/max/ultracode
    providers: Vec<Provider>,
    upstreams: Vec<UpstreamConfig>,
    retry_count: u32,                 // 默认 3
    request_store_capacity: usize,    // 默认 1000（RingBuffer，已不再使用）
    mcp_store_capacity: usize,        // 默认 500
    hook_store_capacity: usize,       // 默认 1000
    request_retention_hours: u32,     // 默认 8，0=不清理
    session_max_count: u32,           // 默认 20，0=不限制
    session_delete_after_days: u32,   // 默认 0，>0 时删除超龄 session
    request_timeout_secs: u64,        // 默认 120
}
```

### ServerConfig

```rust
ServerConfig {
    listen_address: String,  // 默认 "127.0.0.1"
    http_port: u16,          // 默认 5000
    proxy_port: u16,         // 默认 8888
    mcp_proxy_port: u16,     // 默认 9999
}
```

### Retention（运行时）

```rust
Retention {
    session_max_count: u32,
    request_retention_hours: u32,
    session_delete_after_days: u32,
}
```

## 配置验证

`AppConfig::validate()` 启动时检查：
1. TierRule 引用的 provider 是否存在于 `providers` 列表
2. TierRule 的 model 若是逻辑 ID，是否在 `ModelPricing.providers` 中有对应 provider 的映射
3. 返回所有错误的列表，有错误则退出

`ProxyConfig::migrate()` — 确保 `active_upstream` 指向存在的 upstream，否则回退到第一个。

## Tier 路由

```
请求 model → lower → high.keywords 匹配? → mid.keywords? → low.keywords? → default
匹配时：用 match 的 provider + model（model 经 ModelPricing 翻译为 Provider 端模型名）
```

## Effort 注入

当 `active_effort != "auto"` 时：
1. 将 `output_config.effort` 合并到请求 body JSON
2. 追加 beta header `effort-2025-11-24` 到 `anthropic-beta`

有效值：`auto`（透传）、`low`、`medium`、`high`、`xhigh`、`max`、`ultracode`

## 持久化（`persist_config()`）

触发时机：`/api/providers`、`/api/upstreams`、`/api/model-pricing`、`/api/retention`、`/api/effort` 变更

流程：
1. 读取 `config.toml` 原文件
2. 通过 `toml_edit` 更新对应 TOML 段（保留格式和注释）
3. 移除 legacy `api_target`
4. 写回磁盘
5. `broadcast_send(UpstreamChanged)` 通知所有 WS 客户端（携带 model_pricing）

Provider token 更新逻辑：
- payload 中存在 `"token"` 键且为空字符串或 null → 清除 token
- payload 中缺少 `"token"` 键 → 保留现有 token
