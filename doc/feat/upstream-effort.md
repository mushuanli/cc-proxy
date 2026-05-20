# Upstream Effort Level + Cost 计费 — 实现文档

## 背景

Claude Code 的 `/effort` 命令控制模型的推理深度：

| 级别 | 用途 | 跨会话持久 |
|------|------|----------|
| `low` | 短、延迟敏感、不依赖智能的任务 | 是 |
| `medium` | 成本敏感，可牺牲部分智能 | 是 |
| `high` | 大多数编码任务的平衡默认值（Opus 4.6/Sonnet 4.6 默认） | 是 |
| `xhigh` | 更深推理，更高 token 消耗 | 是 |
| `max` | 最深推理，无 token 上限（仅 Opus 4.6，易过度思考） | 否（session-only） |
| `ultracode` | xhigh + 自动 Dynamic Workflow 编排 | 否（session-only） |

## 实现机制（API 层面）

Claude Code 当前使用 Era 3 — Adaptive Thinking：

```json
{
    "model": "claude-opus-4-6",
    "thinking": { "type": "adaptive" },
    "output_config": { "effort": "max" },
    ...
}
```

需要 beta header：`anthropic-beta: effort-2025-11-24`

---

## Effort 控制

### 设计决策

**放在 Inspector 工具栏，而非 Settings → Upstreams**

理由：
- Effort 是**全局级别**的设置，与模型 Tier 路由正交
- Inspector 是监控和控制当前代理行为的入口（已有 upstream 选择器）
- 遵循 YAGNI — 不把 effort 绑定到特定 upstream/tier

**`"auto"` = 透传，不修改**

当 effort 为 `auto`（默认）时，代理不修改 body，原样传递给上游。

### 架构

```
Inspector 工具栏: [Upstream ▼] | [Effort: ▼]
    ↓ change
PUT /api/effort {"effort": "xhigh"}
    ↓
Server: *active_effort.write() → persist_config() → broadcast UpstreamChanged
    ↓
proxy handler: inject_effort_into_body() — 字段级 merge，非整体替换
    ↓
Request body: output_config.effort = "xhigh"（其他 output_config 字段保留）
Header: anthropic-beta += ",effort-2025-11-24"
```

### 关键实现细节

**`inject_effort_into_body`**（`proxy.rs`）：

```rust
// 字段级 merge，保留 output_config 中已有的其他字段
if let Some(oc) = json.get_mut("output_config").and_then(|v| v.as_object_mut()) {
    oc.insert("effort".into(), Value::String(effort.to_string()));
} else {
    json["output_config"] = json!({"effort": effort});
}
```

> **注意**：使用字段级 merge 而非整体替换 `output_config`，避免清除 `max_tokens` 等已有字段。

**Beta header 追加**（不替换已有值）：
```rust
if let Some(existing) = headers.get("anthropic-beta")... {
    if !existing.contains(BETA) {
        headers.insert("anthropic-beta", format!("{},{}", existing, BETA));
    }
} else {
    headers.insert("anthropic-beta", "effort-2025-11-24");
}
```

### 配置持久化

`active_effort` 写入 `config.toml` 的 `[proxy]` 段。旧配置无此字段时 serde default 为 `"auto"`。

### API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/effort` | 返回 `{"effort": "xhigh"}` |
| PUT | `/api/effort` | 设置 effort，广播 UpstreamChanged |

有效值：`["auto", "low", "medium", "high", "xhigh", "max", "ultracode"]`

无效值返回 400：`{"error": "Invalid effort 'xxx'. Valid: auto, low, ..."}`

### WsMessage 扩展

`UpstreamChanged` 新增 `active_effort: String` 字段，与 `active_upstream` 同步广播。

`GET /api/upstreams` 响应中也包含 `active_effort`（前端初始化用）。

### 边界情况

| 场景 | 行为 |
|------|------|
| 旧 config.toml 无 `active_effort` | serde default → `"auto"` |
| effort = `"auto"` | 不修改 body，不添加 beta header |
| body 中无 `output_config` | 注入 `{"output_config": {"effort": "x"}}` |
| body 已有 `output_config.effort` | 被代理设置覆盖，其他字段保留 |
| beta header 已含其他值 | 追加 `,effort-2025-11-24`，不替换 |
| 无效 effort 值 | API 返回 400，不修改状态 |
| CONNECT tunnel 模式 | 不经过 body 注入路径（CONNECT 建立 TCP 隧道，body 不解析） |
| 多 WS 客户端 | UpstreamChanged 广播到所有客户端，UI 保持同步 |

---

## Cost 计费视图

### 设计概览

独立 Cost tab（非 Inspector 内嵌徽章），按需加载，支持时间范围查询：

