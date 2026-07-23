# CC Proxy 全量代码审查

审查日期：2026-07-23  
审查范围：Cargo workspace 下 5 个 crate、`wwwroot/` 前端、Shell 脚本、配置与设计文档。  
审查基线：当前工作区存在用户未提交改动，本报告按当前文件内容审查，未修改既有代码。

## 结论摘要

当前版本可以编译，45 个 Rust 单元测试全部通过，JavaScript 与 Shell 脚本语法检查通过；但仍存在会造成任意位置归档文件写入、敏感会话数据泄露、伪流式响应、配置状态损坏和统计数据不一致的高优先级问题。README/设计文档声明的 Capture、Hook、删除、单次 Flush、Cleanup、YAML 导出、CONNECT 等能力中，也有多项尚未接通或只是返回成功的空实现。

建议优先处理顺序：

1. 限制并统一校验 `SessionId`，修复归档路径逃逸和前端属性注入。
2. 为 Dashboard/WebSocket 增加认证及 WebSocket `Origin` 校验。
3. 重做 SSE 转发，使响应边读边发，并对采集数据设置上限。
4. 将 Session 建成独立于 Task 存活周期的权威累计状态，并打通 Summary → Archive 链路。
5. 让配置更新和数据库写入具备真正的原子性。
6. 接通 `active_effort`、全局代理及对外暴露但未实现的 API。

## 按目标数据模型复核

目标应明确为：

```text
每个完整 Task
    ├── 原子累加到 SessionState（历史权威状态，不依赖 Task 是否还存在）
    ├── 生成/更新最后 TaskSummary
    └── Archive = SessionState 快照 + 最后 TaskSummary
```

### Session 累加状态

当前 `sessions` 表已经正确设计并累加了以下字段：

- `task_count`、`completed_task_count`、`failed_task_count`
- input/output/cache-write/cache-read tokens
- `total_cost_microusd`
- `last_activity_at`、archive checkpoint、`archive_dirty`

样本数据库验证结果：38 个 Session、1,638 个 Task，当前 `sessions` 与 Task/Daily Usage 的请求数和费用没有发现不一致。但这只能说明现有样本尚未触发故障，不代表写入具备事务安全。

当前 Session 仍不是完整的权威状态：

- 没有 session status、`ended_at`、最后 task status/error/stop reason。
- 没有最新 provider/model/upstream、priced/unpriced 计数和累计 duration/TTFT。
- `cwd`、`project_key` 等字段虽然存在，relay 创建 Session 时没有提供。
- Session Summary API 不读取 Session 累计值，而是分析最后 Task；失败 fallback 也只汇总当前仍存活的 Task。旧 Task cleanup 后会少算历史 token。
- `created_at`/`first_activity_at` 使用 Task 写库时刻，而不是请求真正的 `started_at`。
- `proxy-common::Session` 中存在 `Recording/Stopped/Archived` 状态机，但 `proxy-store::Session` 和数据库没有对应状态，属于两套脱节模型。

因此，Session 应扩展为独立于 Task 保留策略的 `SessionState`。每个 Task 完成时用同一 SQLite transaction 累加所有可恢复状态；删除 Task 只能减少明细，不能改变 Session 历史累计。

### Summary 有用性评估

结论：现有分析器能提供“对话浏览草稿”，但不足以作为可靠的 Task/Session Summary，更不足以直接归档。

有价值的部分：

- 能从 Anthropic `messages` 提取真实 user prompt。
- 能概括常见工具调用及按文件统计 Read/Write/Edit。
- 能统计工具调用、tool result、thinking block 数量。

主要缺口：

