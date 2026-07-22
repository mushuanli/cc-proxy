# API 端点 & WebSocket

## REST API

全部端点在 dashboard 端口（默认 :5000）。

### 请求

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/requests?session_id=&q=&from=&to=&limit=` | 请求列表（不含 body，用 messages_count + last_msg_summary 替代） |
| DELETE | `/api/requests` | 批量删除 `{ids: []}` |
| GET | `/api/request/:id` | 单条请求详情（含 body） |
| DELETE | `/api/request/:id` | 单条删除 |
| GET | `/api/request/:id/summary` | 请求摘要分析 |

### Session

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/sessions?q=` | 会话列表（按 started_at DESC） |
| GET | `/api/session/:id` | 会话详情（含嵌套 requests） |
| PUT | `/api/session/:id` | 重命名 `{label}` → broadcast SessionUpdated |
| DELETE | `/api/session/:id` | 删除会话（FK cascade 清空关联请求的 session_id） |
| GET | `/api/session/:id/export?format=json\|har\|markdown\|yaml` | 导出（含 content-disposition header） |
| GET | `/api/session/:id/summary` | 会话摘要（解析 messages[]，提取 prompts、actions、files） |

### 配置 — Model Pricing

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/model-pricing` | 列表 |
| POST | `/api/model-pricing` | 新增 → persist_config |
| PUT | `/api/model-pricing/:id` | 更新 → persist_config |
| DELETE | `/api/model-pricing/:id` | 删除 → persist_config |

### 配置 — Providers

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/providers` | 列表（token 脱敏为 has_token: bool） |
| POST | `/api/providers` | 新增 → persist_config |
| PUT | `/api/providers/:name` | 更新 → persist_config |
| DELETE | `/api/providers/:name` | 删除 → persist_config |

### 配置 — Upstreams

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/upstreams` | 列表（含 active_upstream + providers + model_pricing） |
| POST | `/api/upstreams/:name/activate-proxy` | 将 upstream 设为透明 proxy 当前 upstream（与 relay 独立） |
| POST | `/api/upstreams` | 新增 → persist_config |
| PUT | `/api/upstreams/:name` | 更新 → persist_config |
| DELETE | `/api/upstreams/:name` | 删除（最后一条不可删） → persist_config |
| POST | `/api/upstreams/:name/activate` | 切换 active upstream + 应用 upstream 级 effort → broadcast UpstreamChanged |

### Effort

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/effort` | 当前 effort 级别 |
| PUT | `/api/effort` | 设置 effort `{level}` → persist_config |

### 成本

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/costs?from=&to=` | 成本聚合（by_model + by_provider + by_session + by_day），默认当天 |

### Hook

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/hook-event` | 接收 Hook 事件（由 proxy-hook-agent 调用） |
| PUT | `/api/hook-event` | 批量更新 Hook（按 body 中的 id） |
| PUT | `/api/hook-event/:id` | 更新单个 Hook |
| POST | `/api/clear-hooks` | 清空所有 hooks |

### MCP

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/mcp-destination` | 获取 MCP 目标 URL |
| PUT | `/api/mcp-destination` | 设置 MCP 目标 `{url}` → broadcast McpConfigChanged |
| POST | `/api/clear-mcp` | 清空 MCP 请求 |

### 录制 & 清理

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/capture` | 开关录制 `{enabled}` → broadcast TeeStatusChanged |
| GET | `/api/capture/status` | 录制状态 `{enabled}` |
| GET | `/api/retention` | 获取保留设置 |
| PUT | `/api/retention` | 更新保留设置 → persist_config |
| POST | `/api/cleanup` | 手动触发清理 |

### Flush & 清空

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/flush` | 导出指定 session(s) 到 `sessions/` 目录（YAML） `{session_ids: []}` |
| POST | `/api/flush-all` | 导出所有有请求的 session 到 `sessions/` 目录 |
| POST | `/api/clear` | 清空所有 requests + hooks + sse_events → broadcast Cleared |

### Archive

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/archive/list` | 列出 `sessions/` 目录下的 YAML 文件 |
| GET | `/api/archive/search?q=` | 全文搜索 archive 文件（按 role 过滤 + 多关键词 AND） |
| GET | `/api/archive/file/:name` | 读取单个 archive 文件内容 |
| PUT | `/api/archive/name/:sid` | 重命名 archive 文件 |

### 其他

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/ws` | WebSocket 升级 |
| GET | `/api/health` | 健康检查（requests/hooks/mcp 数量） |
| GET | `/` 及其他 | 回退到 `rust-embed` 静态文件服务（wwwroot/） |

## WebSocket 消息

路径：`/ws`，Tagged union JSON（`{type, payload}`）

### 消息类型

| 类型 | Payload | 方向 | 说明 |
|------|---------|------|------|
| `NewRequest` | `ProxiedRequest` | S→C | 新请求（非流式完成时） |
| `RequestUpdated` | `ProxiedRequest` | S→C | 请求更新（流式完成时） |
| `SseEvent` | `{request_id, event: SseEvent}` | S→C | 流式事件片段 |
| `NewHook` | `HookEvent` | S→C | 新 Hook 事件 |
| `NewMcp` | `ProxiedRequest` | S→C | 新 MCP 请求 |
| `Cleared` | (unit) | S→C | 请求已清空 |
| `McpCleared` | (unit) | S→C | MCP 已清空 |
| `McpConfigChanged` | `{destination_url: Option<String>}` | S→C | MCP 目标变更 |
| `UpstreamChanged` | `{active_upstream, upstreams[], providers[], active_effort, model_pricing[]}` | S→C | 上游配置变更 |
| `History` | `{requests[]}` | S→C | 请求历史（仅 WS 首态） |
| `HookHistory` | `{events[]}` | S→C | Hook 历史（WS 首态） |
| `McpHistory` | `{requests[]}` | S→C | MCP 历史（WS 首态） |
| `SessionStarted` | `Session` | S→C | 新 session 开始 |
| `SessionStopped` | `Session` | S→C | Session 结束 |
| `SessionUpdated` | `{request_id}` | S→C | Session 重命名通知 |
| `TeeStatusChanged` | `{enabled: bool}` | S→C | 录制开关状态变更 |
| `Resync` | (unit) | S→C | 广播缓冲区溢出，客户端应通过 REST 重新同步 |

### 连接握手

WS 连接建立后，服务端立即推送 6 条初始状态：

1. `HookHistory` — 全部 hook 事件
2. `McpHistory` — 全部 MCP 请求
3. `McpConfigChanged` — 当前 MCP 目标
4. `UpstreamChanged` — upstreams + providers + active_effort + model_pricing
5. `TeeStatusChanged` — 录制开/关

**注意**：请求历史（`History`）不在 WS 首态中推送，由前端通过 `GET /api/requests` REST 加载。

### 生命周期

- Ping/Pong：每 10s 发送 Ping，300s 无 Pong 则断开（在 200s/220s/240s 发出分级警告）
- 重连：前端指数退避 1s → 2s → 4s → ... → max 30s
- 静默检测：前端每 5s 检查，超过 180s 无消息则显示 "Connected (silent Ns)"
