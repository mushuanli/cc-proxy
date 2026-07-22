# Proxy 代理

## 三种模式

| 模式 | 触发条件 | 行为 |
|------|---------|------|
| **CONNECT tunnel** | `method == CONNECT` | 建立 TCP 双向隧道（`tokio::io::copy_bidirectional`） |
| **Forward proxy** | URI 含 scheme（如 `https://`） | 使用 `active_proxy_upstream` 选择 provider/出站代理，目标 URL、认证 header、model 与 body 透明转发 |
| **Reverse proxy** | URI 不含 scheme（相对路径） | 从 upstream config 解析 provider base URL + 拼接路径（`ANTHROPIC_BASE_URL` 模式） |

## proxy_handler 流程

```
1. 提取 session_id（metadata.user_id.session_id 嵌套 JSON）
2. ensure_session(sid) → DB
3. 提取 model → UpstreamConfig.resolve() → (provider, model_field)
4. ModelPricing.model_name_for_provider() → 翻译为 Provider 端模型名
5. Effort 注入（如果 active_effort != "auto"）
6. dispatch_upstream() → 执行请求
7. compute_cost() → 计费
8. insert_request() → DB + broadcast
9. tee_writer.write() → 录制
```

## dispatch_upstream()

正向代理和反向代理共用 `RelayHandler`、`dispatch_upstream()`、Session/Task 存储与事件发布。两种入口可同时使用并独立选择 upstream：

- relay（相对 URI）使用 `active_upstream`，执行 token、model 与 effort 改写。
- proxy（absolute URI）使用 `active_proxy_upstream`，只用 tier 决定 provider 及其网络 `proxy`，请求和响应报文透明转发。

Provider 的 `proxy` 优先于全局 `http_proxy`；`proxy = ""` 表示显式直连。

透明 upstream 只要求 tier/default 中存在 provider，model 可以留空。转发与 Inspector 始终保留请求线上原始 model；ModelPricing 只参与费率匹配，不参与透明报文改写。没有匹配费率时请求仍正常转发和存储，Task 标记为 `priced=false`，Inspector 显示“未定价”而不是零费用。

实际上游请求：

- **重试**：指数退避 200ms × 2^n，只对 connect/timeout 错误重试，默认最多 3 次
- **超时**：默认 120s，超时返回 JSON-RPC-style error with 504
- **流式响应**：SseParser 实时解析 SSE 事件 → 100ms 批量广播 `SseEvent` → 完成后合并 delta 文本、计算 session token 总计、compute_cost、写入 DB、广播 `RequestUpdated`
- **非流式响应**：缓冲完整响应体，提取 usage 统计（input/output/cache tokens），广播 `NewRequest`
- 两种路径都会写入 tee 文件（如录制开启）

## Effort 注入

当 active upstream 的 effort != "auto" 时：
1. 将 `output_config.effort` 合并到请求 body JSON 中
2. 追加 beta header `effort-2025-11-24` 到 `anthropic-beta`

有效值：`auto`（透传）、`low`、`medium`、`high`、`xhigh`、`max`、`ultracode`

## SSE 解析

`SseParser` 解析 Anthropic SSE 字节流（`\n\n` / `\r\n\r\n` 分隔），实时提取：
- `event_kind` — 事件类型
- `delta_text` — 文本增量
- `usage_from_delta` — token 用量
- `stop_reason` / `message_id` / `model_from_start` / `input_tokens_from_start`
- `cache_creation_tokens_from_start` / `cache_read_tokens_from_start`

同时识别 Codex/OpenAI Responses 报文：`/v1/responses`、`input`、`prompt_cache_key`，以及 `response.output_text.delta` / `response.completed` 事件。Codex Session/Task 使用 `ClientType::Codex` 持久化，原始 response 和 SSE events 写入 task metadata，Inspector 的 Response 与 SSE Events 页签可直接查看。

`merge_delta_text()` 处理三种 delta 类型：
- `text_delta` → 纯文本
- `thinking_delta` → `[Thinking]` 标记
- `input_json_delta` → `[Tool Use]` 标记

## Headers 处理

- `x-api-key` / `authorization` → 脱敏为 `[REDACTED]`（存储用），上游请求时替换为 Provider token
- `transfer-encoding` / `content-encoding` / `content-length` → 丢弃
- Provider token 认证：`sk-` 开头 → `Authorization: Bearer <token>`，否则 → `x-api-key: <token>`
- `accept-encoding` → 强制设为 `identity`（禁用压缩以正确解析）

## 模型翻译

`translate_model(model_pricing, provider, model_field) -> String`：

1. `model_field` 匹配某个 `ModelPricing.id`（逻辑 ID）→ 查找 `providers[provider]` 的第一个名字
2. `providers[provider]` 为空 vec → 返回逻辑 ID 本身
3. `providers` 中没有该 provider → 无映射，继续下一步
4. `model_field` 不匹配任何逻辑 ID → 作为原始 provider 模型名透传

## 计费

`compute_cost(tokens, ModelPricing) -> f64`：

```
cost = input_tokens × price.input/1e6
     + output_tokens × price.output/1e6
     + cache_creation × price.cache_write/1e6
     + cache_read × price.cache_read/1e6
```

- 计费在每次请求完成时执行
- cost 写入 `ProxiedRequest.cost`（用于 `insert_request` 中累加到 `sessions.total_cost`）
- `session_daily_usage` upsert 的是 token 计数（非 cost），cost 由前端使用当前定价实时计算