- 只支持 Anthropic `messages`，Codex `input` 直接分析失败。
- 分析器输入是一个人为构造的最小 `ProxiedRequest`，只填了 request body 和 requested model；session ID、时间、status code、stop reason、四类 token 均为空或 0。
- 当前 Task 的真实 response 没有传入分析器，所以 `final_response` 会退回请求历史中的“上一个 assistant response”，不是刚完成 Task 的最终回复。
- 文件统计代表“调用过工具”，没有关联 tool result，失败的 Edit/Write 仍会被计为成功修改。
- 没有 provider/upstream/resolved model、费用、duration/TTFT、error、是否 priced 等排障和成本分析信息。
- 没有目标、最终结论、未完成事项、错误/重试、关键决策等更高层语义。
- Summary 没有持久化：样本库 1,638 个 Task 中 `summary_json IS NOT NULL` 的数量为 0。

建议把 Summary 分成两个稳定 schema：

- `TaskSummaryV1`：本次 user request、assistant result、tool actions/results、touched files、status/error、model/provider、usage/cost/timing。
- `SessionSummaryV1`：从最后完整会话上下文提炼目标、阶段、关键决策、累计文件变化、未完成事项，并引用 SessionState 累计指标。

分析器应直接接收 `Task + SessionState`，不能通过缺字段的兼容 DTO 间接构造。

### Archive 内容评估

当前 Archive 已保存部分 Session 信息、累计 statistics、daily usage 和最后一个非 recording Task，方向与目标接近；但“最后 Task Summary”实际上没有可靠保存：

- `ArchiveTask.summary` 读取 `task.summary_json`，而生产路径从未生成该字段。
- cache 模块使用 `proxy_common::TaskSummary`（`user_request/assistant_result/touched_files: Vec<String>`）。
- 在线 analyzer 产出的是另一种 `SessionSummary`（`user_prompts/assistant_actions/final_response/touched_files: Vec<FileTouched>`）。
- Archive formatter 又按 `TaskSummary` 字段名手工读取 JSON。即便未来直接缓存 analyzer 输出，归档中的 user request、assistant result 和 touched files 仍会为空。

建议 Archive schema 直接表达目标，而不是嵌套整个最后 Task：

```yaml
version: 2
session:             # 完整 SessionState 快照
  id:
  status:
  started_at:
  ended_at:
  task_counts:
  usage:
  cost:
  latest_route:
  error_counts:
  archive_checkpoint:
last_task:
  id:
  sequence_no:
  status:
  started_at:
  ended_at:
summary:             # 稳定的 TaskSummaryV1
  user_request:
  assistant_result:
  actions:
  touched_files:
  unresolved_items:
```

归档流程应先确保最后 Task 的 `TaskSummaryV1` 已生成并持久化，再在同一逻辑操作中读取 SessionState + Summary 生成 YAML；只有原子写文件成功后才推进 checkpoint/cleanup。

## 问题清单

### [P0] 1. 客户端可控的 Session ID 能逃逸归档目录并覆盖 `.yaml` 文件

位置：

- `crates/proxy-relay/src/upstream.rs:105-140`
- `crates/proxy-common/src/models.rs:8-14`
- `crates/proxy-store/src/archive/manager.rs:57-61`
- `crates/proxy-store/src/archive/manager.rs:344-347`
- `crates/proxy-store/src/archive/file.rs:38-43`

请求头、Codex 字段或 Anthropic `metadata.user_id` 中的 session ID 被直接包装为 `SessionId`，没有字符集或长度校验。归档时又直接执行：

```rust
self.archive_dir.join(format!("{}.yaml", session_id.as_str()))
```

仓库其实已有 `is_safe_filename()`，但写归档前从未调用。攻击者可以使用类似 `../outside` 的 session ID，使 Flush 将内容写到 `data/outside.yaml`；使用更多 `../` 可以继续逃逸。服务进程权限范围内原有同名 YAML 也会被覆盖。

建议：

- 将 `SessionId::new` 改为可失败构造，统一限定长度及 `[A-Za-z0-9_-]`。
- 在所有文件系统边界再次调用 `is_safe_filename`，不要只依赖入口校验。
- `canonicalize` 父目录并确认目标仍位于 `archive_dir` 下。
- 增加恶意 header、Anthropic metadata、Codex body 和归档写入的集成测试。

### [P0] 2. WebSocket 无认证且不校验 Origin，恶意网页可窃取实时代理内容

位置：

