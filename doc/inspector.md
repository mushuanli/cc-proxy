# Inspector 页面 — Session List 事件流 & 接口

## 页面概述

Inspector（会话管理）是 CC Proxy 的核心视图，以**会话（Session）** 为单位分组展示所有被代理的 API 请求（Request/Task）。
支持折叠展开、模型/时间筛选、分页、多选（导出/删除）、请求详情查看与会话摘要面板。

---

## 整体数据流

RelayHandler 同时接收两类入口：相对 URI 的 relay 请求使用 `active_upstream`，absolute URI 的透明 proxy 请求使用 `active_proxy_upstream`。二者都会写入相同的 Session/Task 表并发布相同的 `NewRequest` 事件，因此 Inspector 无需区分存储来源。Codex Responses 报文会标记 `client_type=codex`、`request_type=codex`，并持久化原始 response/SSE events 供详情页 inspect。

```
┌─────────────────────────────────────────────────────────────────┐
│                        Claude Code Client                        │
└──────────────────────────────┬──────────────────────────────────┘
                               │ HTTP (Anthropic API)
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│  proxy-relay (RelayHandler)                                      │
│  - 代理请求 → store.write(session_id, task)                      │
│  - 发布 EventBus: WsMessage::NewRequest / CostUpdated            │
└────────────────────────┬─────────────────────────────────────────┘
                         │ EventBus (broadcast channel, cap=256)
                         ▼
┌──────────────────────────────────────────────────────────────────┐
│  proxy-server (ws.rs)                                            │
│  - 订阅 EventBus → 序列化 WsMessage → WebSocket Text 帧          │
│  - 连接时发送初始快照 (McpConfigChanged, TeeStatusChanged)       │
│  - Lagged → 发送 Resync 通知前端全量重拉                         │
└────────────────────────┬─────────────────────────────────────────┘
                         │ WebSocket (ws://host/ws)
                         ▼
┌──────────────────────────────────────────────────────────────────┐
│  前端 (main.js → inspector.js)                                   │
│  - handleMessage() 分发 WsMessage → upsertRequestRow()           │
│  - 补齐 session 元数据: fetchSessionMeta(sid) → GET /api/session │
│  - renderPage() 按 session 分组渲染                              │
└──────────────────────────────────────────────────────────────────┘
```

### 关键设计点

1. **Session 是隐式创建的** — 没有 `/api/session/start` 接口。当 `store.write()` 首次写入某个 session_id 的 task 时，通过 `ensure_session()` 自动创建 session 行。
2. **没有 Session 级别的 WebSocket 事件** — `WsMessage::SessionStarted` / `SessionStopped` / `SessionUpdated` 在代码中已定义但从未发布，属于死代码。
3. **前端通过 REST 轮询发现新 session** — 初始化时调用 `GET /api/sessions` 拉取全量 session 元数据，收到 `NewRequest` 时通过 `fetchSessionMeta(sid)` 补齐。
4. **Resync 机制** — EventBus channel 容量 256，当 WebSocket 接收端落后时，服务端发送 `Resync` 消息，前端全量重拉 `GET /api/requests?limit=2000`。

---

## 一、初始化加载流程

```
页面加载
  │
  ├─① GET /api/sessions
  │    → 填充 state.sessionMeta[sid] 和 state.sessionCache[sid]
  │
  ├─② GET /api/requests?limit=2000
  │    → 合并填充 state.requestRows (Map<id, req>)，已有字段优先不覆盖
  │    → getSessionGroups() 按 session_id 分组
  │    → 自动展开最新 session: expandedSessions.add(groups[0].session_id)
  │    → renderPage() + updateRequestCount()
  │    → updateFilterOptions()  (填充模型下拉)
  │    → refreshInspectorCostStatsNow()  (更新工具栏费用统计)
  │
  ├─③ WebSocket connect → ws://host/ws
  │    → onopen 后服务端推送初始快照:
  │       - WsMessage::McpConfigChanged { destination_url }
  │       - WsMessage::TeeStatusChanged { enabled }
  │    → 进入主循环，接收实时事件
  │
  ├─④ GET /api/upstreams (Settings 面板数据)
  ├─⑤ GET /api/mcp-destination
  ├─⑥ GET /api/capture/status
  └─⑦ GET /api/retention (保留策略)
```

### 涉及的前端状态

