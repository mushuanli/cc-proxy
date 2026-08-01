# Optimize Task — 实时会话与请求通知优化

## 1. 结论

采用“**任务开始时持久化、任务结束时原子终结、前端以轻量 WS 事件驱动**”的实现。

现有 `ProxyStore::task_write()` 是仅插入接口：相同 `TaskId` 再次调用时只返回旧记录，不会更新任务，也不会更新费用和 session 聚合。因此不能通过“使用同一个 `task_id` 再次调用 `task_write()`”完成状态更新。本方案新增独立的 `task_finalize()`，并明确开始和终结阶段不同的聚合语义。

基本数据流：

```text
有效请求完成解析与路由
  │
  ├── task_start(Recording)
  │     └── 单事务：插入 task + 增加 task_count
  │
  ├── 持久化成功后发布 NewRequest
  │
  └── dispatch_upstream
          │
          ├── 正常响应       → task_finalize(Completed)
          ├── HTTP/上游错误  → task_finalize(Failed)
          └── 流处理被中断   → task_finalize(Interrupted)
                    │
                    └── 终结成功后发布 RequestUpdated + CostUpdated
```

前端的数据流：

```text
首次连接 / 重连 / Resync
  └── REST snapshot
          └── requestRows（唯一任务列表状态源）
                  ├── Inspector
                  └── Conversation（排序后取最新 100 条）

连接正常
  └── WS 增量事件 → reducer → requestRows → 重绘受影响视图
```

## 2. 目标与边界

### 2.1 目标

1. Conversation 实时显示 task 的 `prompt`，点击后进入对应 Inspector 详情。
2. 有效请求在调用上游前以 `Recording` 状态写入 store。
3. `NewRequest` 发布时，`GET /api/request/{id}` 必须已经可查询。
4. 每个 task 最多产生一次 `NewRequest` 和一次终态 `RequestUpdated`。
5. 正常连接期间，task/session/cost 列表依赖 WS 增量更新，不因每条 task 事件请求 session REST。
6. 请求详情按需加载，并对并发请求和相同状态版本去重。
7. 浏览器断线、WS 消息丢失或服务重启后可以恢复一致状态。

### 2.2 非目标

- 不实现逐 token 的 Inspector 详情更新。
- 不实现服务端 WS 事件日志和断点重放。
- 不默认通过 WS 广播所有请求/响应 body。
- 无效 JSON、路由解析失败、请求序列化失败不创建 task；这些错误发生在任务被系统接受之前，继续通过 HTTP 错误和服务日志诊断。

如果后续要求记录所有入站失败，应单独设计 `RejectedRequest`，不要混入正常 task 状态机。

## 3. 必须保持的不变量

1. `NewRequest(id)` 只能在 `task_start(id)` 事务成功后发布。
2. `RequestUpdated(id)` 只能在 `task_finalize(id)` 事务成功后发布。
3. task 状态只能执行以下迁移：

   ```text
   Recording → Completed | Failed | Cancelled | Interrupted
   ```

4. `task_count` 每个 task 只增加一次。
5. token、费用、耗时、completed/failed 计数每个 task 最多累计一次。
6. 重复 finalize 必须幂等，不能重复累计聚合数据或重复发布事件。
7. WS 轻量 payload 中的空 body 不能覆盖已经通过 REST 加载的详情缓存。
8. 重连或 `Resync` 后，REST snapshot 与同步期间收到的 WS 事件必须合并，不能丢失较新的事件。

## 4. 后端实施方案

### 4.1 WS 数据模型

修改 `crates/proxy-common/src/models.rs`：

1. 为 `ProxiedRequest` 增加任务状态：

   ```rust
   pub status: TaskStatus,
   ```

2. 增加 session 增量事件：

   ```rust
   SessionUpdated(SessionSnapshot),
   ```

   `SessionSnapshot` 只包含前端列表需要的字段：

   - `id`
   - `label`
   - `status`
   - `cwd`
   - `project_key`
   - `started_at`
   - `ended_at`
   - task/completed/failed 数
   - token、费用和最新 model/provider

