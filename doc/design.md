# CC Proxy — 设计文档

## 1. 概述

CC Proxy 是 Claude Code API 的透明代理，拦截、可视化、分析 AI Coding Agent 的全部 API 流量。

### 1.1 核心能力

| 能力 | 说明 |
|------|------|
| **透明代理** | 三种模式（CONNECT / Forward / Reverse），对 Claude Code 完全透明 |
| **Tier 路由** | 基于模型名的关键词匹配，将请求分发到不同 Provider 和模型 |
| **流量可视化** | 实时 WebSocket 推送 + REST API 查询，7 个前端视图 |
| **成本分析** | 按模型/Provider/Session/天 四维度聚合，Canvas 可视化 |
| **Session 管理** | 自动分组、摘要分析、归档导出、清理策略 |
| **MCP 代理** | JSON-RPC 透传，捕获和分析 MCP 工具调用 |
| **Hook 事件** | 接收 Claude Code hook 事件，在仪表盘统一查看 |
| **配置热更新** | 运行时通过 API 修改配置，自动持久化到 config.toml + EventBus 通知 |

### 1.2 技术栈

| 层 | 技术 |
|----|------|
| 语言 | Rust 2021 edition |
| 异步运行时 | tokio (multi-thread) |
| HTTP 框架 | axum 0.7 |
| HTTP 客户端 | reqwest 0.12 |
| 数据库 | SQLite (rusqlite, bundled, WAL 模式) |
| 前端 | Vanilla JS/HTML/CSS (rust-embed 内嵌) |
| 序列化 | serde + serde_json |
| 配置持久化 | toml_edit (格式保留) |
| 日志 | tracing + tracing-subscriber |

---

## 2. 架构设计

### 2.1 3 端口模型

```
:5000 ─── Dashboard (SPA + REST API + WebSocket)
:8888 ─── Anthropic API Proxy (Tier 路由)
:9999 ─── MCP Proxy (JSON-RPC 透传)
```

**设计理由**：
- 分离关注点：仪表盘、API 代理、MCP 代理各司其职
- 安全隔离：仪表盘对外暴露，代理端口仅本地监听
- Claude Code 仅需配置 `ANTHROPIC_BASE_URL` 指向 :8888

### 2.2 Crate 分层

```
proxy-hook-agent (CLI) ──POST──► proxy-server (组装层)
                                     │
                          ┌──────────┼──────────┐
                          ▼          ▼          ▼
                    proxy-common  proxy-store  proxy-relay
                    (共享类型+配置) (存储+归档)   (代理中继)
```

| Crate | 类型 | 职责 |
|-------|------|------|
| `proxy-common` | lib | 共享领域类型、ConfigStore、EventBus、响应规范化 |
| `proxy-store` | lib | SQLite 存储、Session/Task 管理、Archive YAML、内部计费、Summary 缓存 |
| `proxy-relay` | lib | 代理中继 — 收发流量、dispatch_upstream、SSE 解析、Hook 接收、录制控制 |
| `proxy-server` | bin | 组装层 — main 入口、axum 路由、WebSocket、web handler（7 模块） |
| `proxy-hook-agent` | bin | CLI：stdin → POST，独立二进制 |

### 2.3 AppState 共享状态

```rust
pub struct AppState {
    pub config: ConfigStore,     // proxy-common — 配置管理
    pub store: ProxyStore,       // proxy-store  — 存储层
    pub events: EventBus,        // proxy-common — 事件总线
    pub relay: RelayHandler,     // proxy-relay  — API 代理
    pub mcp: McpRelay,           // proxy-relay  — MCP 代理
    pub capture: CaptureControl, // proxy-relay  — 录制控制
}
```

6 个字段，每个来自独立 crate，职责清晰。

**与旧架构对比**：旧版 AppState 13 个字段直接持有 `AppConfig`、`Database`、`Vec<Provider>` 等散落状态。新版通过 `ConfigStore`、`ProxyStore`、`EventBus` 等封装统一管理，每个 crate 负责自己的状态并发安全。

### 2.4 依赖关系