```
┌─ Cost Tab ───────────────────────────────────────────────┐
│ [Today] [This Week] [This Month] | From: [date] To: [date] [Refresh] │
│                                                           │
│ ┌─ Total Cost ─┐ ┌─ Input Tokens ─┐ ┌─ Output ─┐ ┌─ Reqs ─┐ │
│ │   ¥12.5000   │ │     1.2M       │ │  350K    │ │  128   │ │
│ └──────────────┘ └────────────────┘ └──────────┘ └────────┘ │
│                                                           │
│ By Session          By Model            By Provider       │
│ ┌──────────────┐    ┌────────────────┐  ┌──────────────┐  │
│ │ session / cost│    │ model / cost   │  │ provider/cost│  │
│ └──────────────┘    └────────────────┘  └──────────────┘  │
└──────────────────────────────────────────────────────────┘
```

### API 端点

`GET /api/costs?from=<date>&to=<date>`

参数为 `YYYY-MM-DD` 格式，默认为今日。

**响应结构**：

```json
{
  "from": "2026-06-07",
  "to": "2026-06-08",
  "by_model": [
    {
      "model": "claude-opus-4-6",
      "input_tokens": 300000,
      "output_tokens": 120000,
      "cache_creation_tokens": 5000,
      "request_count": 42
    }
  ],
  "by_session": [
    {
      "session_id": "abc123",
      "session_label": "my-repo",
      "input_tokens": 80000,
      "output_tokens": 32000,
      "request_count": 15,
      "first_request": "2026-06-07T09:00:00Z",
      "last_request": "2026-06-07T14:30:00Z",
      "models": ["claude-opus-4-6", "claude-sonnet-4-6"]
    }
  ]
}
```

**实现要点**：
- 使用 `input_tokens` / `output_tokens`（单次请求），而非 `total_*`（累计值），避免重复计算
- `by_model`：`GROUP BY model`，聚合 SUM + COUNT，含 `cache_creation_input_tokens`
- `by_session`：`GROUP BY session_id`，用 `GROUP_CONCAT(DISTINCT model)` 获取模型列表
- 仅统计 `status_code IS NOT NULL` 的已完成请求
- 两次 SQL 查询分别 lock/unlock，避免长时间持有 Mutex

### 前端成本计算

复用已有的 `lookupProviderPrice(model)` 函数（从 `providerList` 中按 model id 查找定价）：

```javascript
function calcModelCosts(byModel) {
    return byModel.map(m => {
        const price = lookupProviderPrice(m.model);
        const cost = m.input_tokens * (price?.in ?? 5) / 1e6
                   + m.output_tokens * (price?.out ?? 25) / 1e6;
        return { ...m, cost };
    });
}
```

按 Provider 汇总通过 `findProviderForModel(model)` 实现（遍历 providerList）。

### 日期范围预设

```javascript
const COST_PRESETS = {
    today: () => { /* 今日 0:00 ~ 明日 0:00 */ },
    week:  () => { /* 本周日 ~ 下周日 */ },
    month: () => { /* 本月 1 日 ~ 下月 1 日 */ },
};
```

Cost tab 首次激活时自动加载 Today 数据。

### 边界情况

| 场景 | 行为 |
|------|------|
| 日期范围内无数据 | 卡片全显示 0，表格显示 "No data" |
| model 为 NULL 的请求 | `by_model` 中显示为 `"unknown"` |
| session 无 label | 使用 session_id 前 8 位 |
| provider 无定价 | 使用默认值 input=5/output=25（¥/百万 token） |
| `cache_creation_tokens` | 纳入 `by_model` 聚合，前端目前按基础 input 价格计算 |

---

## 关键文件

| 文件 | 改动 |
|------|------|
| `crates/proxy-core/src/config.rs` | `ProxyConfig.active_effort` 字段 + serde default + `AppConfig::default()` |
| `crates/proxy-core/src/models.rs` | `UpstreamChanged.active_effort` + `ModelCost/SessionCost/CostData` 结构体 |
| `crates/proxy-core/src/db.rs` | `get_cost_data()` 聚合查询方法 |
| `crates/proxy-server/src/main.rs` | `AppState.active_effort` + persist + WS 广播 + 启动日志 |
| `crates/proxy-server/src/api.rs` | `GET/PUT /api/effort` + `GET /api/costs` + `list_upstreams` 扩展 |
| `crates/proxy-server/src/proxy.rs` | `inject_effort_into_body()` + forward/reverse proxy handler 调用 |
| `wwwroot/index.html` | Effort select + Cost nav tab + Cost 视图骨架 |
| `wwwroot/js/app.js` | Effort 状态管理 + Cost 视图逻辑（loadCosts/renderCostView 等） |
| `wwwroot/css/style.css` | Cost 视图样式 + effort select 样式 |