| 字段 | 类型 | 说明 |
|---|---|---|
| `state.requestRows` | `Map<id, req>` | 所有活跃请求的内存缓存 |
| `state.sessionMeta` | `{sid: obj}` | session 元数据（从 `/api/sessions` 或 `/api/session/:id` 获取） |
| `state.sessionCache` | `{sid: label}` | session 显示名缓存（label 或 shortSid） |
| `state.expandedSessions` | `Set<sid>` | 当前展开的 session 集合 |
| `state.currentSelectedSession` | `sid \| null` | 当前选中的 session（控制摘要面板） |
| `state.pendingSessionFetches` | `Set<sid>` | 正在 fetch 中的 session（防抖去重，完成前阻止重复请求） |
| `state.queuedSessionFetches` | `Set<sid>` | fetch 期间到达的新事件对应的 session，完成后立即发起新请求（防止新 task 事件丢失） |
| `state._renderPageTimer` | `number\|null` | renderPage 防抖定时器 ID |
| `state._updateFilterTimer` | `number\|null` | updateFilterOptions 防抖定时器 ID |

---

## 二、实时更新流程 (WebSocket)

### 2.1 代理请求完成 → 前端更新

```
RelayHandler::proxy_request()
  │
  ├─ 成功: publish(WsMessage::NewRequest(ProxiedRequest))
  └─ 失败: publish(WsMessage::RequestUpdated(ProxiedRequest { error }))
  └─ 都会: publish(WsMessage::CostUpdated(stats))
         │
         ▼ (EventBus broadcast)
  ws.rs 主循环 rx.recv()
         │
         ▼ (WebSocket Text frame)
  main.js handleMessage()
         │
         ├─ case 'NewRequest' / 'RequestUpdated':
         │    upsertRequestRow(payload)
         │    │  → 合并写入 state.requestRows（现有字段 + 新字段，不覆盖丢失数据）
         │    │  → 防抖后 renderPage()（新请求立即，更新请求 100ms）
         │    │  → 500ms 防抖后 updateFilterOptions()
         │    ├─ addToTimeline(payload)  → 对话时间线视图
         │    ├─ updateRequestCount()    → 状态栏计数
         │    └─ fetchSessionMeta(sid)   → GET /api/session/:sid
         │         │
         │         ├─ 防抖：pendingSessionFetches 去重，queuedSessionFetches 排队
         │         ├─ 填充 state.sessionMeta[sid]
         │         ├─ 填充 state.sessionCache[sid]
         │         ├─ 补齐 requestRows（合并 REST 列表 tasks，WS 已有字段优先）
         │         └─ renderPage() + updateRequestCount()
         │
         └─ case 'CostUpdated':
              window._applyCostStats(payload) → 更新工具栏费用统计
```

### 2.2 Resync 流程

```
ws.rs: rx.recv() → Err(RecvError::Lagged(n))
  │
  ├─ 服务端: send_json(WsMessage::Resync)
  └─ 前端 handleMessage():
       case 'Resync':
         GET /api/requests?limit=2000
         → state.requestRows.clear()
         → 重新填充 state.requestRows
         → renderPage() + updateRequestCount() + updateFilterOptions()
```

### 2.3 WebSocket 连接管理

- **心跳**: 服务端每 10s 发 Ping，前端浏览器自动回 Pong
- **超时**: 300s 无 Pong → 服务端断开连接
- **重连**: 前端指数退避（初始 1s，翻倍至最大），最大重试间隔状态变量控制
- **静默检测**: 前端每 5s 检查最后收消息时间，>180s 显示警告

---

## 三、Session 分组 & 渲染

### 3.1 getSessionGroups() 分组逻辑

```
1. getFilteredRequests()
   ├─ 过滤: model != filterModel → skip
   ├─ 过滤: !session_id → skip (必须属于某 session)
   └─ 过滤: timestamp 不在时间范围内 → skip
   └─ 按 timestamp 降序排列

2. 构建 groupsMap (Map<sid, group>)
   ├─ 遍历 filtered requests，按 session_id 分组
   └─ group 字段:
        session_id, label, requests[], totalIn, totalOut,
        totalCost, firstTime, lastTime, models (Set),
        archived (bool), request_count

3. 合并 state.sessionMeta
   ├─ sessionMeta 中有但 groupsMap 中没有 → 创建 archived group (requests=[])
   └─ sessionMeta 中有且 groupsMap 中也有 → 回填 started_at，补充 request_count

4. 计算聚合值
   ├─ 非 archived: 遍历 requests 计算 totalIn/Out/Cost，收集 models
   └─ 全部: 按 lastTime 降序排列
```

### 3.2 renderPage() 四种渲染模式

| 条件 | 渲染方式 | 交互 |
|---|---|---|
| `session_id === '__no_session__'` | 平铺请求行，无折叠 | 点击行 → showRequestDetail |
| `requests.length === 0` (archived) | 紧凑归档行 + 归档徽章 | 点击 → selectSession → 打开摘要面板 |
| `requests.length === 1` | 平铺请求行 (无折叠头) | 点击 → selectSession + showRequestDetail |
| `requests.length > 1` | 可折叠组：header 行 + 子行 | 三角图标 → toggleSession；header 点击 → selectSession；子行点击 → showRequestDetail |