3. `NewRequest` 和 `RequestUpdated` 都必须携带：

   - `id`
   - `status`
   - `timestamp`
   - `session_id`
   - `method`
   - `path`
   - `prompt`
   - `model`
   - `provider`
   - `is_streaming`
   - 已知的状态码、token、费用和耗时

4. `ws_include_bodies=false` 仍为默认值。关闭时不广播 headers、request body、response body、SSE 和 `content_text`，避免大对象和敏感内容发送给所有 Dashboard 客户端。

5. 扩展 `CostStats`，补充月度 input/output token。当前工具栏显示月度 token，但现有 WS payload 只有月度费用；如果取消周期 REST，必须让 `CostUpdated` 覆盖工具栏需要的全部字段：

   ```rust
   pub month_input_tokens: i64,
   pub month_output_tokens: i64,
   ```

   `usage::query_cost_stats()` 在现有月度费用聚合中一并查询这两个字段，避免增加额外 SQL 请求。

`status` 是详情缓存版本的最小依据：一个 task 在前端最多有 `recording` 和一个终态两个详情版本，本需求不需要额外数据库 revision。

### 4.2 Store API

修改 `crates/proxy-store/src/store.rs`，将任务生命周期拆成两个明确接口。

#### 4.2.1 `task_start`

```rust
pub async fn task_start(
    &self,
    session_id: &SessionId,
    task: NewTask,
) -> StoreResult<TaskStartResult>;
```

约束：

- 只接受 `TaskStatus::Recording`。
- `ended_at`、响应、usage、duration 和 error 必须为空或为零。
- task ID 冲突时返回 `AlreadyExists`，不修改任何聚合。
- 在一个 `BEGIN IMMEDIATE` 事务中完成：
  1. 创建或读取 session。
  2. 分配 `sequence_no`。
  3. 插入 Recording task。
  4. session `task_count += 1`。
  5. session priced/unpriced task 数增加一次。
  6. daily usage `task_count += 1`。
- 返回持久化后的 task 和最新 session snapshot。

可复用现有 `task_write()` 的插入逻辑，但应重命名或限制为内部实现，避免调用方误认为它支持更新。

#### 4.2.2 `task_finalize`

新增只包含终态可变字段的输入类型，避免重新提交并覆盖 task 的不可变请求字段：

```rust
pub struct TaskFinalization {
    pub status: TaskStatus,
    pub first_byte_at: Option<i64>,
    pub ended_at: i64,
    pub response_headers: Option<serde_json::Value>,
    pub response_body: Option<NormalizedResponse>,
    pub http_status_code: Option<u16>,
    pub usage: TaskUsage,
    pub timing: TaskTiming,
    pub error: Option<TaskError>,
    pub metadata_patch: serde_json::Value,
}

pub enum TaskFinalizeResult {
    Applied { task: Task, session: Session },
    AlreadyFinalized { task: Task },
}
```

接口：

```rust
pub async fn task_finalize(
    &self,
    task_id: &TaskId,
    finalization: TaskFinalization,
) -> StoreResult<TaskFinalizeResult>;
```

约束：

- 只接受 `Completed`、`Failed`、`Cancelled` 或 `Interrupted`。
- 先读取现有 task；不存在时返回 `NotFound`。
- 已经是终态时返回 `AlreadyFinalized`，不更新聚合。
- 在一个 `BEGIN IMMEDIATE` 事务中执行条件更新：

  ```sql
  UPDATE tasks
  SET status = ?,
      first_byte_at = ?,
      ended_at = ?,
      response_headers_json = ?,
      response_body = ?,
      http_status_code = ?,
      input_tokens = ?,
      output_tokens = ?,
      cache_creation_tokens = ?,
      cache_read_tokens = ?,
      duration_ms = ?,
      ttft_ms = ?,
      stop_reason = ?,
      upstream_message_id = ?,
      error_type = ?,
      error_message = ?,
      cost_microusd = ?,
      metadata_json = ?
  WHERE id = ? AND status = 'recording'
  ```

- 只有条件更新实际修改一行时才：
  - 增加 completed/failed 计数。
  - 累计 token、费用、duration、TTFT。
  - 更新 session 的 latest/last 字段及 `archive_dirty`。
  - 更新 daily usage，但不再次增加 `task_count`。