- `crates/proxy-server/src/web/mod.rs:33-35`
- `crates/proxy-server/src/ws.rs:17-22`
- `crates/proxy-relay/src/relay.rs:633-673`

`/ws` 对任何连接直接升级，没有 token、cookie、来源校验或子协议校验。浏览器 WebSocket 不受普通 Fetch CORS 读取限制；用户只要打开恶意网页，该网页就可以连接 `ws://127.0.0.1:5000/ws`，持续接收 `NewRequest` 事件。事件包含 prompt、代码上下文、响应正文、session ID 和调用统计。

默认绑定 loopback 只能减少远程扫描风险，不能防止 Cross-Site WebSocket Hijacking；如果用户把 `listen_address` 改为非 loopback，所有 REST 配置/删除接口也会无认证暴露。

建议：

- Dashboard 和 WebSocket 使用同一个随机 bearer token 或安全 session。
- 握手时校验 `Origin` 白名单；非浏览器客户端可明确配置放行策略。
- 非 loopback 绑定时强制启用认证，或启动时拒绝不安全配置。
- WebSocket 事件按最小必要数据发送，敏感 body 可提供显式开关。

### [P1] 3. “流式”请求会等上游完整结束后才向客户端返回

位置：

- `crates/proxy-relay/src/relay.rs:496-501`
- `crates/proxy-relay/src/relay.rs:680-713`
- `crates/proxy-relay/src/upstream.rs:225-230`
- `crates/proxy-relay/src/upstream.rs:289-364`

`handle_streaming_response()` 完整消费 `response.bytes_stream()`，把所有 chunk、SSE event 和文本保存在内存；函数返回后，relay 才用 `Body::from(raw_body)` 构建客户端响应。因此客户端看不到逐 token 输出，TTFT 虽被记录，却不是真正的客户端 TTFT。

这还会把同一份流同时保存在 `raw_body`、`sse_events`、normalized text 和数据库 metadata 中。长会话会造成高内存峰值和数据库快速膨胀，且当前没有响应体或 SSE event 数量上限。

建议：

- 用 channel/stream body 将上游 chunk 立即转发给客户端，同时在旁路增量解析与计量。
- 客户端断开时取消上游请求。
- 对 raw capture、单事件、事件数和总采集字节设置上限，超限后保留截断标志。
- 增加可观测的首 chunk 集成测试，而不只测试 SSE parser。

### [P1] 4. 配置更新失败会留下已修改的无效内存配置

位置：

- `crates/proxy-common/src/config/store.rs:45-65`
- `crates/proxy-common/src/config/persist.rs:41-43`

`ConfigStore::update` 直接在写锁保护的正式配置上执行 updater，然后验证。验证失败时函数返回错误，但没有回滚 updater 已经做出的修改。磁盘写入失败时也一样：内存已经改变，磁盘仍是旧值。

此外，函数在持久化前释放写锁，再重新读取快照。两个并发 update 可能交错写文件，使某次调用返回/持久化的并不是它自己的修改；`tokio::fs::write` 也不是原子替换，进程崩溃可能留下截断的 TOML。

建议：

- 在锁内 clone 当前配置，在 clone 上更新和验证。
- 使用临时文件、`sync_all`、原子 rename 持久化成功后再一次性替换内存状态。
- 用单独的 update/persist mutex 串行化完整事务。
- 测试验证失败、磁盘只读、两个并发 update 三种场景。

### [P1] 5. Task、Session 聚合和每日费用不是同一数据库事务

位置：

- `crates/proxy-store/src/store.rs:96-158`

一次 `write()` 顺序执行建 session、分配 sequence、插 task、更新 session aggregate、更新 daily usage，但没有 SQLite transaction。任一步失败都会保留之前已经提交的部分结果。例如 daily usage upsert 失败时，task 和 session aggregate 已存在，但调用者收到失败；重试又因 task ID 已存在直接返回，不会补写缺失的 daily usage。

这会永久造成任务列表、Session 总计和 Cost 页面互相不一致。

建议：

