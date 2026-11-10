# Proxy 代理

## 三种模式

| 模式 | 触发条件 | 行为 |
|------|---------|------|
| **CONNECT tunnel** | `method == CONNECT` | 建立 TCP 双向隧道（`tokio::io::copy_bidirectional`） |
| **Forward proxy** | URI 含 scheme（如 `https://`） | 使用 `active_proxy_upstream` 选择 provider/出站代理，目标 URL、认证 header、model 与 body 透明转发 |
| **Reverse proxy** | URI 不含 scheme（相对路径） | 从 upstream config 解析 provider base URL + 拼接路径（`ANTHROPIC_BASE_URL` 模式） |

## 自动协议转换

反向代理支持三种客户端协议，并按 provider 的 `protocols` 自动选择上游协议：

| 配置名 | 客户端入口 | 别名 |
|--------|------------|------|
| `anthropic` | `/v1/messages` | `claude`、`messages` |
| `codex` | `/v1/responses` 或 `/responses` | `openai`、`responses` |
| `chat_completions` | `/v1/chat/completions` | `chat-completions`、`completions` |

三种协议支持两两双向转换，包含普通 JSON 响应和 SSE 流式响应。入口路径优先识别 Chat Completions；Responses 还可通过请求体有 `input` 且没有 `messages` 识别。`openai` 保持表示 Responses，不能用它声明 Chat Completions。

Provider 未设置 `protocols` 或设置为空时视为支持全部协议，保持客户端协议直接转发。显式声明协议后，优先使用客户端协议；provider 不支持客户端协议时，按声明顺序选择可转换的协议。代理不探测上游实际能力，也不根据上游错误自动重试其他协议。透明正向代理保持透传。

例如只支持 Chat Completions 的 provider：

```toml
active_upstream = "woagent"

providers = [
    { name = "woagent", url = "https://ai.woagent.net", token = "sk-your-key", protocols = ["chat_completions"] },
]
accounts = []
upstreams = [
    { name = "woagent", default = { provider = "woagent", model = "KM-kimi-k2.6" } },
]
```

选择 `woagent` upstream 后，Messages / Responses 请求会转换为 `/v1/chat/completions`，响应再转回客户端协议。`url` 应填写基础地址，也允许以 `/v1` 结尾；不要填写完整接口路径。通过网页 provider 编辑器也可选择 Chat Completions。

Chat Completions 使用 `messages` 数组，`stream` 使用布尔值。转换覆盖系统指令、文本、URL / base64 图片、函数工具定义与调用结果、采样参数、token 上限、Responses 与 Chat Completions 之间的 reasoning effort 和 JSON 输出格式。Chat Completions 的 `prompt_tokens` 包含缓存，转换到 Anthropic 时会拆出 `cache_read_input_tokens`，反向转换会还原总数。文本增量实时转发；交错的多个工具调用按调用分别缓冲，完成后发出。Messages ↔ Chat Completions 会保留 `thinking` / `reasoning_content`，并将思考内容、文本和工具调用保存在同一个 assistant 回合中，供 DeepSeek 等思考模式上游在工具结果回传时继续使用。桥接产生的 thinking signature 只是本代理的往返标记，不是 Anthropic 厂商签名。

跨协议转换不支持 Responses 服务端历史（`previous_response_id` / `conversation`）、内置工具、音频、视频或 Chat Completions 的 `n > 1`；相关输入会明确报错。同协议透传不受这些转换限制。仅 Chat Completions ↔ Responses 原生对应的字段，在经 Anthropic 转换时可能不可表示；Responses 路径中的 thinking、reasoning 和专有扩展仍受原有桥接限制。流中断不会生成成功结束事件；上游忽略 `stream: true` 返回 JSON 时会补成客户端 SSE。

## CLI 验证

Claude 使用 `claude -p '分析代码改动内容'`。Codex 的非交互命令是 `codex exec '提示词'`；`-p` 表示 profile，不表示提示词。可用仓库脚本验证当前代理：

```bash
CC_PROXY_BASE_URL=http://192.168.31.10:8888/v1 \
CC_PROXY_MODEL=deepseek-v4-flash \
./scripts/test-codex-proxy.sh '分析代码改动内容'
```

代理地址应与 `[server].listen_address` / `proxy_port` 一致。`CC_PROXY_MODEL` 是客户端请求的逻辑模型名，实际上游模型仍由当前 upstream 决定。脚本默认使用本地测试密钥，真实 woagent 密钥来自代理配置；如需传入客户端密钥，可设置 `CODEX_API_KEY`。省略提示词时仅验证回复 `PROXY_CODEX_OK`。

脚本使用独立的命令行 provider 设置，不改写用户 Codex 配置；使用只读模式，不保存会话。它关闭网页搜索、Apps 和子代理，避免这些功能的 Responses 内置工具 / namespace 工具被误当成 Chat Completions function 工具；同时关闭 WebSocket，使用代理支持的 HTTP SSE。当前桥接不能代表这些扩展功能也已可用。

Chat 上游的 `reasoning_content` 会编码为 Responses `reasoning.summary` 并在后续工具回合还原，避免 DeepSeek 思考模式因缺少历史 reasoning 而返回 400。对于修复前已经丢失思考内容的会话，重新开启会话验证。

参考：[OpenAI 官方 Codex 非交互文档](https://learn.chatgpt.com/docs/non-interactive-mode)。

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
1. 完成协议转换后，按实际上游协议写入 effort：Anthropic 使用 `output_config.effort`，Responses 使用 `reasoning.effort`，Chat Completions 使用 `reasoning_effort`。
2. Anthropic 上游追加 beta header `effort-2025-11-24` 到 `anthropic-beta`。

有效值：`auto`（透传）、`low`、`medium`、`high`、`xhigh`、`max`、`ultra`、`ultracode`

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