- daily usage 日期必须由原 task 的 `started_at` 计算，不能使用 finalize 时的当前日期，避免跨 UTC 午夜的任务被拆到两天。
- `metadata_patch` 必须是 JSON object，与开始阶段 metadata 做受控浅合并；非 object 返回参数错误，不能用 `null` 或数组覆盖全部诊断信息。

将现有聚合函数拆成语义明确的函数：

```text
sessions::record_task_started()
sessions::record_task_finalized()
usage::record_task_started()
usage::record_task_finalized()
```

不要实现通用 `task_upsert()`；它难以保证聚合增量只执行一次。

### 4.3 Relay 生命周期

修改 `crates/proxy-relay/src/relay.rs`。

#### 4.3.1 生成统一 task 上下文

完成 JSON 解析、session 解析、路由、billing、effort/model 转换和 body 序列化后，在 `dispatch_upstream()` 前生成：

```rust
let task_id = TaskId::generate();
let task_started_at =
    chrono::Utc::now().timestamp_millis() - start.elapsed().as_millis() as i64;
```

同一请求的所有分支必须复用该 ID 和开始时间，禁止在 dispatch 错误分支重新生成。

构造 Recording task：

- 保存经过脱敏的 request headers。
- 保存 `stored_request_body()` 的结果。
- 提前设置 `prompt_text`。
- 设置 session defaults、billing、protocol、upstream mode。
- `ended_at=None`、usage 为零、无 response/error。
- `is_streaming` 使用原始请求值。

在 task start 前执行 capture 裁剪逻辑，确保 capture 关闭时也能保留最后一个真实用户消息和 `prompt_text`。

#### 4.3.2 开始任务

```text
task_start 成功
  ├── publish NewRequest(status=Recording)
  └── publish SessionUpdated

task_start 失败
  ├── 不发布任何 task WS 事件
  ├── 不调用上游，避免产生不可追踪的计费请求
  └── 返回 503 Service Unavailable
```

初始写入不发布 `CostUpdated`，因为此时 token 和费用均为零。

#### 4.3.3 非流式完成

处理完上游响应后构造 `TaskFinalization`：

- HTTP 状态码 `< 400` 且没有解析/上游错误：`Completed`
- HTTP 状态码 `>= 400` 或存在上游错误：`Failed`

调用 `task_finalize()`：

- `Applied`：发布 `RequestUpdated`、`SessionUpdated`、`CostUpdated`。
- `AlreadyFinalized`：记录 debug 日志，不重复发布。
- store 错误：记录 error，不发布与数据库不一致的 `RequestUpdated`；上游响应仍返回客户端。

#### 4.3.4 流式完成与中断

保留后台等待 stream metadata 的结构，但所有退出路径必须终结 task：

```rust
match stream_meta.await {
    Ok(meta) => finalize Completed/Failed,
    Err(err) => finalize Interrupted,
}
```

不能再出现 `Err(_) => return`。

本地 SQLite finalize 失败时进行有限重试，例如最多 3 次指数退避。重试仍失败则：

- 保留 Recording，方便后续恢复和诊断。
- 记录 task ID、session ID 和错误。
- 不发布终态事件。

不为正常长流设置任意短超时；上游超时策略仍由现有 request/stream 配置控制。

#### 4.3.5 进程重启恢复

服务开始接受请求前执行一次恢复：

```text
将本次进程启动时间之前仍为 Recording 的 task 标记为 Interrupted
```

恢复必须走 store 事务逻辑：

- 不重复增加 task 数。
- 不产生 token 和费用。
- 更新 task/session 最终状态。
- 是否广播恢复事件不是必须，因为客户端连接后会加载 snapshot。

这可以处理进程崩溃、强制退出和后台任务被取消留下的 Recording。

### 4.4 发布顺序

替换现有“无论 store 是否成功都 publish”的 `write_and_publish()`。拆分为：

```text
start_and_publish()
finalize_and_publish()
```

两者都必须遵守：

```text
store transaction successful → publish
store transaction failed     → do not publish
```

`CostUpdated` 只在 finalize `Applied` 后查询和发布。

## 5. 前端实施方案

### 5.1 单一任务列表状态源

继续使用：