- 用 `Connection::transaction`/`unchecked_transaction` 包住整个写流程。
- idempotency 检查也放入事务，并明确已存在 task 时如何修复/验证 aggregate。
- 增加故障注入测试，在每个 SQL 步骤失败后断言所有表均回滚。

### [P1] 5A. Session 只累计了基础计费字段，尚未成为完整历史状态

位置：

- `crates/proxy-store/src/db/migration.rs:27-89`
- `crates/proxy-store/src/db/sessions.rs:90-129`
- `crates/proxy-relay/src/relay.rs:422-460`
- `crates/proxy-relay/src/relay.rs:553-600`
- `crates/proxy-common/src/models.rs:346-399`

当前写 Task 时会正确累加请求数、成功/失败数、四类 token 和 cost，这部分设计应该保留。但 Session 缺少状态/结束时间、最新路由与模型、未定价计数、错误分类、累计 duration/TTFT 等已经存在于 Task 的状态。Task cleanup 后，这些信息无法从 Session 恢复。

同时存在两套 Session 模型：`proxy-common::Session` 有 `Recording/Stopped/Archived` 状态机，数据库使用的 `proxy-store::Session` 没有 status 和 ended_at。前者没有进入真实存储链路。

Session 的 cwd/project 信息也没有在 relay 中提取；`NewSessionDefaults` 实际只填了 client type 和 client session ID。`ensure_session` 又用写库时的 `now_ms` 作为 created/first activity，而 relay 是上游响应全部完成后才写库，所以 Session 起始时间偏晚。

建议定义唯一的持久化 `SessionState`，明确哪些字段采用：

- `SUM`：usage、cost、task/error/priced counts、duration。
- `MIN`：started/first activity。
- `MAX`：last activity/ended time。
- `LAST BY sequence`：provider、model、upstream、stop reason、last error、last task ID。
- 状态机：Recording → Stopped → Archived，迁移必须单向且可重放。

所有更新和 Task insert 放在同一事务，archive/delete 不能重新从残存 Task 反算历史累计。

### [P1] 5B. 在线 Summary 的元数据和 final response 是错误或缺失的

位置：

- `crates/proxy-store/src/store.rs:283-308`
- `crates/proxy-store/src/summary/analyzer.rs:67-72`
- `crates/proxy-store/src/summary/analyzer.rs:156-179`
- `crates/proxy-server/src/web/sessions.rs:102-178`

`ProxyStore::summary` 从数据库取得完整 Task 后，却只把 `request_body` 和 `requested_model` 放入临时 `ProxiedRequest`。分析器随后从该 DTO 读取 session ID、timestamp、status、stop reason、token 和 response text，因此这些字段会变成空值、epoch/default 或 0。

当前 Task 的 response body/content text 没有传入，`final_response` fallback 只能从请求 messages 中找最后一个 assistant text。API 请求 body 只包含发送给上游的历史上下文，所以这个文本通常属于上一轮，而不是当前 Task 刚产生的回答。

Session summary handler 又把“最后一个 Task 的 Summary”直接当成 Session Summary；分析失败时只累加仍留在 tasks 表中的明细，cleanup 后会低估历史 token，并丢弃 Session 表已有的累计 cost/cache/count。如果 Task 已全部清理，它虽然能查到 Session，却把 input/output/cache token 全部固定返回 0。

建议：

- analyzer 输入改为 `(&Task, &SessionState)`，直接使用持久化字段。
- 当前 Task response 必须作为 assistant result 的第一来源。
- Session 指标始终来自 SessionState，不能从可清理 Task 反算。
- 分开 Task Summary 和 Session Summary API/schema。

### [P1] 5C. Summary 从未生成到数据库，Archive 也读取了不兼容的 schema

位置：

- `crates/proxy-common/src/models.rs:114-121`
- `crates/proxy-store/src/summary/analyzer.rs:10-26`
- `crates/proxy-store/src/summary/cache.rs:7-36`
- `crates/proxy-store/src/store.rs:335-355`
- `crates/proxy-store/src/archive/format.rs:164-176`