### 3.3 分页

- **分页单位**: Session 组（不是单个请求）
- `pageSize` 默认 50，可选 20/50/100/200
- `currentPage` 切换时仅渲染当前页的 session 组
- 点击请求详情时，自动跳转到该请求所在 session 的页码

### 3.4 筛选

| 筛选器 | 前端状态 | 触发方式 |
|---|---|---|
| 模型 | `state.filterModel` ('' / '__has_model__' / 具体模型名) | `#filter-model` change → applyFiltersAndRender() |
| 起始时间 | `state.filterTimeFrom` | `#filter-time-from` change |
| 结束时间 | `state.filterTimeTo` | `#filter-time-to` change |

筛选变更时 `currentPage` 重置为 1。

---

## 四、请求详情 & 摘要面板

### 4.1 请求详情 (showRequestDetail)

```
点击请求行
  ├─ state.selectedRequestId = req.id
  ├─ 自动展开所属 session
  ├─ GET /api/request/:id  (获取完整 body)
  │    → 回写 state.requestRows (补充 body 用于摘要渲染)
  │    → updateDetailView() 渲染 detail-content
  └─ renderPage() (高亮选中行，跳转页码)
```

详情面板三个 Tab:
- **Request**: 请求头 + JSON body (jsonTreeHTML 渲染)
- **Response**: 响应头 + JSON body
- **SSE Events**: 流式事件（thinking / text / tool_calls / tool_results，或旧格式 sse_events）

### 4.2 会话摘要面板 (selectSession → session.js)

```
点击 session header
  ├─ state.currentSelectedSession = sid
  ├─ state.expandedSessions.add(sid)
  ├─ renderPage()
  └─ openSummaryPanel(sid)
       ├─ GET /api/session/:id/summary
       └─ 渲染 summary-content (模型、tokens、工具调用、文件触碰等)
```

---

## 五、前端状态完整清单

| 状态 | 类型 | 说明 |
|---|---|---|
| `requestRows` | `Map<id, req>` | 所有请求的内存缓存（含 WS 实时 + REST 补齐，merge 写入不丢失字段） |
| `sessionMeta` | `{sid: obj}` | session 元数据（label, tokens, cost, request_count, started_at, ended_at 等） |
| `sessionCache` | `{sid: label}` | session 显示名（label 优先，否则 shortSid） |
| `expandedSessions` | `Set<sid>` | 当前展开的 session（控制子行 visible） |
| `currentSelectedSession` | `sid \| null` | 当前选中 session（控制摘要面板） |
| `selectedIds` | `Set<req_id>` | 选中的请求 ID（用于批量删除/导出） |
| `selectedSessionIds` | `Set<sid>` | 选中的 session ID（用于批量删除/导出/Flush） |
| `selectedRequestId` | `id \| null` | 当前详情面板展示的请求 |
| `pendingSessionFetches` | `Set<sid>` | 正在 fetch 的 session（防抖去重） |
| `queuedSessionFetches` | `Set<sid>` | fetch 期间到达的事件 session，完成后立即重取 |
| `_renderPageTimer` | `number\|null` | renderPage 防抖定时器 ID |
| `_updateFilterTimer` | `number\|null` | updateFilterOptions 防抖定时器 ID |
| `filterModel` | `string` | 模型筛选值 |
| `filterTimeFrom` / `filterTimeTo` | `string` | 时间筛选（datetime-local 格式） |
| `currentPage` | `number` | 当前页码（以 session 组计） |
| `pageSize` | `number` | 每页 session 组数（默认 50） |

---

## 六、REST API 端点参考

### Session 相关

| 方法 | 路径 | 说明 | 前端调用时机 |
|---|---|---|---|
| `GET` | `/api/sessions` | 列出所有 session（轻量，仅 `SessionListItem` 字段 + 别名） | 初始化① |
| `GET` | `/api/session/:id` | 获取单个 session 详情 + 其下所有 tasks | `fetchSessionMeta()` |
| `PUT` | `/api/session/:id` | 重命名 session `{"label": "..."}` | 摘要面板重命名按钮 |
| `DELETE` | `/api/session/:id` | **未实现**，返回 `{"ok":false}` | 删除按钮（实际无效） |
| `GET` | `/api/session/:id/export` | 导出 session 的 tasks 为 JSON | 导出选中 → JSON 下载 |
| `GET` | `/api/session/:id/summary` | 获取 session 结构化摘要（最新 task 的 summary） | `openSummaryPanel()` |
| `POST` | `/api/flush` | 导出选中 session 到磁盘 `{"session_ids": [...]}` | Flush 按钮 |