```javascript
state.requestRows = new Map();
```

Inspector 和 Conversation 都从 `requestRows` 派生，不单独保存 timeline item 数据。

新增纯 reducer：

```javascript
function applyTaskEvent(payload) {
    const previous = state.requestRows.get(payload.id) || {};
    const next = mergeTaskSummary(previous, payload);
    state.requestRows.set(payload.id, next);
}
```

`mergeTaskSummary()` 只合并 WS/REST 列表字段，不管理详情 body。这样轻量 WS 中的 `null` 不会删除详情缓存。

终态优先级高于 `Recording`。初始化 snapshot 较晚返回时，不允许把已经收到的终态事件降级为 Recording。

### 5.2 详情缓存和请求去重

在 `wwwroot/js/state.js` 增加：

```javascript
detailCache: new Map(),      // key: `${id}:${status}`
detailFetches: new Map(),    // key: `${id}:${status}`, value: Promise
```

`loadRequestDetail(req)`：

1. 如果当前 WS payload 带有完整 body，直接写入对应状态版本缓存。
2. 命中 `detailCache` 时直接返回。
3. 命中 `detailFetches` 时复用同一个 Promise。
4. 否则请求一次 `/api/request/{id}`。
5. 请求结束后删除 `detailFetches`。
6. HTTP 非 2xx 必须显示可见错误，不能吞异常。

默认 `ws_include_bodies=false` 时：

- 用户首次点击 Recording task：最多 fetch 一次 Recording 详情。
- 该 task 进入终态且当前仍被选中：最多 fetch 一次终态详情。
- 未被选中的 task 不自动 fetch 详情。

这保留 WS 主驱动，同时避免默认广播敏感且体积较大的 body。

如果 `ws_include_bodies=true`，终态事件包含完整 body 时直接更新详情缓存，不发 REST。

### 5.3 WS 消息处理

`handleMessage()` 调整为：

```text
NewRequest / RequestUpdated
  ├── applyTaskEvent
  ├── render Inspector
  ├── render Conversation
  ├── 如果是当前选中 task，更新详情
  └── 不调用 fetchSessionMeta

SessionUpdated
  ├── 更新 sessionMeta
  ├── 更新 sessionCache
  └── 重绘 session/filter

CostUpdated
  └── applyCostStats

Cleared
  └── 清空 requestRows、detailCache、timeline filter 和选中状态

Resync
  └── resyncState("lagged")
```

删除 `NewRequest`/`RequestUpdated` 路径下的 `fetchSessionMeta(sid)`。

`fetchSessionMeta()` 可以保留给 Summary 等明确的用户操作，但不得由每条 task WS 事件触发。

### 5.4 初始化、重连和 Resync

封装统一的：

```javascript
async function resyncState(reason)
```

用于：

- 首次 WS 连接成功
- 断线重连成功
- 收到 `Resync`

同步流程：

1. 设置 `state.syncing=true`。
2. 同步期间把 task/session WS 事件放入 `pendingEvents`。
3. 并行加载 `/api/sessions` 和 `/api/requests?limit=2000`。
4. 用 snapshot 构造新的 request/session Map。
5. 按接收顺序重放 `pendingEvents`。
6. 原子替换前端状态并统一重绘。
7. 设置 `state.syncing=false`。

只允许一个 resync Promise 运行；同步期间再次请求 resync 时设置一次 queued 标志，当前同步完成后再执行一次。

这样可以消除以下竞态：

- WS 事件先到、旧 REST snapshot 后到。
- REST snapshot 查询期间产生新任务。
- 浏览器断线期间漏掉事件。
- broadcast buffer 溢出。

正常连接期间不轮询 task/session。成本统计保留首次加载以及重连/Resync 恢复，之后依赖 `CostUpdated`；移除 `renderPage()` 间接触发的周期性 cost REST 更新。

### 5.5 Conversation 视图

将 `addToTimeline()` 改为派生渲染：

```javascript
function renderTimeline() {
    // requestRows → filter → timestamp DESC → slice(0, 100) → DOM
}
```

每个条目：