cache 读写的是 `TaskSummary { user_request, assistant_result, touched_files: Vec<String> }`；在线 analyzer 产出的是字段完全不同的 `SessionSummary`。`cache_summary()` 在仓库内没有调用者，`RunCommand::Summary` 只检查已有 cache，从不分析或写入。

样本库也验证了该断链：1,638 个 Task 中 `summary_json` 非空数量为 0。Archive formatter 仍从 `summary_json` 手工读取 `user_request`/`assistant_result`，所以当前所有归档的最后 Task Summary 都是空的。即便直接把现有 analyzer JSON 写进去，字段名和 touched file 类型不匹配，仍无法正确归档。

建议：

- 删除重复 summary 类型，定义有版本号的 `TaskSummaryV1`。
- Task 完成后生成 summary，或在归档前保证最后 Task summary 存在。
- cache、API、前端和 archive 统一使用强类型序列化，禁止 `serde_json::Value` 手工猜字段。
- Summary 成功更新后设置 `archive_dirty`，归档成功后才推进 checkpoint。

### [P1] 6. Inspector 的 Effort 选择值不会参与实际代理请求

位置：

- `crates/proxy-server/src/web/settings.rs:410-439`
- `crates/proxy-common/src/config/routing.rs:49-55`
- `crates/proxy-relay/src/relay.rs:332-375`
- `wwwroot/js/settings.js:864-881`

`PUT /api/effort` 修改 `proxy.active_effort`，但路由结果只从 `UpstreamConfig.effort` 取值，relay 也只使用 `route.effort`。所以 Inspector 中切换 Effort、接口返回成功、TOML 也更新后，发给上游的 body/header 仍不变。

该 handler 还没有发布 `UpstreamChanged`，其他浏览器窗口不会同步。

建议：明确唯一数据源。若 Inspector 是全局即时控制，应由 routing 优先使用 `active_effort` 并广播变更；若 effort 绑定 upstream，则接口应更新当前 upstream 的 `effort`，同时修正文档和字段命名。

### [P1] 7. 全局 HTTP Proxy 配置被保存但从未使用

位置：

- `crates/proxy-server/src/web/settings.rs:511-546`
- `crates/proxy-common/src/config/provider.rs:10-15`
- `crates/proxy-relay/src/relay.rs:316-330`
- `crates/proxy-server/src/main.rs:41-44`

配置模型声明 provider 的 `proxy = None` 应继承 `proxy.http_proxy`，UI 也能保存全局代理；实际 relay 只读取 `provider.proxy`，主 HTTP client 也没有应用全局代理。因此没有 per-provider override 的 provider 永远直连。

建议：按 `Some("") => direct`、`Some(url) => override`、`None => global` 三态解析 effective proxy，并为三种情况写测试。

### [P1] 8. 多个已公开功能是空实现或根本没有接线

位置：

- `crates/proxy-relay/src/relay.rs:119-125`：CONNECT 固定返回 501。
- `crates/proxy-server/src/web/requests.rs:164-173`：单条/批量删除未实现。
- `crates/proxy-server/src/web/sessions.rs:95-100`：Session 删除未实现。
- `crates/proxy-server/src/web/settings.rs:599-602`：选中 Session Flush 未实现，却返回 `ok: true`。
- `crates/proxy-server/src/web/settings.rs:632-637`：Cleanup 未实现，却返回 `ok: true`。
- `crates/proxy-server/src/web/sessions.rs:181-199`：导出仅支持 JSON，前端仍提供 YAML。
- `crates/proxy-relay/src/capture.rs:28-47`：Capture 只有开关，relay 从未读取或写入 capture。
- `crates/proxy-hook-agent/src/main.rs:50-57`：agent POST `/api/hook-event`，router 没有该路由。
- `crates/proxy-relay/src/hook.rs:14-47`：`HookReceiver` 没有被 AppState 或 router 使用。

README 和设计文档明确宣称上述多项能力可用。最危险的是 Flush/Cleanup 返回 HTTP 200 和 `ok: true`，用户会误以为数据已经导出或清理。

