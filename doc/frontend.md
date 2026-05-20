# 前端

## 技术栈

- Vanilla JS/HTML/CSS，通过 `rust-embed` 内嵌到二进制
- CSS：深色主题，BEM 命名，CSS 变量
- JS：ES 模块拆分（9 个模块），原生 DOM + 模板字符串 + 事件委托
- i18n：`wwwroot/assets/zh.json`，`t(key)` 函数 + `data-i18n` / `data-i18n-title` / `data-i18n-placeholder` 属性绑定

## 7 个视图 Tab

| Tab | 容器 ID | 功能 |
|-----|---------|------|
| **Inspector** | `#view-inspector` | 请求表格 — Session 分组折叠、session/request 混合多选批量删除、行内详情（Request/Response/SSE 三个子 tab）。工具栏含 Upstream 选择器、Effort 选择器、Cost 统计面板、录制按钮、Flush 按钮。右侧常驻 Summary 面板（可折叠） |
| **Cost** | `#view-cost` | 成本分析 — 日期范围（今天/本周/本月预设）、摘要卡片、按天 Canvas 柱状图、按 Model/Session/Provider 分组明细 |
| **Settings** | `#view-settings` | 全宽 accordion 布局，4 个面板：Model Pricing（独立模型定价 CRUD）+ Providers CRUD + Upstreams CRUD（四层 Tier 编辑）+ Data Retention |
| **Conversation** | `#view-conversation` | 实时时间轴（API + Hook + MCP 混合），按 session 过滤，最多 100 条，支持全屏 |
| **MCP Observer** | `#view-mcp` | MCP JSON-RPC 请求列表 + 目标地址配置 |
| **Hooks** | `#view-hooks` | Hook 事件表格（Event/Session/CWD/ExitCode），最多 200 条 |
| **Archive** | `#view-archive` | Archive 视图 — 左右分栏：左侧文件列表（搜索过滤），右侧文件内容查看 + 重命名 |

## JS 模块划分

| 模块 | 职责 |
|------|------|
| `main.js` | 入口：WebSocket 连接、消息分发、视图路由、初始数据加载序列 |
| `state.js` | 全局可变状态：requestRows(Map)、selectedIds(Set)、selectedSessionIds(Set)、sessionCache、filters、pagination |
| `i18n.js` | 翻译系统：`t(key)` 函数、`data-i18n` 等属性绑定 |
| `inspector.js` | 请求表格渲染：session 分组、分页、模型/时间过滤、多选、展开/折叠、行内详情面板 |
| `session.js` | Summary 侧面板：6 区块渲染、rename/export/delete 操作、面板折叠 |
| `settings.js` | 设置视图：Model Pricing 矩阵表 + Providers CRUD + Upstreams CRUD + Retention 设置 |
| `cost.js` | 成本分析：day/week/month 模式、摘要卡片、Canvas 柱状图、三级 drill-down |
| `timeline.js` | 实时时间轴：API+Hook+MCP 混合事件流，session 过滤 |
| `archive.js` | Archive 视图：文件列表、搜索、内容查看、重命名 |
| `utils.js` | 通用工具函数 |

## 关键状态变量

```javascript
// 数据
requestRows: Map<string, ProxiedRequest>   // id → 请求对象
selectedIds: Set<string>                    // 多选 request 集合
selectedSessionIds: Set<string>             // 多选 session 集合
expandedSessions: Set<string>              // 当前展开的 session
currentSelectedSession: ?string            // Summary 面板显示的 session ID
summaryCollapsed: boolean                  // Summary 面板是否折叠
sessionCache: Object                        // session id → label
sessionMeta: Object                         // session id → 完整 Session 对象

// 配置
providerList: Array<ProviderInfo>
upstreamList: Array<UpstreamInfo>
modelPricingList: Array<ModelPricing>
activeUpstream: string
activeEffort: string                       // 默认 "auto"

// 过滤 & 分页
filterModel: string                        // 默认 "__has_model__"
filterTimeFrom / filterTimeTo: string
currentPage: number                        // 默认 1
pageSize: number                           // 默认 50
```

## 数据加载流程