```
proxy-server ──► proxy-common + proxy-store + proxy-relay
proxy-relay  ──► proxy-common + proxy-store
proxy-store  ──► proxy-common
```

`proxy-config`（内嵌在 proxy-common 中）和 `proxy-store` 之间不直接依赖，通过 proxy-server 协调。

### 2.5 并发模型

- `ConfigStore` 内部通过 `Arc<RwLock<AppConfig>>` 管理可热更新配置
- `ProxyStore` 内部封装 SQLite `Mutex<Connection>`，通过 `spawn_blocking` 卸载异步线程
- `EventBus` 基于 `broadcast::Sender` 封装，send 非阻塞，lagged 时发送 Resync
- `spawn_blocking` 用于所有 DB 操作，防止阻塞 tokio worker 线程

### 2.6 Config 与 Store 分离

**核心原则**：

```
Config = 当前应该如何执行
Store  = 历史上实际发生了什么
```

- `ConfigStore` 管理当前配置、路由规则、模型定价、TOML 持久化
- `ProxyStore` 保存历史事实：Session、Task、费用快照、Archive 文件
- Config 修改只影响未来 Task，历史 Task 不重新计算
- Task 写入时保存完整价格快照和最终费用

---

## 3. 模块设计

### 3.1 proxy-common

**目录结构**：

```
crates/proxy-common/src/
├── lib.rs              # 公共 re-export
├── models.rs           # 共享领域类型
├── response.rs         # 响应规范化
├── config/
│   ├── mod.rs          # ConfigStore + AppConfig 定义
│   ├── store.rs        # ConfigStore 实现
│   ├── loader.rs       # TOML 加载
│   ├── persist.rs      # toml_edit 持久化
│   ├── validation.rs   # 配置验证
│   ├── migration.rs    # 配置迁移
│   ├── pricing.rs      # 模型定价查找
│   └── routing.rs      # Tier 路由解析
└── core/
    └── event.rs        # EventBus
```

**职责**：
- 定义共享领域类型：SessionId、TaskId(ULID)、TaskUsage、PriceRates、BillingSnapshot、WsMessage、ProxiedRequest、SseEvent 等
- 配置管理：ConfigStore（Arc<RwLock<AppConfig>>）、加载、验证、TOML 格式保留持久化
- 事件总线：EventBus（broadcast::Sender 封装）、publish/subscribe
- 响应规范化：sanitize_text()、normalize_response()

**ConfigStore 核心接口**：

```rust
impl ConfigStore {
    pub async fn open(path: impl Into<PathBuf>) -> ConfigResult<Self>;
    pub async fn get(&self) -> AppConfig;
    pub async fn update<F>(&self, updater: F) -> ConfigResult<AppConfig>
        where F: FnOnce(&mut AppConfig) -> ConfigResult<()>;
    pub async fn persist(&self) -> ConfigResult<()>;
    pub async fn resolve_route(&self, request_model: &str) -> ConfigResult<ResolvedRoute>;
    pub async fn resolve_billing(&self, provider: &str, model: &str) -> ConfigResult<BillingSnapshot>;
}
```

**设计决策 — ConfigStore 封装**：
- 内部持有 `Arc<RwLock<AppConfig>>`，所有读写通过公开方法
- 配置更新自动触发 validate + persist + broadcast
- 路由和定价快照由 ConfigStore 统一提供，避免调用方直接操作配置结构

### 3.2 proxy-store

**目录结构**：

```
crates/proxy-store/src/
├── lib.rs              # ProxyStore 公开 API
├── error.rs            # 错误类型
├── billing.rs          # 内部计费 calculate_cost_microusd()
├── db/
│   ├── mod.rs          # Database 封装
│   ├── connection.rs   # SQLite 连接管理
│   ├── migration.rs    # Schema 迁移
│   ├── sessions.rs     # Session CRUD
│   ├── tasks.rs        # Task CRUD
│   └── usage.rs        # daily_usage CRUD
├── archive/
│   ├── mod.rs          # ArchiveManager
│   ├── format.rs       # YAML 格式定义
│   └── file.rs         # 原子文件写入
└── summary/
    ├── mod.rs
    └── cache.rs         # Summary 缓存
```

