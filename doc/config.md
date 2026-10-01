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
    url: String,                    // API base URL（如 https://api.anthropic.com）
    token: Option<String>,          // 以 "sk-" 开头用 Bearer，否则用 x-api-key
    proxy: Option<String>,          // 独立 HTTP/SOCKS5 代理；缺省继承全局 http_proxy
    protocols: Vec<String>,         // 该 Provider 服务的协议（"anthropic" / "codex"）；空 = 全部
    codex_url: Option<String>,      // codex 协议专用端点；空 = 复用 url
    account: Option<String>,        // 引用 [[proxy.accounts]] 的 name；与 token 互斥
    models_url: Option<String>,     // 覆盖模型目录地址（自建网关常用）
    models_kind: Option<String>,    // openai | anthropic | codex | gemini | manual；空 = 推断
}
```

Provider 不再内嵌 models 字段。模型支持由 `ModelPricing.providers` 声明。

> **持久化注意**：`proxy` 段落由 `config/persist.rs` **手写**字段序列化（不是 serde round-trip）。
> 新增 Provider 字段时必须同步补 `write_proxy_section` 与读路径
> （`list_providers` / `ProviderInfo`），否则面板保存一次配置就会把该字段从
> `config.toml` 里抹掉。`protocols` / `codex_url` / `active_codex_upstream` /
> `account` 曾长期存在这个问题，已修复并有回归测试。
>
> 与之配套的规则：**读路径缺字段 = 该字段在面板上不可见、不可编辑**，
> 所以新增字段必须同时进 `write_proxy_section`、`list_providers` 和
> `ProviderInfo`（`upstream_changed` 的 WS payload）。

### Account — 上游账号（planx）

用**一个统一的账号模型**管理上游凭据，两个家族 × 两种模式都支持：

| family | `mode = "api_key"` | `mode = "plan"` |
|---|---|---|
| `gpt`（ChatGPT / Codex） | 静态密钥 → `Bearer`（`sk-` 前缀）或 `x-api-key` | Codex OAuth 刷新 + Codex CLI 身份 |
| `claude`（Anthropic） | 静态密钥 → `x-api-key` | Claude OAuth 刷新 + Claude Code 身份 + 合并 `oauth-2025-04-20` |

```rust
AccountConfig {
    name: String,                  // 被 Provider.account 引用
    family: AccountFamily,         // gpt（默认）| claude
    mode: AccountMode,             // plan（默认）| api_key

    // mode = api_key
    api_key: Option<String>,       // 静态密钥

    // mode = plan
    auth_json: Option<String>,     // Codex CLI auth.json 路径（仅 gpt，支持 ~/）
    refresh_token: Option<String>, // 可刷新的 RT（claude: sk-ant-ort01-…）
    access_token: Option<String>,  // 长期 AT / setup token（claude: sk-ant-oat01-…）
    account_id: Option<String>,    // 工作区 / 组织 UUID 覆盖
    persist: bool,                 // 刷新后原子回写 auth.json（仅 gpt）

    identity: Option<String>,      // codex_tui | codex_cli_rs | claude_code | passthrough
    impersonate: Option<String>,   // off（默认）| chrome | chrome142 | edge | firefox | safari
    cli_version: Option<String>,   // 伪装的 CLI 版本，缺省 = 内置默认
}
```

配置示例（四种组合）：

```toml
# ── GPT × plan ──
[[proxy.accounts]]
name      = "gpt-sub"
family    = "gpt"
mode      = "plan"
auth_json = "~/.codex/auth.json"
persist   = true

# ── GPT × api_key ──
[[proxy.accounts]]
name    = "gpt-key"
family  = "gpt"
mode    = "api_key"
api_key = "sk-..."

# ── Claude × plan ──
[[proxy.accounts]]
name          = "claude-sub"
family        = "claude"
mode          = "plan"
refresh_token = "sk-ant-ort01-..."   # 或 access_token = "sk-ant-oat01-..."（setup-token，无 RT）