- `data-request-id`：task ID。
- `data-session`：session ID。
- 第一行：`prompt`，无 prompt 时显示 `method + path`，最多 80 个字符。
- 第二行：时间、状态、HTTP 状态码、model、token、耗时。
- Recording 显示明确的进行中样式。
- Failed/Interrupted 显示错误样式。

要求：

1. `RequestUpdated` 更新原条目，不追加重复 DOM。
2. 排序依据 task `timestamp`，状态更新不改变原始时间顺序。
3. 初始化和 Resync 后立即从 snapshot 显示最近 100 条。
4. `Cleared` 后清空列表和 `convSessions`。
5. session filter 使用 `sessionCache` 名称，未知名称回退到短 SID。
6. 使用 DOM API 设置 `textContent`/`dataset`，prompt、method、path、model 和 label 不直接拼接未转义 HTML。

### 5.6 点击跳转

在 `wwwroot/js/main.js` 或 `inspector.js` 提供唯一入口：

```javascript
export async function navigateToRequest(id)
```

流程：

1. 从 `requestRows` 查找 task；不存在时显示错误。
2. 激活 Inspector 导航。
3. 展开 task 所在 session。
4. 计算并切换到正确分页。
5. 调用 `showRequestDetail(req)`。
6. `requestAnimationFrame()` 后滚动并高亮对应行。

Conversation 使用事件委托处理点击，避免每次重绘重复绑定监听器。

fullscreen 当前通过复制 `innerHTML` 生成，因此不会复制元素监听器。fullscreen 容器也必须绑定同一个事件委托，或者明确只由外层统一捕获 `data-request-id` 点击。

### 5.7 样式

样式放在 `wwwroot/css/inspector.css`，不在 `index.html` 中增加内联样式。

新增或调整：

- prompt 摘要
- 元信息行
- Recording 动画或状态点
- Completed/Failed/Interrupted 左边框颜色
- hover、focus 和可点击光标
- 键盘焦点样式

条目使用 `button` 或设置 `tabindex="0"`、`role="button"`，同时支持 Enter/Space。

## 6. 影响文件

| 文件 | 改动 |
|---|---|
| `crates/proxy-common/src/models.rs` | `ProxiedRequest.status`、`SessionSnapshot`、`SessionUpdated`、完整的 `CostStats` |
| `crates/proxy-store/src/models.rs` | `TaskFinalization`、start/finalize 返回类型 |
| `crates/proxy-store/src/store.rs` | `task_start()`、`task_finalize()`、事务和幂等控制 |
| `crates/proxy-store/src/db/tasks.rs` | Recording 插入、条件终态更新 |
| `crates/proxy-store/src/db/sessions.rs` | start/finalize 两阶段聚合 |
| `crates/proxy-store/src/db/usage.rs` | start/finalize 两阶段 daily usage、月度 token 统计 |
| `crates/proxy-relay/src/relay.rs` | dispatch 前持久化、统一 task ID、所有退出路径 finalize |
| `crates/proxy-server/src/main.rs` | 启动时恢复遗留 Recording task |
| `crates/proxy-server/src/web/requests.rs` | 确保 REST list/detail 状态字段一致 |
| `wwwroot/js/state.js` | detail cache、in-flight fetch、resync 状态 |
| `wwwroot/js/main.js` | WS reducer、初始化/重连/Resync、导航入口 |
| `wwwroot/js/inspector.js` | 详情缓存和 fetch 去重 |
| `wwwroot/js/timeline.js` | 从 `requestRows` 派生 Conversation |
| `wwwroot/js/cost.js` | 正常连接期间取消周期 REST 刷新 |
| `wwwroot/css/inspector.css` | Conversation 状态和交互样式 |

数据库已有 task `status` 和终态所需字段，不需要 schema migration。

## 7. 实施顺序

1. 实现并测试 store 的 `task_start()`/`task_finalize()`。
2. 增加 WS `status` 和 `SessionUpdated` 数据模型。
3. 修改 relay 生命周期和发布顺序。
4. 增加启动恢复逻辑。
5. 实现前端 reducer、详情缓存和 resync。
6. 将 Conversation 改成 `requestRows` 派生视图。
7. 移除 task 事件触发的 session/cost REST 请求。
8. 完成集成测试和浏览器人工验收。