**职责**：
- SQLite 初始化、Schema 迁移、连接管理
- Session 自动创建、Task 写入（原子事务：三表写入）
- 内部计费：Task 写入时基于 BillingSnapshot + TaskUsage 计算最终费用
- Archive YAML：原子写入（临时文件 → rename），Session 快照
- Summary 缓存：延迟分析、持久化到 tasks.summary_json
- Task 清理：基于 retention 策略删除已归档的历史 Task

**ProxyStore 核心接口**：

```rust
impl ProxyStore {
    pub fn open(config: ProxyStoreConfig) -> StoreResult<Self>;
    pub fn write(&self, session_id: &SessionId, task: NewTask) -> StoreResult<Task>;
    pub fn info(&self, task_id: &TaskId) -> StoreResult<Task>;
    pub fn list_sessions(&self, filter: SessionFilter) -> StoreResult<Vec<SessionListItem>>;
    pub fn list_tasks(&self, session_id: &SessionId, time_range: Option<TimeRange>) -> StoreResult<Vec<TaskListItem>>;
    pub fn name(&self, session_id: &SessionId, new_name: Option<&str>) -> StoreResult<Session>;
    pub fn archive(&self, session_ids: Option<&[SessionId]>, options: ArchiveOptions) -> StoreResult<Vec<ArchiveInfo>>;
    pub fn summary(&self, task_id: &TaskId) -> StoreResult<TaskSummary>;
    pub fn list_archives(&self, filter: Option<&str>) -> StoreResult<Vec<ArchiveInfo>>;
}
```

**SQLite 表结构**（datav2.db）：

| 表 | 主键 | 说明 |
|----|------|------|
| `sessions` | id (TEXT) | 会话，含 client_type/client_session_id/cwd/project_key/累计统计/archive 状态 |
| `tasks` | id (TEXT, ULID) | 请求记录，含完整价格快照（4 个 rate）+ 最终费用 + 时序 + 状态 |
| `session_daily_usage` | (date, session_id, provider, model, currency) | 每日聚合统计，Task 写入时 upsert 累加 |

Task 表在设计上保存价格快照和最终费用，历史 Task 不因配置变更重新计算。

**设计决策 — 内部计费**：
- Task 写入时 store 内部计算 `calculate_cost_microusd(usage, rates)` → 写入 cost_microusd
- 调用方只需提供 BillingSnapshot（来自 ConfigStore）和 TaskUsage（来自 relay）
- 计费发生在 store 内部，确保费用计算与写入在同一事务边界

**设计决策 — Archive YAML 原子写入**：
- 写入临时文件 → flush → sync_all → rename 为正式文件
- file rename 是文件系统原子操作，保证不会产生残缺文件
- 写入成功后更新 session 的 archive checkpoint（last_archived_at、archive_dirty=0）
- 写入失败不更新 checkpoint，下次重试

**设计决策 — Task 清理**：
- 只清理同时满足 4 条件的 Task：已结束、已归档（sequence ≤ last_archived_sequence）、超过 retention 时间、非 recording 状态
- 清理不影响 Session 累计费用、Dayly Usage、Archive 文件

### 3.3 proxy-relay

**目录结构**：

```
crates/proxy-relay/src/
├── lib.rs              # 公共 re-export
├── relay.rs            # RelayHandler — :8888 API 代理
├── mcp.rs              # McpRelay   — :9999 MCP 代理
├── hook.rs             # HookReceiver — Hook 事件接收
├── capture.rs          # CaptureControl — 录制开关
├── upstream.rs         # dispatch_upstream + 重试 + 超时
└── sse.rs              # SseParser — SSE 字节流解析
```

