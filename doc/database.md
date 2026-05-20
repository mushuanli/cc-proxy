# 数据库

## 表结构

### sessions

| 列 | 类型 | 约束 |
|----|------|------|
| `id` | TEXT | PRIMARY KEY |
| `label` | TEXT | |
| `started_at` | TEXT | NOT NULL |
| `ended_at` | TEXT | |
| `status` | TEXT | NOT NULL DEFAULT 'Recording' |
| `total_input_tokens` | INTEGER | NOT NULL DEFAULT 0（聚合列，migration 追加） |
| `total_output_tokens` | INTEGER | NOT NULL DEFAULT 0（聚合列，migration 追加） |
| `request_count` | INTEGER | NOT NULL DEFAULT 0（聚合列，migration 追加） |
| `summary_json` | TEXT | 摘要 JSON（migration 追加，cleanup 时生成） |
| `total_cost` | REAL | NOT NULL DEFAULT 0.0（migration 追加，每次 insert_request 累加） |

### requests

| 列 | 类型 | 约束 |
|----|------|------|
| `id` | TEXT | PRIMARY KEY |
| `session_id` | TEXT | FK → sessions(id) ON DELETE SET NULL |
| `timestamp` | TEXT | NOT NULL |
| `method` | TEXT | NOT NULL |
| `path` | TEXT | NOT NULL |
| `model` | TEXT | |
| `status_code` | INTEGER | |
| `input_tokens` | INTEGER | |
| `output_tokens` | INTEGER | |
| `cache_creation_input_tokens` | INTEGER | |
| `cache_read_input_tokens` | INTEGER | |
| `duration_ms` | INTEGER | |
| `ttft_ms` | INTEGER | time to first token |
| `stop_reason` | TEXT | |
| `message_id` | TEXT | |
| `error` | TEXT | |
| `request_headers` | TEXT | JSON 字符串 |
| `request_body` | TEXT | |
| `content_text` | TEXT | 合并后的响应文本 |
| `is_streaming` | INTEGER | NOT NULL DEFAULT 0 |
| `provider` | TEXT | 路由决策写入的 provider 名（migration 追加） |
| `last_msg_summary` | TEXT | 最后一条消息的紧凑摘要（migration 追加，startup backfill） |

索引：`idx_requests_session`（session_id）、`idx_requests_timestamp`（timestamp）

### sse_events

| 列 | 类型 | 约束 |
|----|------|------|
| `id` | INTEGER | PRIMARY KEY AUTOINCREMENT |
| `request_id` | TEXT | NOT NULL, FK → requests(id) ON DELETE CASCADE |
| `event_type` | TEXT | |
| `data` | TEXT | |
| `seq` | INTEGER | NOT NULL |

索引：`idx_sse_request`（request_id）

### hook_events

| 列 | 类型 | 约束 |
|----|------|------|
| `id` | TEXT | PRIMARY KEY |
| `timestamp` | TEXT | NOT NULL |
| `hook_event_name` | TEXT | NOT NULL |
| `session_id` | TEXT | NOT NULL |
| `cwd` | TEXT | NOT NULL DEFAULT '' |
| `permission_mode` | TEXT | NOT NULL DEFAULT '' |
| `transcript_path` | TEXT | NOT NULL DEFAULT '' |
| `hook_input` | TEXT | NOT NULL DEFAULT 'null' |
| `environment_variables` | TEXT | NOT NULL DEFAULT '{}' |
| `exit_code` | INTEGER | NOT NULL DEFAULT 0 |
| `stdout` | TEXT | NOT NULL DEFAULT '' |
| `stderr` | TEXT | NOT NULL DEFAULT '' |

索引：`idx_hooks_timestamp`（timestamp）

### mcp_requests

| 列 | 类型 | 约束 |
|----|------|------|
| `id` | TEXT | PRIMARY KEY |
| `timestamp` | TEXT | NOT NULL |
| `method` | TEXT | NOT NULL DEFAULT '' |
| `model` | TEXT | NOT NULL DEFAULT '' |
| `status_code` | INTEGER | |
| `request_body` | TEXT | |
| `response_body` | TEXT | |