```
connect() → WebSocket 首态
  ├─ HookHistory, McpHistory, McpConfigChanged, UpstreamChanged, TeeStatusChanged
  └─ 然后 REST 加载：
       1. GET /api/upstreams      → upstream select + provider/upstream/model_pricing 列表
       2. GET /api/sessions        → 预热 sessionCache + sessionMeta
       3. GET /api/requests?limit=2000 → 填充 requestRows，展开最新 session
       4. GET /api/mcp-destination  → MCP 目标地址
       5. GET /api/capture/status   → 录制开关状态
       6. GET /api/retention        → retention 设置
```

## 实时更新

WebSocket 消息驱动的前端更新：

| 消息 | 前端行为 |
|------|---------|
| `NewRequest` / `RequestUpdated` | `upsertRequestRow()` + 200ms 防抖渲染 + 500ms 防抖更新筛选下拉 + 加入时间线 |
| `SseEvent` | 若匹配当前打开的详情，实时追加 SSE 事件 |
| `NewHook` | 单行插入（最多 200 行），加入时间线 |
| `NewMcp` | 单行插入（最多 100 行），加入时间线 |
| `Cleared` | 清空全部表格和筛选 |
| `McpCleared` | 清空 MCP 表格 |
| `McpConfigChanged` | 同步 MCP 目标输入框 |
| `UpstreamChanged` | 刷新 provider/upstream/effort/model_pricing 列表和下拉 |
| `TeeStatusChanged` | 同步录制开关状态 |
| `Resync` | 重新通过 REST 加载请求数据 |
| `SessionUpdated` | 刷新 session 标签缓存 |

## Effort 乐观更新

`#effort-select` 变更时采用乐观更新策略：
1. 立即更新 `activeEffort` 和 UI
2. 发送 `PUT /api/effort` 持久化
3. 失败时回滚到修改前的值

## Session 分组与交互

### 分组渲染

`renderPage()` 按 session 分组渲染：

| 情况 | 渲染方式 |
|------|---------|
| 0 请求（archived session） | 灰色 archived badge 行，含 session checkbox |
| 1 请求 | 扁平 request 行（无 session header） |
| 多请求 | 可折叠 session header 行 + 子 request 行 |

### 交互

- 点击 expand icon → 折叠/展开子请求
- 点击 header 其他区域 → 选中 session，Summary 面板加载
- session checkbox 和 request checkbox 可同时勾选

## Summary 面板

右侧固定定位，宽 500px：
- **展开态**（默认）：6 区块内容 + 操作按钮，Inspector 右边距 516px
- **折叠态**：32px 竖向标签条，Inspector 右边距 48px

6 个内容区块：Meta row → User Prompts → Assistant Actions → Touched Files → Final Response → Stats

操作按钮：Rename / Export（JSON/YAML） / Delete

## Settings 面板

全宽 accordion 布局，4 个可折叠面板：

### Model Pricing
- 独立于 Provider 的模型定价管理
- Canonic ID + aliases（providers 映射）+ 四字段定价（in/cache-write/cache-read/out，USD/百万 token）
- 列表 + 新增/编辑/删除 CRUD，Save 后合并到 config.toml

### Providers
- 列表：名称、URL、token 状态（钥匙图标）
- 编辑：名称、URL、Token（密码字段）
- 模型归属由 ModelPricing.providers 管理

### Upstreams
- 四层 Tier 编辑：High / Mid / Low / Default
- 每层：关键词 + Provider 下拉 + Model 输入（含 datalist）
- Effort 下拉，auto 表示继承全局 effort
- 新建默认关键词：High="opus", Mid="sonnet", Low="haiku"

### Data Retention
- `request_retention_hours`（默认 8）、`session_max_count`（默认 20）、`session_delete_after_days`（默认 0）
- "Clean Up Now" + "Export All Now" 按钮 + 上次清理时间

## Inspector 工具栏 Cost 统计

实时显示当前上游的消耗统计（今日/本月 Tokens + 费用）：
- 上游过滤：从 activeUpstream 的所有 tier 提取 provider 集合
- 请求归属：通过 model 反查 providerList
- 费用计算：`input × price.input/1e6 + output × price.output/1e6 + cache_creation × price.cache_write/1e6 + cache_read × price.cache_read/1e6`

## Archive 视图

左右分栏布局：
- 左侧：文件列表 + 搜索框（`/api/archive/search?q=` 全文搜索）
- 右侧：文件内容查看器 + Rename 按钮