### Request/Task 相关

| 方法 | 路径 | 说明 | 前端调用时机 |
|---|---|---|---|
| `GET` | `/api/requests?limit=2000` | 列出所有 tasks（遍历所有 session 收集，截断到 limit） | 初始化②、Resync |
| `GET` | `/api/requests?session_id=X` | 列出指定 session 的 tasks | （未直接使用，通过 `/api/session/:id` 间接获取） |
| `GET` | `/api/request/:id` | 获取单个 task 完整详情（含 headers、body） | `showRequestDetail()` |
| `DELETE` | `/api/request/:id` | **未实现** | 单行删除按钮 |
| `DELETE` | `/api/requests` | **未实现**（批量删除） | 批量删除按钮 |
| `GET` | `/api/request/:id/summary` | 获取 task 结构化摘要 | （预留，前端未使用） |

### 其他相关

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/ws` | WebSocket 升级端点 |
| `GET` | `/api/costs?period=daily\|monthly\|session` | 费用统计（Cost 视图使用，非 Inspector 核心） |
| `POST` | `/api/cleanup` | 手动触发数据清理 |
| `POST` | `/api/flush-all` | 导出全部 session 到磁盘 |

---

## 七、WsMessage 类型速查

| 类型 | Payload | 发布者 | Inspector 是否使用 |
|---|---|---|---|
| `NewRequest` | `ProxiedRequest` | `RelayHandler` (成功) | 是 — 核心事件 |
| `RequestUpdated` | `ProxiedRequest` | `RelayHandler` (失败) | 是 — 核心事件 |
| `CostUpdated` | `CostStats` | `RelayHandler` (每次写入后) | 是 — 工具栏费用 |
| `Resync` | 无 | `ws.rs` (Lagged 时) | 是 — 全量重拉 |
| `SseEvent` | `{request_id, event}` | 无（未发布） | 否（预留） |
| `NewHook` | `HookEvent` | `HookReceiver` | 否（Hook 视图用） |
| `NewMcp` | `ProxiedRequest` | `McpRelay` | 否（MCP 视图用） |
| `Cleared` | 无 | `HookReceiver` | 是 — clearAllTables() |
| `McpConfigChanged` | `{destination_url}` | `McpRelay` | 否（MCP 视图用） |
| `TeeStatusChanged` | `{enabled}` | `CaptureControl` | 否（Capture 用） |
| `UpstreamChanged` | 复杂结构 | `settings.rs` (配置变更) | 否（Settings 用） |
| `History` / `HookHistory` / `McpHistory` | 列表 | 无（未发布） | 否（向后兼容预留） |
| **`SessionStarted`** / **`SessionStopped`** / **`SessionUpdated`** | Session / `{request_id}` | **从未发布（死代码）** | **否** |

---

## 八、关键文件索引

| 文件 | 职责 |
|---|---|
| `wwwroot/js/inspector.js` | Session 分组、渲染、筛选、分页、请求详情、全屏、选中/删除/导出 |
| `wwwroot/js/main.js` | WebSocket 连接/重连、消息分发、初始化加载、`fetchSessionMeta()` |
| `wwwroot/js/state.js` | 共享状态（requestRows, sessionMeta, expandedSessions 等） |
| `wwwroot/js/session.js` | 会话摘要面板（openSummaryPanel, 重命名, 删除, 导出） |
| `wwwroot/js/cost.js` | Inspector 工具栏费用统计 |
| `wwwroot/index.html:35-105` | Inspector 视图 HTML 结构 |
| `crates/proxy-server/src/web/sessions.rs` | Session REST API（list/get/rename/delete/summary/export） |
| `crates/proxy-server/src/web/requests.rs` | Request REST API（list/get/delete/summary） |
| `crates/proxy-server/src/ws.rs` | WebSocket 处理（连接、心跳、EventBus→WS 转发、Resync） |
| `crates/proxy-common/src/models.rs` | `WsMessage` 枚举、`ProxiedRequest`、`Session`、`CostStats` |
| `crates/proxy-common/src/core/event.rs` | `EventBus`（broadcast channel 封装） |
| `crates/proxy-relay/src/relay.rs` | `RelayHandler` — 代理请求、写 store、发布 WsMessage |
| `crates/proxy-store/src/store.rs` | `ProxyStore::write()` — 自动 create session、写 task、更新聚合 |
| `crates/proxy-store/src/db/sessions.rs` | Session DB 操作（ensure_session, list_sessions, 聚合更新） |

---

## 九、Proxy 配置事件流

### 9.1 配置架构总览

```
config.toml (磁盘)
     │
     ▼ (启动时 load, 变更时 persist)