**职责**：
- API 代理：三种模式（CONNECT/Forward/Reverse），Tier 路由、effort 注入、模型翻译
- MCP 代理：JSON-RPC 透传、header 过滤
- Hook 接收：接收 proxy-hook-agent POST 的 hook 事件
- 录制控制：打开/关闭原始流量录制到 captures/ 目录
- 上游分发：dispatch_upstream、重试（指数退避）、超时控制
- SSE 解析：`\n\n` / `\r\n\r\n` 分隔，Anthropic 事件字段提取

**RelayHandler 核心接口**：

```rust
impl RelayHandler {
    pub fn new(config: ConfigStore, store: ProxyStore, events: EventBus, client: reqwest::Client) -> Self;
    pub fn build_router(self) -> axum::Router;  // :8888
}
```

**请求处理流程**：
1. 提取 session_id → store.write() 自动创建 Session
2. config.resolve_route(model) → ResolvedRoute
3. config.resolve_billing(provider, model) → BillingSnapshot
4. Effort 注入 + beta header
5. dispatch_upstream() → 流式 SSE 解析 / 非流式缓冲
6. store.write(sid, NewTask { billing, usage }) → 原子写入三表
7. events.publish(NewRequest) → EventBus → WebSocket → 浏览器

**设计决策 — 数据流分层**：
- relay 负责网络 I/O：收发 HTTP 流量、解析 SSE、管理重试
- relay 调用 config 获取路由和定价快照
- relay 调用 store 写入持久化数据
- relay 发布事件到 EventBus（不直接管理 WebSocket 连接）

### 3.4 proxy-server

**目录结构**：

```
crates/proxy-server/src/
├── main.rs       # 入口 + AppState 组装 + cleanup_loop
└── web/
    ├── mod.rs           # build_router()
    ├── sessions.rs      # Session 列表/详情/导出
    ├── requests.rs      # Task 列表/详情/删除
    ├── settings.rs      # 配置 CRUD（providers/upstreams/model-pricing/retention）
    ├── costs.rs         # 成本聚合查询
    ├── archive.rs       # Archive 文件列表/搜索/读取
    ├── health.rs        # 健康检查
    └── static_files.rs  # rust-embed 静态文件服务
```

**职责**：
- 入口点：CLI 解析、ConfigStore/ProxyStore/EventBus 初始化、3 端口启动
- Web handler：~30+ REST 端点，分为 7 个模块
- WebSocket 转发：订阅 EventBus → 推送到浏览器
- cleanup_loop：每 30 分钟触发 Archive + Task 清理
- 静态文件服务：rust-embed 内嵌前端 SPA

**设计决策 — Web 模块拆分**：
- 每个前端视图对应一个 web handler 模块
- handler 只做参数解析和 JSON 序列化，调用 store/config 完成业务逻辑
- 不直接在 handler 中操作 SQLite 或配置结构

### 3.5 proxy-hook-agent

独立 CLI 二进制，从 stdin 读取 Claude Code hook JSON，POST 到仪表盘：

```
stdin Hook JSON → POST /api/hook-event → store.insert_hook() → events.publish(NewHook)
```

- 通过 `--dashboard-url` 参数指定仪表盘地址
- 静默失败：POST 失败时不阻塞 Claude Code 流程
- 独立编译为小二进制（~2MB），无 axum 依赖

---

## 4. 接口设计

### 4.1 Crate 边界接口

```
proxy-common 导出:
  models:   SessionId, TaskId(ULID), TaskUsage, PriceRates, BillingSnapshot,
            NormalizedResponse, WsMessage, ProxiedRequest, SseEvent
  config:   ConfigStore, AppConfig, ModelPricing, Provider, TierRule,
            UpstreamConfig, ResolvedRoute, Retention
  response: sanitize_text(), normalize_response()
  event:    EventBus

proxy-store 导出:
  ProxyStore, ProxyStoreConfig, NewTask, SessionFilter, ArchiveOptions,
  SessionListItem, TaskListItem, Task, Session, ArchiveInfo, TaskSummary,
  StoreResult, SessionId, TaskId（re-export）

proxy-relay 导出:
  RelayHandler, McpRelay, HookReceiver, CaptureControl
```

### 4.2 REST API

全部端点在 Dashboard 端口（默认 :5000）。完整列表见 [API 文档](./api.md)。