索引：`idx_mcp_timestamp`（timestamp）

### session_daily_usage

持久化的每日聚合表，在请求清理前写入，确保归档后的 session 仍可统计成本。

| 列 | 类型 | 约束 |
|----|------|------|
| `date` | TEXT | NOT NULL（YYYY-MM-DD） |
| `session_id` | TEXT | NOT NULL |
| `model` | TEXT | NOT NULL DEFAULT 'unknown' |
| `provider` | TEXT | NOT NULL DEFAULT 'unknown' |
| `input_tokens` | INTEGER | NOT NULL DEFAULT 0 |
| `output_tokens` | INTEGER | NOT NULL DEFAULT 0 |
| `cache_creation_tokens` | INTEGER | NOT NULL DEFAULT 0 |
| `cache_read_tokens` | INTEGER | NOT NULL DEFAULT 0 |
| `request_count` | INTEGER | NOT NULL DEFAULT 0 |

主键：`(date, session_id, model)`
索引：`idx_daily_usage_date`（date）

写入时机：
- `insert_request()` — 每次请求完成时 upsert（ON CONFLICT DO UPDATE 累加）
- `cleanup_old_requests()` — 清理前批量 INSERT OR REPLACE 确保完整

## PRAGMA 设置

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
```

`busy_timeout` = 5s，防止 WAL checkpoint 或外部访问时的 SQLITE_BUSY。

## 数据清理

### 后台任务（`cleanup_loop`）

启动时执行一次，之后每 30 分钟运行。

### 清理流程（`cleanup_old_requests`）

6 步流程：

1. **聚合统计** — 将要删除的 session 的 token 聚合回写到 `sessions` 表
2. **生成摘要** — 对将失去请求的 session，取最新请求做 `analyze_request()`，写入 `summary_json`
3. **持久化每日用量** — INSERT OR REPLACE 批量写入 `session_daily_usage`
4. **标记 Archived** — 将相关 session 状态更新为 `Archived`，`ended_at` 设为最后请求时间
5. **保留墓碑** — 每个被清理的 session 保留最新一条请求（tombstone），其余删除
6. SSE events 通过 FK CASCADE 自动级联删除

### 保留策略

- `request_retention_hours` 小时后删除旧请求，但保留最新 session（`keep_session_id`）的请求
- 默认 8 小时，0 = 不清理

### Session 限制

- 超过 `session_max_count` 时删除最旧的 sessions
- 默认 20，0 = 不限制

### Session 年龄清理

- `session_delete_after_days` > 0 时，删除超过指定天数的 session 记录（仅删 SQLite row，`sessions/` 目录文件保留）
- 默认 0 = 不清理

### 手动触发

`POST /api/cleanup`

## 聚合查询

### `get_cost_data(from, to) -> CostData`

按时间范围聚合成本数据，返回四个维度：
- `by_model: Vec<ModelCost>` — 按模型分组
- `by_provider: Vec<ProviderCost>` — 按 provider 分组
- `by_session: Vec<SessionCost>` — 按 session 分组（含 label、模型列表、时间范围）
- `by_day: Vec<DailyCost>` — 按天 + 模型分组（UNION ALL 合并活跃请求 + `session_daily_usage` 归档数据，通过 NOT IN 子查询去重）

### `list_session_ids_with_requests() -> Vec<String>`

返回所有有请求的 session ID 列表，按最新请求时间降序。供 flush-all 遍历 session 组使用。

### `backfill_last_msg_summary()`

启动时调用，为 `last_msg_summary` 为 NULL 的历史请求回填摘要（最多 2000 条）。

### `sessions_with_old_requests(hours, keep_session_id) -> Vec<String>`

返回有超时请求的 session ID 列表，用于 cleanup 前的 pre-flush。