ConfigStore (Arc<RwLock<AppConfig>>)
     │
     ├── 读路径: relay.config.resolve_route() 每次请求读取
     └── 写路径: state.config.update(|c| { ... })
           │
           ├── 1. 获取写锁 → 修改内存
           ├── 2. validate() → 校验合法性
           ├── 3. persist_config() → 写入 config.toml (toml_edit 保格式)
           └── 4. 返回新快照
                │
                ▼
         state.events.publish(WsMessage::UpstreamChanged { ... })
                │
                ▼ (EventBus broadcast)
         所有 WebSocket 客户端 → applyUpstreamState() → UI 刷新
```

### 9.2 核心数据结构

```
AppConfig
├── model_pricing: Vec<ModelPricing>   — 模型定价表
│     ├── id: "claude-sonnet"
│     ├── price: [3.0, 15.0, 3.75, 0.3]  // [input, output, cache_write?, cache_read?] USD/1M tokens
│     └── providers: {"anthropic": ["claude-sonnet-4-6"]}
│
├── proxy: ProxyConfig
│     ├── active_upstream: "default"    — 当前激活的上游名称
│     ├── active_effort: "auto"         — 全局 effort 级别
│     ├── http_proxy: Option<String>    — 全局 HTTP 代理
│     ├── providers: Vec<Provider>      — 云厂商端点
│     │     └── { name, url, token, proxy? }
│     ├── upstreams: Vec<UpstreamConfig> — 上游路由配置
│     │     └── { name, high?, mid?, low?, default?: TierRule, effort? }
│     │           └── TierRule { keywords, provider, model }
│     ├── retry_count: 3
│     ├── request_timeout_secs: 120
│     ├── request_retention_hours: 8
│     ├── session_max_count: 20
│     └── session_delete_after_days: 0
│
├── server: ServerConfig
│     └── { listen_address, http_port, proxy_port, mcp_proxy_port }
│
└── logging: LoggingConfig
      └── { level }
```

### 9.3 配置变更的完整事件流

```
用户操作 (前端 Settings UI)
  │
  ├─ 切换上游:   <select> change → POST /api/upstreams/:name/activate
  ├─ 切换 Effort: <select> change → PUT  /api/effort  { effort }
  ├─ 新增/编辑/删除 Upstream → POST|PUT|DELETE /api/upstreams[/:name]
  ├─ 新增/编辑/删除 Provider  → POST|PUT|DELETE /api/providers[/:name]
  ├─ 新增/编辑/删除 Pricing   → POST|PUT|DELETE /api/model-pricing[/:id]
  └─ 设置全局代理            → PUT  /api/proxy  { http_proxy }
        │
        ▼ (HTTP)
  settings.rs Handler
        │
        ├─ state.config.update(|c| { c.proxy.xxx = yyy; Ok(()) }).await
        │     │
        │     ├─ 获取 write lock on RwLock<AppConfig>
        │     ├─ 执行闭包中的变更
        │     ├─ c.validate() → 检查 upstream/provider 引用完整性、effort 值、代理 URL scheme
        │     ├─ 失败 → ConfigError::Validation → 422 返回前端
        │     └─ 成功 → persist_config() → toml_edit 写回 config.toml
        │
        └─ state.events.publish(upstream_changed(&state.config).await)
              │
              ├─ upstream_changed() 读取当前 config 快照构造:
              │   WsMessage::UpstreamChanged {
              │     active_upstream, upstreams: Vec<UpstreamInfo>,
              │     providers: Vec<ProviderInfo>, active_effort,
              │     model_pricing: Vec<ModelPricing>, http_proxy,
              │   }
              │
              ▼ (EventBus broadcast, cap=256)
         ws.rs: rx.recv() → JSON serialize → WebSocket Text
              │
              ▼
         main.js: handleMessage()
              case 'UpstreamChanged':
                applyUpstreamState(active, upstreams, providers, effort, pricing, httpProxy)
              │
              ▼
         settings.js: applyUpstreamState()
              ├─ state.activeUpstream = active
              ├─ state.upstreamList = upstreams
              ├─ state.providerList = providers
              ├─ state.modelPricingList = pricing
              ├─ state.globalProxy = httpProxy
              ├─ state.activeEffort = effort
              ├─ populateUpstreamSelect()    → Inspector 工具栏上游下拉
              ├─ populateEffortSelect()      → Effort 下拉
              ├─ renderModelMatrix()         → 模型定价表
              ├─ renderUpstreamTable()       → 上游配置表
              ├─ refreshProviderSelects()    → Provider 下拉
              └─ renderGlobalProxy()         → 全局代理输入框