**设计原则**：
- RESTful 资源命名：`/api/sessions`, `/api/request/:id`
- 配置变更自动 `persist_config()` + `events.publish(UpstreamChanged)`
- 批量删除使用 `{ids: []}` JSON body
- 导出端点设置 `content-disposition` header
- 列表查询支持 `?q=` 搜索 + `?from=&to=` 时间范围 + `?limit=` 分页

### 4.3 WebSocket

路径：`/ws`，消息格式：`{type: string, payload: object}`

**设计原则**：
- Tagged union 序列化（serde JSON），前端 switch 分发
- 首态不含请求历史（由 REST 加载），避免大量数据导致超时
- 基于 EventBus：relay 发布事件 → ws handler 订阅转发
- 非阻塞 send（broadcast::channel），lagged 时发送 `Resync` 提示客户端重取
- Ping/Pong 心跳 + 死连接检测 + 分级警告

### 4.4 代理接口

三种代理模式：

| 模式 | 触发条件 | 输入 | 输出 |
|------|---------|------|------|
| CONNECT | `method == CONNECT` | host:port | 双向 TCP 隧道 |
| Forward | URI 含 scheme | 完整 URL | 上游响应 |
| Reverse | URI 不含 scheme | 相对路径 | 上游响应 |

公共处理流程（Forward/Reverse 共用）：
1. 提取 `session_id` → store.write() 自动创建或复用 Session
2. `config.resolve_route(model)` → (provider, resolved_model)
3. `config.resolve_billing(provider, model)` → BillingSnapshot
4. Effort 注入 + beta header
5. `dispatch_upstream()` → 重试 → SSE 解析 → store.write() → events.publish()

---

## 5. 数据流

### 5.1 请求代理流

```
Claude Code → relay (:8888)
  → config.resolve_route(model)            → ResolvedRoute
  → config.resolve_billing(provider, m)    → BillingSnapshot
  → dispatch_upstream() → reqwest → 上游 API
  │   ├── 流式: SseParser 实时解析 → events.publish(SseEvent)
  │   └── 非流式: 缓冲完整响应 → 提取 usage stats
  → store.write(sid, NewTask { billing, usage })
  │   └── store 内部: calculate_cost_microusd() → 原子写入三表
  → events.publish(NewRequest / RequestUpdated)
  → ws handler 订阅 → WebSocket → 浏览器
```

### 5.2 清理/归档流

```
cleanup_loop (每 30min, 启动时立即执行)
  → config.get().proxy.request_retention_hours
  → store.archive(None, ArchiveOptions { task_retention_hours, force: false })
      │   └── 遍历 archive_dirty = 1 的 Session
      │       ├── 生成 YAML 快照（原子写入 data/archives/{sid}.yaml）
      │       ├── 更新 Session archive checkpoint
      │       └── 删除已归档的超期 Task
```

### 5.3 配置变更流

```
API handler → ConfigStore.update()
  → validate() → toml_edit 更新 config.toml
  → events.publish(UpstreamChanged)
  → ws handler → WebSocket → 前端刷新 provider/upstream/effort 下拉
```

### 5.4 Hook 事件流

```
Claude Code hook → proxy-hook-agent (stdin JSON)
  → POST /api/hook-event → store.insert_hook()
  → events.publish(NewHook) → WebSocket → 前端实时显示
```

---

## 6. 横切关注点

### 6.1 错误处理

| 场景 | 策略 |
|------|------|
| **上游 API 错误** | 透传状态码和错误信息，不重试（4xx/5xx） |
| **连接/超时错误** | 指数退避重试（最多 3 次） |
| **DB 写入失败** | 记录 tracing::error，不阻塞请求 |
| **broadcast lagged** | 发送 Resync 消息，前端 REST 重取 |
| **Hook 发送失败** | 静默退出，不阻塞 Claude Code |
| **配置验证失败** | 启动时 fatal error，列出所有错误 |

### 6.2 安全