后端存储语义应先完成，避免前端先依赖尚不存在的一致性保证。

## 8. 异常与极端情况

| 场景 | 预期行为 |
|---|---|
| 初始 task 写入失败 | 返回 503，不请求上游，不发布 `NewRequest` |
| dispatch transport 失败 | 同一 ID 从 Recording 变为 Failed |
| 上游 HTTP 4xx/5xx | 终结为 Failed，保存状态码和错误 |
| 非流式解析失败 | 终结为 Failed，原始响应信息保留在 metadata |
| `stream_meta.await` 失败 | 终结为 Interrupted，不能直接 return |
| finalize 被重复调用 | 第二次返回 `AlreadyFinalized`，聚合不变化 |
| finalize 临时失败 | 有限重试；仍失败则保留 Recording 并记录错误 |
| 进程崩溃 | 下次启动将遗留 Recording 转为 Interrupted |
| 请求跨 UTC 午夜 | daily usage 归属 task 开始日期 |
| WS payload 不含 body | 列表正常更新；只有选中详情按状态版本 fetch 一次 |
| WS payload 含完整 body | 写入详情缓存，不额外 fetch |
| WS 断线 | 重连后执行完整 resync |
| WS buffer lagged | 收到 `Resync` 后执行完整 resync |
| snapshot 与 WS 并发 | snapshot 完成后重放同步期间事件 |
| 收到旧 Recording 事件 | 不覆盖本地同 ID 的终态 |
| 第 101 个 Conversation task | 移除时间最旧条目 |
| 恶意 prompt/label | 作为文本渲染，不执行 HTML/脚本 |

## 9. 测试与验收

### 9.1 Store 单元测试

1. `task_start` 创建 Recording task。
2. 相同 ID 重复 start 不增加 task/session/daily usage 数。
3. Recording → Completed 正确写入响应、token、费用和聚合。
4. Recording → Failed 正确写入错误和 failed count。
5. Recording → Interrupted 不增加 completed/failed count。
6. 重复 finalize 不重复累计任何数据。
7. finalize 不存在的 ID 返回 `NotFound`。
8. finalize 已是终态的 task 返回 `AlreadyFinalized`。
9. finalize 事务任一步失败时完整回滚。
10. 跨 UTC 午夜仍更新开始日期的 daily usage。

### 9.2 Relay 集成测试

1. 非流式成功事件顺序：

   ```text
   NewRequest(Recording) → RequestUpdated(Completed)
   ```

2. 流式成功时收到 `NewRequest` 后 REST 详情立即可查询。
3. dispatch 失败复用同一个 task ID。
4. HTTP 500 终结为 Failed。
5. stream metadata channel 关闭时终结为 Interrupted。
6. task start 失败时上游 mock 未被调用。
7. finalize 失败时不发布虚假终态事件。
8. `ws_include_bodies=true/false` 均符合事件契约。

### 9.3 前端测试

1. `NewRequest` 和 `RequestUpdated` 只保留一个 task/timeline 条目。
2. 终态不会被迟到的 Recording snapshot 降级。
3. 同一 `id + status` 的并发详情请求只产生一个 fetch。
4. 未选中的 task 更新不请求详情。
5. 新 session 通过 `SessionUpdated` 增量加入筛选器。
6. 初始化、重连、`Resync` 和 `Cleared` 后两个视图一致。
7. 同步期间事件在 snapshot 后正确重放。
8. 100 条上限和 session filter 正确。
9. Conversation 点击、键盘操作和 fullscreen 点击均可跳转。
10. prompt、path、model、label 的 XSS 输入只作为文本显示。

### 9.4 人工验收

浏览器 Network 面板应满足：

- 正常 WS 连接期间，`NewRequest`/`RequestUpdated` 不触发 `/api/session/{id}`。
- 未选中的 task 不触发 `/api/request/{id}`。
- 默认不含 body 时，选中 task 对每个状态版本最多请求一次详情。
- 不再出现“已收到 `NewRequest`，随后详情接口 404”。
- 断线重连和 `Resync` 只进行一次合并后的完整同步，不形成请求风暴。
- Conversation 在首次加载时已有历史快照，并实时更新 prompt 和终态。