```

### 9.4 关键设计点

1. **ConfigStore 是唯一真相源** — `Arc<RwLock<AppConfig>>` 在 server、relay 间共享。写操作通过 `config.update()` 原子化：获取写锁 → 修改 → 校验 → 持久化。

2. **Relay 无需"通知"** — 每次代理请求到达时，relay 从同一个 `ConfigStore` 读当前快照（read lock），因此配置变更即时生效，无热重载延迟。

3. **前端 fire-and-forget** — 前端发起 REST 变更请求后不解析 HTTP 响应体，而是信任 WebSocket `UpstreamChanged` 推送来刷新 UI。

4. **`set_effort` 不发布 UpstreamChanged** — 这是当前的一个缺口。effort 变更后前端只能靠自身的乐观更新，没有 WebSocket 广播通知其他客户端。

5. **持久化保格式** — 使用 `toml_edit` 而非 `toml` crate，保留注释和原始格式。

6. **校验在持久化之前** — `validate()` 如果失败，内存中的写锁修改会被丢弃（闭包中的变更不会提交），返回 422。

### 9.5 配置变更 API 端点

| 方法 | 路径 | 说明 | 发布 UpstreamChanged |
|---|---|---|---|
| `GET` | `/api/upstreams` | 获取完整上游/Provider/Pricing/代理快照 | — |
| `POST` | `/api/upstreams` | 新增上游 | 是 |
| `PUT` | `/api/upstreams/:name` | 编辑上游 | 是 |
| `DELETE` | `/api/upstreams/:name` | 删除上游（拒绝删除最后一个） | 是 |
| `POST` | `/api/upstreams/:name/activate` | 切换激活的上游 | 是 |
| `POST` | `/api/providers` | 新增 Provider | 是 |
| `PUT` | `/api/providers/:name` | 编辑 Provider（URL/Token/代理） | 是 |
| `DELETE` | `/api/providers/:name` | 删除 Provider | 是 |
| `POST` | `/api/model-pricing` | 新增模型定价 | 是 |
| `PUT` | `/api/model-pricing/:id` | 编辑模型定价 | 是 |
| `DELETE` | `/api/model-pricing/:id` | 删除模型定价 | 是 |
| `GET` | `/api/effort` | 获取全局 effort | — |
| `PUT` | `/api/effort` | 设置全局 effort | **否** |
| `GET` | `/api/proxy` | 获取全局代理 | — |
| `PUT` | `/api/proxy` | 设置全局代理 | 是 |

---

## 十、Upstream 实现与请求代理事件流

### 10.1 路由解析算法 (resolve_route)

```
resolve_route(config, request_model)
  │
  ├─ 1. 根据 config.proxy.active_upstream 找到当前 UpstreamConfig
  │
  ├─ 2. 按优先级匹配 TierRule: high → mid → low → default
  │     └─ TierRule::matches(model):
  │        规则有效 && keywords 中任一 keyword 是 model 的**子串**（大小写不敏感）
  │
  ├─ 3. 模型名翻译 (translate_model):
  │     ├─ 查找 ModelPricing，其 id == tier_rule.model
  │     ├─ 找到 → model_name_for_provider(provider) → 返回 Provider 侧模型名
  │     └─ 未找到 → 直接用 tier_rule.model 透传（非逻辑模型 ID）
  │
  └─ 4. 返回 ResolvedRoute { upstream, provider, configured_model, resolved_model, effort }
```

**示例**: 请求模型 `claude-sonnet-4-6` → 匹配到 upstream `default` 的 high tier rule `{keywords: ["claude-sonnet"], provider: "anthropic", model: "claude-sonnet"}` → ModelPricing id=`claude-sonnet` 的 providers 中有 `anthropic → [claude-sonnet-4-6]` → resolved_model = `claude-sonnet-4-6`

### 10.2 完整请求代理流程

```
客户端请求到达 :8888
  │
  ▼