# ── Claude × api_key ──
[[proxy.accounts]]
name    = "claude-key"
family  = "claude"
mode    = "api_key"
api_key = "sk-ant-api03-..."

# Provider 通过名字引用账号
[[proxy.providers]]
name      = "claude-plan"
url       = "https://api.anthropic.com"
protocols = ["anthropic"]
account   = "claude-sub"
```

校验规则（`AppConfig::validate`，任一条不满足即拒绝保存）：

- 账号名唯一、非空、无首尾空白；`account` 引用必须存在；
- `token` 与 `account` 互斥；
- `protocols` 里的名字必须是已知协议（`anthropic` / `codex`，大小写与别名容错）；
  Provider 声明的协议必须与所引用账号的 family 一致
  （`gpt` 账号只能给 `codex` Provider 用，反之亦然）；
- `identity` / `impersonate` 必须是已知画像名，且 `identity` 不得跨家族
  （写错不会被静默降级，而是报错）；
- `mode = plan` 至少要有一种凭据来源；`auth_json` / `persist` 仅 `family = gpt`；
- `active_upstream` / `active_codex_upstream` / `active_proxy_upstream`
  必须指向已存在的 upstream。
```

语义与约束：

- **`token` 与 `account` 互斥** —— 同时设置会被 `validate()` 拒绝（避免机制歧义）。
- 引用不存在的账号名、账号凭据与其 `mode` 不匹配，都会被 `validate()` 拒绝。
- `family = "claude"` **不支持 `auth_json`**（那是 Codex CLI 的文件格式），校验会报错。
- `persist` **仅 gpt 支持**：Claude 的凭据由 Claude Code 自己管理（keychain），
  没有可移植的凭据文件可回写。
- `identity` 会被**家族校验**：给 claude 配 `codex_tui` 会被忽略并回落到
  `claude_code`，避免产出「codex 的 UA + Anthropic 的端点」这种错配指纹。
- 出站身份头是**账号级**的；**会话头不由 planx 生成**，仍按透明语义从下游转发。
- 透明（forward proxy）模式下不注入任何认证，与既有行为一致。
- 凭据刷新：`plan` 模式到期前 5 分钟由后台任务刷新；上游 **401 时 relay 当次强制
  刷新并重放一次**（只重试一次）。`api_key` 模式无刷新，401 直接透传。
- 账号配置在**启动时**加载一次；面板改动需重启生效（P1 计划接配置变更事件）。
- `impersonate` 需要 `cargo build -p proxy-server --features impersonate`
- `cli_version` 只能填版本形态的字符串（字母数字与 `.` `-` `_`，≤32 字符）

### 两个「模型清单」是两回事

| 面 | 端点 | 内容 | 数据源 |
|---|---|---|---|
| **客户端** | `GET /v1/models`（代理口） | 客户端**能要**的模型 = 你声明的路由表 | `model_pricing[].id`，为空时回退 tier 规则里的 `model`；**离线可用** |
| **管理** | `POST /api/providers/:name/models` | 上游**自己的**目录（含推理档位、上下文窗口） | 联网抓取该 provider 的目录端点，见下 |

为什么分开：客户端发 `claude-opus-4-1` 可能被 bridge/tier 路由到别的上游模型，所以「能要什么」由**你的配置**决定；
而上游目录是「它有什么」，两者不一致本身就是有用信号——面板会把差异标出来（`only_upstream` / `only_local`）。

目录按**接入类型**读取（`models_kind`，空则由「账号家族 / URL 是否 chatgpt.com / 其余」推断为 anthropic / codex / openai）：