建议：未完成前返回 `501 Not Implemented` 并在 UI 禁用入口；随后以端到端测试逐项接通。文档应由实际路由/功能测试生成或至少纳入发布核对。

### [P1] 9. API 错误普遍使用 HTTP 200，前端会把失败当成功

位置示例：

- `crates/proxy-server/src/web/settings.rs:42-45`
- `crates/proxy-server/src/web/settings.rs:273-276`
- `crates/proxy-server/src/web/sessions.rs:63-79`
- `crates/proxy-server/src/web/requests.rs:153-161`
- `wwwroot/js/settings.js:780-793`
- `wwwroot/js/session.js:31-40`

多数 handler 在验证失败、Not Found、数据库错误时只返回 `Json({"error": ...})`，状态仍为 200。前端大量依赖 `resp.ok`，因此会关闭编辑器、更新本地状态或删除 UI 行，即使后端操作失败。

建议：

- 建立统一 `ApiError`，映射 400/404/409/422/500。
- 所有 mutation 返回一致的成功 schema，前端始终解析失败 body。
- 对不存在资源的 update/delete 不应静默成功。

### [P1] 10. 可控 UTF-8 字符串按字节切片，会触发 panic

位置：

- `crates/proxy-relay/src/relay.rs:265-269`
- `crates/proxy-relay/src/relay.rs:510-517`
- `crates/proxy-store/src/store.rs:81-94`

session ID 和上游错误正文都可能包含非 ASCII 字符。代码用 `&s[s.len()-8..]` 和 `&s[..200]` 截断；只要边界落在多字节字符中就会 panic。恶意 session ID 可稳定让请求任务异常终止，上游返回长中文错误也可能触发。

建议：统一使用 `chars()`、`char_indices()` 或已有的 Unicode-safe truncate helper；Session ID 完成 ASCII 校验后仍应避免通用字符串使用字节切片。

### [P2] 11. 归档列表存在持久化 HTML 属性注入

位置：

- `wwwroot/js/utils.js:36-38`
- `wwwroot/js/archive.js:20-34`
- `wwwroot/js/archive.js:54-80`

`escHtml()` 只转义 `&<>`，没有转义单双引号，却被用于 `data-file`、`data-sid` 等 HTML 属性。归档文件名来自客户端可控 session ID；包含引号的 ID 被 flush 后，可闭合属性并注入事件处理器。搜索结果中的 `s.role` 也未经属性转义直接拼进 class。

这与问题 1 共用同一入口，能把恶意 session ID 进一步升级为 Dashboard 中的 stored XSS。

建议：

- 不再拼接带不可信值的 HTML；用 `createElement`、`textContent`、`dataset`、`classList`。
- 如必须模板渲染，使用同时覆盖文本与属性上下文的成熟 escape，并限制 role 枚举。
- 添加包含引号、反引号、换行和 Unicode 的 DOM 测试。

### [P2] 12. Session 详情用模糊匹配取得元数据，可能返回错误 Session

位置：

- `crates/proxy-server/src/web/sessions.rs:52-79`
- `crates/proxy-store/src/db/sessions.rs:196-203`

`GET /api/session/:id` 将精确 ID 传给 `id_or_name`，底层却执行 `id LIKE %q% OR name LIKE %q%`，然后直接取排序后的第一条；task 查询则使用精确 ID。结果可能组合出 A Session 的元数据和 B ID 的 task 列表，也可能把其他 Session 的名称、cwd、项目路径泄露给错误请求。

建议：详情接口直接调用精确 `get_session(id)`；模糊搜索只用于列表搜索，并使用独立 query 参数。

### [P2] 13. 同步 SQLite 和全目录文件扫描直接运行在 Tokio worker 上

位置：

- `crates/proxy-server/src/web/archive.rs:63-103`
- `crates/proxy-store/src/archive/manager.rs:102-136`
- `crates/proxy-store/src/archive/manager.rs:154-264`
- `crates/proxy-store/src/store.rs:40-42`