RelayHandler::proxy_request()                        // relay.rs:161
  │
  ├─ 1. 模式检测
  │     ├─ CONNECT → 501 Not Implemented
  │     ├─ Forward Proxy (绝对 URI + scheme) → 提取完整 URL
  │     └─ Reverse Proxy (路径 /v1/messages) → 使用路径
  │
  ├─ 2. 解析请求体
  │     ├─ 提取 request_model (body.model)
  │     ├─ 提取 stream (body.stream)
  │     └─ 提取 session_id (body.metadata.user_id → JSON → session_id)
  │
  ├─ 3. 路由解析
  │     relay.config.resolve_route(&request_model).await
  │     → 失败 → 502 Bad Gateway (Unknown model)
  │
  ├─ 4. 计费解析
  │     relay.config.resolve_billing(&provider, &model).await
  │     → ModelPricing::to_price_rates() → PriceRates { input/output/cache_write/cache_read_microusd }
  │     → 未找到 → 零费率（不计费但不报错）
  │     → 生成 BillingSnapshot (冻结，历史成本不受定价变更影响)
  │
  ├─ 5. Provider 查找
  │     config.proxy.providers 中按 name 查找
  │     → 获取 url, token, per-provider proxy
  │
  ├─ 6. HTTP 代理解析
  │     ├─ per-provider proxy (空字符串 = "直连，绕过全局") > 全局 http_proxy
  │     └─ client_for_proxy() → 缓存 HashMap<String, reqwest::Client>
  │
  ├─ 7. Effort 注入 (如果 route.effort 非空且非 "auto")
  │     ├─ body.output_config = {"effort": "<value>"}
  │     └─ 追加 header: anthropic-beta: effort-2025-11-24
  │
  ├─ 8. 模型名替换
  │     └─ route.resolved_model != request_model → body.model = resolved_model
  │
  ├─ 9. URL 构建 & 请求头构建
  │     ├─ 反向代理: provider.url + 原始路径
  │     ├─ 正向代理: 原始 URL
  │     └─ build_upstream_headers():
  │         剥离 hop-by-hop (host, connection, transfer-encoding, content-length, ...)
  │         注入 Authorization: Bearer <token> 或 x-api-key: <token>
  │
  ├─ 10. dispatch_upstream() → 带重试的 HTTP 请求        // upstream.rs
  │     ├─ POST 到上游
  │     ├─ 重试策略: 指数退避 200ms * 2^attempt
  │     ├─ 仅重试连接/超时错误（不重试 HTTP 4xx/5xx）
  │     └─ 使用配置的 timeout 和 retry_count
  │
  ├─ 11. 响应解析
  │     ├─ 流式 (stream=true):
  │     │   SseParser 增量解析 SSE 字节流
  │     │   → events[], content_text, usage (tokens), stop_reason, message_id, model, ttft
  │     │
  │     └─ 非流式 (stream=false):
  │         JSON body 解析
  │         → usage, model, stop_reason, content[], error
  │
  ├─ 12. 写入 Store (SQLite)
  │     store.write(session_id, NewTask {
  │       method, path, provider, requested_model, resolved_model,
  │       request_headers, request_body, response_headers, response_body,
  │       input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens,
  │       duration_ms, ttft_ms, http_status_code, stop_reason,
  │       billing: BillingSnapshot, error_type, error_message,
  │       is_streaming, upstream,
  │     })
  │     └─ 内部: ensure_session() → insert task → update session aggregates
  │
  ├─ 13. 发布事件
  │     ├─ events.publish(WsMessage::NewRequest(ProxiedRequest { ... }))
  │     └─ events.publish(WsMessage::CostUpdated(stats))
  │
  └─ 14. 返回响应给客户端
        ├─ 流式: 重序列化 SSE (event: ...\ndata: ...\n\n)
        ├─ 非流式: 原始 JSON body
        └─ 剥离 hop-by-hop 响应头
```

### 10.3 上游调度细节 (upstream.rs)

```
dispatch_upstream(client, url, headers, body, timeout, retries)
  │
  ├─ 循环 (attempt in 0..=retries):
  │     ├─ 构建 RequestBuilder: client.post(url).headers().body()
  │     ├─ 发送请求
  │     ├─ 成功 → 返回 Response
  │     └─ 错误:
  │         ├─ 是最后一次重试 → 返回错误
  │         ├─ connect_timeout / timeout → 等待 200ms * 2^attempt → 重试
  │         └─ 其他错误 → 立即返回错误
  │
  └─ 返回 (Response, elapsed)
```

**SSE 解析** (sse.rs):
- 基于字节流增量解析，处理 `event:` / `data:` / `id:` / `retry:` 行
- 收集 `message_start` / `content_block_start` / `content_block_delta` / `message_delta` / `message_stop` / `ping` / `error`
- 累加 content text（`text_delta`）、tool use input（`input_json_delta`）
- 追踪 `ttft`（首字节时间）— 第一个非 ping 事件或第一个 content 块的时间戳
- 最终收集 `usage` (input/output/cache tokens) 和 `stop_reason`

**请求头过滤** (`build_upstream_headers`):
```
剥离的 hop-by-hop 头:
  host, connection, proxy-connection, transfer-encoding,
  content-length, accept-encoding, proxy-authorization,
  proxy-authenticate, te, trailer, upgrade

注入:
  Authorization: Bearer <token>   (Anthropic, OpenAI, Azure)
  x-api-key: <token>              (OpenRouter 等)

附加 (effort 模式):
  anthropic-beta: effort-2025-11-24