| kind | 路径 | 载荷 | 说明 |
|---|---|---|---|
| `openai` | `/v1/models` | `data[].id` | OpenAI 兼容网关，Bearer |
| `anthropic` | `/v1/models` | `data[].id` | 需 `anthropic-version`（账号身份自带；纯 token 时自动补），分页 |
| `codex` | `/models` | `models[].slug` | ChatGPT 后端；按 `client_version` 门控，且带 `supported_reasoning_levels` |
| `gemini` | `/v1beta/models` | `models[].name` | 名称归一化（`models/x` → `x`）。**注意：cc-proxy 的线协议只有 anthropic/codex，不能转发到 Gemini**，此项只用于看目录 |
| `manual` | — | — | 由网关决定或手动声明：**不抓取**，客户端列表就是你的配置 |

`models_url` 会完全覆盖推导出的地址（自建网关/私有部署用）；留空即按上表推导。

### 关于 `cli_version` 与身份画像

`cli_version`（缺省 `0.159.2`）会同时出现在 `User-Agent`（两处）、`version` 头和模型清单的
`client_version` 查询参数上，四处同源。

它值得单独设一个开关，是因为**上游的模型清单按版本门控**：同一个账号，
`client_version=0.153.3` 只返回 7 个模型，`0.159.2` 返回 10 个（多出 `gpt-6-sol`、
`gpt-6.1-sol`、`gpt-6-luna`）。写死在二进制里就意味着每次跟进都要重新编译。

账号加载时会读 `<auth_json 同目录>/version.json` 的 `latest_version`，如果比我们伪装的新，
就在日志里告警、并在账号面板上提示（`cli_version_stale`），照着改一下即可。

UA 里的平台串不是硬编码：发行版取自 `/etc/os-release`，架构取自 `std::env::consts::ARCH`，
终端取自 `TERM`（缺省/`dumb` 时回退 `xterm-256color`）。实测真实 TUI 发的是
`codex-tui/0.159.2 (Debian n/a; x86_64) xterm-256color (codex-tui; 0.159.2)`，
而它在 `TERM=dumb` 下**会拒绝启动**，所以伪装成 TUI 时不能报 `dumb`。
  （BoringSSL，需 cmake + C++ 工具链）；未编译时该字段被忽略。

**Claude plan 模式的两个必要细节**（来自对官方客户端的实测）：

1. OAuth 凭据调用推理接口时**必须**声明 `anthropic-beta: oauth-2025-04-20`，
   且必须与客户端已有的 beta **合并**而非替换（Claude Code 会自带
   `claude-code-20250219` 等），因此 planx 用 append 语义而非 set。
2. 刷新请求发 **JSON**（`{client_id, grant_type, refresh_token}`）且
   **不带 `scope`** —— 按 RFC 6749 §6 继承原始授权，因为网页会话授权的
   scope 可能窄于 Claude Code 的完整 scope 列表。GPT 侧则是
   `application/x-www-form-urlencoded` 且显式带 scope。

实现说明见 [doc/feat/planx.md](./feat/planx.md)。

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
    active_proxy_upstream: String,      // 透明 proxy 入口独立使用的 upstream
    active_effort: String,            // 默认 "auto"，可选 low/medium/high/xhigh/max/ultra/ultracode
    http_proxy: Option<String>,         // 全局出站 HTTP/SOCKS5 代理
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

`active_upstream` 与 `active_proxy_upstream` 可同时指向不同 upstream：相对 URI 的 relay 请求使用前者，absolute-URI 的透明 proxy 请求使用后者。任意 upstream 都可作为 proxy 当前 upstream；它仍按 tier 选择 provider，但不会改写客户端的 model、认证 header 或 JSON body。

透明 proxy 的 TierRule 允许 `model = ""`：provider 用于选择出站端点/网络代理，实际 model 直接取客户端报文。ModelPricing 是可选计费配置；缺失时任务会以未定价状态保存，不阻断转发。

### ServerConfig