rusqlite、归档遍历、逐文件全文读取都是同步阻塞调用，直接从 async handler 执行；所有 DB 操作还共享一个 `std::sync::Mutex<Connection>`。归档较多或 flush/search 较慢时，会占住 Tokio worker，拖慢 WebSocket 心跳、Dashboard API 和代理请求收尾。

建议：

- 将阻塞文件/SQLite 操作放到 `spawn_blocking` 或专用 store worker。
- 归档搜索建立索引，至少加入分页、文件大小和总扫描量限制。
- 监控锁等待和 handler 延迟。

### [P2] 14. MCP 转发缺少总超时，目标配置也不持久化

位置：

- `crates/proxy-server/src/main.rs:41-44`
- `crates/proxy-relay/src/mcp.rs:25-38`
- `crates/proxy-relay/src/mcp.rs:89-121`
- `crates/proxy-server/src/web/settings.rs:557-568`

共享 reqwest client 只有 connect timeout，MCP 请求本身没有 `.timeout()`；目标建立连接后不返回 body，会无限占用任务和连接。destination 只保存在内存 `RwLock` 中，重启即丢失，但 UI/API 没有提示这是临时配置。

建议：增加可配置 request/read timeout、body size limit和并发限制；明确 destination 是临时状态还是写入正式配置。

### [P3] 15. 代码质量门禁尚未闭合

检查结果：

- `cargo test --workspace`：通过，45 passed。
- `cargo clippy --workspace --all-targets`：通过但产生 24 个去重后的 warning。
- `cargo clippy --workspace --all-targets -- -D warnings`：失败。
- `cargo fmt --all -- --check`：失败，多个 Rust 文件未格式化。
- `node --check wwwroot/js/*.js`：通过。
- `bash -n EnableProxy.sh DisableProxy.sh statbar.sh build-release.sh wwwroot/statbar.sh`：通过。

Clippy 问题主要包括 module inception、可 derive 的 Default、冗余转换/闭包、参数过多和无效借用。它们不是本报告高风险缺陷的根因，但说明 CI 尚未建立稳定的格式与 lint 基线。

建议在修复高优先级问题后，将 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、单元测试和前端语法检查加入 CI。

## 测试缺口

现有 45 个测试集中在配置纯函数、SSE parser、计费和 summary analyzer；`proxy-server` 为 0 个测试，`proxy-hook-agent` 为 0 个测试。以下关键路径没有自动化覆盖：

- HTTP router 的状态码、错误 schema 和认证。
- WebSocket Origin/认证、断线、lag/resync 和敏感字段。
- 真正的流式首字节转发与客户端中途断开。
- ConfigStore 更新失败回滚和并发写。
- ProxyStore 跨表事务一致性。
- SessionState 对每种 Task status/usage/cost/timing 的累计，以及 Task cleanup 后状态不变。
- Summary 对当前 response、Codex input、失败 tool result、费用和错误的提取。
- Summary schema 从生成、cache、API 到 Archive 的 round-trip。
- 恶意 Session ID、归档路径和前端 DOM 注入。
- Effort、全局代理、Capture、Hook、Flush、Cleanup、删除、YAML 导出的端到端行为。
- 大响应、大 archive、慢 MCP upstream 下的资源上限。

## 建议的修复里程碑

### 第一阶段：安全与数据正确性

修复问题 1、2、4、5、5A、5B、5C、10、11，并补充回归测试。此阶段完成前不建议把 Dashboard 绑定到非 loopback 地址，也不建议启用会删除 Task 的自动 cleanup。

### 第二阶段：代理核心行为

修复问题 3、6、7、14，增加真实 upstream mock server 的集成测试，验证首 chunk、模型/effort/header 改写和 proxy 继承。

### 第三阶段：产品能力与 API 契约

处理问题 8、9、12；未实现功能在完成前明确返回 501，统一 `ApiError` 和 OpenAPI/接口测试。

### 第四阶段：性能与工程门禁

处理问题 13、15，建立阻塞任务边界、分页/限额、CI lint/format 和最小性能基准。