```

### 10.4 重试与错误处理

| 场景 | 重试？ | 返回客户端的 WsMessage |
|---|---|---|
| 连接超时 / DNS 解析失败 | 是 (指数退避) | `RequestUpdated { error }` |
| HTTP 4xx (认证/参数错误) | 否 | `NewRequest { http_status_code }` |
| HTTP 5xx (上游服务错误) | 否 | `NewRequest { http_status_code }` |
| SSE 流中断 (EOF before message_stop) | 否 | `NewRequest { error, partial_tokens }` |
| 响应体 JSON 解析失败 | 否 | `NewRequest { error }` |
| 超时 (request_timeout_secs) | 是 (如未耗尽重试) | `RequestUpdated { error }` |

### 10.5 路由配置示例

```toml
[proxy]
active_upstream = "production"
active_effort = "auto"

[[proxy.providers]]
name = "anthropic"
url = "https://api.anthropic.com"
token = "sk-ant-xxx"

[[proxy.providers]]
name = "openrouter"
url = "https://openrouter.ai/api"
token = "sk-or-xxx"

[[proxy.upstreams]]
name = "production"

[proxy.upstreams.high]
keywords = ["claude-opus", "claude-sonnet"]
provider = "anthropic"
model = "claude-sonnet"  # logical ModelPricing id → 翻译为 claude-sonnet-4-6

[proxy.upstreams.mid]
keywords = ["claude-haiku"]
provider = "anthropic"
model = "claude-haiku"

[proxy.upstreams.default]
keywords = []
provider = "openrouter"
model = ""  # 透传原始模型名

[[model_pricing]]
id = "claude-sonnet"
price = [3.0, 15.0, 3.75, 0.3]
providers.anthropic = ["claude-sonnet-4-6"]

[[model_pricing]]
id = "claude-haiku"
price = [0.8, 4.0, 1.0, 0.08]
providers.anthropic = ["claude-haiku-4-5"]
```

### 10.6 前端 Upstream UI 模块

`wwwroot/js/settings.js` 提供的 UI 组件:

| 组件 | 实现函数 | 触发方式 |
|---|---|---|
| Inspector 上游下拉 | `populateUpstreamSelect()` | 初始化 + `UpstreamChanged` |
| Effort 下拉 | `populateEffortSelect()` | 初始化 + `UpstreamChanged` |
| 上游配置表 | `renderUpstreamTable()` | `UpstreamChanged` |
| 上游行内编辑 | `openUpstreamTableEdit()` | 表格 Edit 按钮 |
| 模型定价矩阵 | `renderModelMatrix()` | `UpstreamChanged` |
| Provider 下拉 | `refreshProviderSelects()` | `UpstreamChanged` |
| 全局代理输入 | `renderGlobalProxy()` | `UpstreamChanged` |

### 10.7 关键文件索引（配置 & 上游）

| 文件 | 职责 |
|---|---|
| `crates/proxy-common/src/config/config.rs` | `AppConfig`, `ProxyConfig`, `ServerConfig` 结构定义 |
| `crates/proxy-common/src/config/upstream.rs` | `TierRule`, `UpstreamConfig` 结构 + `matches()` 方法 |
| `crates/proxy-common/src/config/provider.rs` | `Provider` 结构（name, url, token, proxy） |
| `crates/proxy-common/src/config/pricing.rs` | `ModelPricing`, `ResolvedRoute` 结构 + 模型名翻译 |
| `crates/proxy-common/src/config/routing.rs` | `resolve_route()` + `resolve_billing()` — 核心路由算法 |
| `crates/proxy-common/src/config/store.rs` | `ConfigStore` — 线程安全读写 + `update()` 原子变更 |
| `crates/proxy-common/src/config/validation.rs` | 配置校验规则 |
| `crates/proxy-common/src/config/persist.rs` | `toml_edit` 格式保留持久化 |
| `crates/proxy-common/src/config/migration.rs` | stale `active_upstream` 自动修复 |
| `crates/proxy-common/src/core/event.rs` | `EventBus` — broadcast channel 封装 |
| `crates/proxy-server/src/web/settings.rs` | 全部配置 CRUD Handler + `upstream_changed()` |
| `crates/proxy-relay/src/relay.rs` | `RelayHandler::proxy_request()` — 完整代理流程 |
| `crates/proxy-relay/src/upstream.rs` | `dispatch_upstream()`, `build_upstream_headers()`, `handle_streaming_response()` |
| `crates/proxy-relay/src/sse.rs` | `SseParser` — 增量 SSE 字节流解析 |
| `crates/proxy-server/src/ws.rs` | WebSocket 处理 — EventBus 订阅 + 消息转发 |
| `wwwroot/js/settings.js` | 前端 Settings UI — 上游/Provider/Pricing 管理 |
| `wwwroot/js/main.js:284` | 初始化: `GET /api/upstreams` + `applyUpstreamState()` |