```rust
ServerConfig {
    listen_address: String,  // 默认 "127.0.0.1"；必须是 IP 字面量
    http_port: u16,          // 默认 5000（Dashboard / REST / WS）
    proxy_port: u16,         // 默认 8888（反向或正向代理入口）
    auth_token: Option<String>,   // 非 loopback 时 /api 与 /ws 强制校验
    ws_include_bodies: bool,      // 默认 false
    cors_origins: Vec<String>,    // 默认空 = 不发 CORS 头；["*"] = 任意来源
}
```

**监听范围**：只有两个监听套接字（`http_port` 与 `proxy_port`），没有端口范围、端口列表或多地址；
### 浏览器（CORS）

代理口（`proxy_port`）**没有鉴权**，所以浏览器页面从别的来源调用它时，必须由服务端放行：

```toml
[server]
# 只放行这些来源（浏览器 Origin 的形态：scheme://host[:port]，不带路径）
cors_origins = ["http://192.168.31.10:3000"]
```

- 留空（默认）= 不发任何 CORS 头 → 浏览器会拦下请求（`No 'Access-Control-Allow-Origin' header`）。
- `["*"]` = 放行任意来源。**这是个坑**：代理口无鉴权，等于让操作者访问过的任意网页都能花他的上游额度。
  只在端口仅本机可达、或前面另有鉴权层时使用。
- 预检（`OPTIONS`）会由服务端直接应答，**不再转发到上游**；`allow-methods`/`allow-headers` 为 `*`，
  以覆盖浏览器客户端会发的 `authorization` / `x-api-key` / `anthropic-version` / `content-type`。
- 改这项需要**重启**（`[server]` 在启动时生效；proxy/upstream/账号等仍是热生效）。

`listen_address` 必须是 IP 字面量，**不做 DNS 解析**：

| 取值 | 含义 |
|---|---|
| `127.0.0.1`（默认） | 仅本机 |
| `0.0.0.0` | 所有 IPv4 网卡 |
| `::` | 所有 IPv6 网卡（Linux 上通常也接受 IPv4 映射） |
| `192.168.x.x` / `::1` … | 指定网卡 / 仅本机 IPv6 |

`localhost` 会被拒绝（不是 IP 字面量），错误信息会直接说明；`http_port` 与 `proxy_port`
必须不同且不能为 0。`mcp_proxy_port` 一类只出现在历史设计文档里的键**不存在于代码中**，
写了也不会监听。

#### 暴露到 loopback 之外的代价

| 端口 | 鉴权 | 说明 |
|---|---|---|
| `http_port`（Dashboard/REST/WS） | `auth_token`，**且** 便利 cookie 只发给 loopback 对端 | 远程浏览器需先打开 `http://<host>:<port>/?token=<token>` 一次（token 存 sessionStorage，见 `wwwroot/js/main.js`）；脚本用 `Authorization: Bearer <token>` |
| `proxy_port`（代理入口） | **无任何鉴权** | 它就是一个出站代理：谁能连上，谁就能借你的上游 provider 与凭据发请求 |

因此把 `listen_address` 改成 `0.0.0.0` / `::` 之前想清楚两件事：

1. `auth_token` 现在是真正的认证（历史实现会把 token 通过 `Set-Cookie` 发给任何匿名
   `GET /`，等于没有认证；已改为只发给 loopback 对端）。
2. 代理口**没有**认证，别把它暴露到不可信网络。更安全的做法是保持
   `127.0.0.1` 并用 SSH 端口转发（`ssh -L 5000:127.0.0.1:5000 user@host`），
   或在前置反向代理上做 TLS + 认证，只放行 `http_port`。

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

有效值：`auto`（透传）、`low`、`medium`、`high`、`xhigh`、`max`、`ultra`、`ultracode`

其中 `low`…`ultra` 就是上游模型清单里 `supported_reasoning_levels[].effort` 的原值；
`ultracode` 是 cc-proxy 自己的复合档（xhigh + 工作流编排），与上游取值并存。

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