| 措施 | 说明 |
|------|------|
| 仅监听 127.0.0.1 | 所有端口仅本地访问 |
| Token 脱敏 | Authorization/x-api-key → [REDACTED] |
| Token 不可见 | 前端仅 `has_token: bool` |
| 请求体剥离 | flush_yaml 去除 tool 定义和 system prompt |

### 6.3 性能

| 措施 | 说明 |
|------|------|
| `spawn_blocking` | DB 操作不阻塞 tokio worker |
| SSE 批量广播 | 100ms 积攒，减少消息数 |
| 请求列表不含 body | 列表查询省略 request_body 和 response_body |
| WAL 模式 | 读写并发（SQLite WAL） |
| `busy_timeout = 5s` | 等待 WAL checkpoint 而非立即返回 BUSY |
| reqwest 连接池 | `pool_idle_timeout = 90s` |

### 6.4 可观测性

| 措施 | 说明 |
|------|------|
| `tracing` | 结构化日志，前缀 `[relay]`/`[store]`/`[api]`/`[archive]` |
| 健康检查 | `GET /api/health` 返回各表计数 |
| 调试日志 | 携带 session/request ID 前 8 字符上下文 |

---

## 7. 事件驱动通信

### 7.1 EventBus

EventBus 封装了 `broadcast::Sender`，提供统一的 publish/subscribe 接口：

| 发布者 | 事件 | 订阅者 |
|--------|------|--------|
| `proxy-relay::relay` | NewRequest, RequestUpdated, SseEvent | proxy-server::ws |
| `proxy-relay::hook` | NewHook | proxy-server::ws |
| `proxy-relay::mcp` | NewMcp, McpConfigChanged | proxy-server::ws |
| `proxy-relay::capture` | TeeStatusChanged | proxy-server::ws |
| `proxy-server::web` | UpstreamChanged | proxy-server::ws |

**设计理由**：
- 解耦 relay 和 ws：relay 不直接管理 WebSocket 连接池
- relay 专注于 HTTP 流量处理，ws 专注于事件转发
- 新增订阅者不需要修改发布者代码

### 7.2 Resync 机制

- `broadcast::channel` 容量 256，满了后 lagged 的 receiver 收到 Lagged 错误
- EventBus 发送 `Resync` 消息触发前端通过 REST 重新同步
- 保证在网络抖动时不会丢消息

---

## 8. 设计决策汇总

| 决策 | 选择 | 理由 |
|------|------|------|
| 数据库 | SQLite (WAL) | 零配置、嵌入式、足够性能、单文件备份 |
| Crate 分层 | 5 个独立 crate | 职责分离、独立编译、单元测试不需启动服务器 |
| Config/Store 分离 | 独立两个抽象 | Config 管当前规则，Store 管历史事实，不互相依赖 |
| 计费策略 | Task 写入时保存价格快照 | 后续改价不影响历史费用，无需版本管理 |
| 并发模型 | Arc<RwLock> + spawn_blocking | 读多写少优化，DB 不阻塞 async |
| 事件驱动 | EventBus (broadcast::channel) | 解耦 relay 和 ws，新增订阅者无需修改发布者 |
| 前端 | Vanilla JS | 无构建步骤、rust-embed 内嵌、规模可控 |
| 配置格式 | TOML (toml_edit) | 人类友好、保留注释、Rust 原生 |
| 模型定价 | 独立 ModelPricing | Provider 和模型解耦、避免冗余 |
| Tier 路由 | 关键词子串匹配 | 简单、灵活、新模型自动适配 |
| SSE 解析 | Rust 端实时解析 | 减少前端 CPU、字段提前提取 |
| Archive | YAML 文件 + session_daily_usage 表 | 文件可手动查看、表用于聚合查询 |
| 成本去重 | UNION ALL + NOT IN | 活跃+归档数据合并，防止跨边界双计 |
| 配置持久化 | toml_edit 格式保留 | 保留用户注释和格式 |
| MCP 代理 | 简单透传 | 不解析语义，保持兼容性 |
| Hook 代理 | 独立 CLI + 静默失败 | 不阻塞 Claude Code 执行 |
