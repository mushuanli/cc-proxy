# C4 Level 4 — 动态图

## 请求代理流程（Reverse Proxy 模式）

```mermaid
sequenceDiagram
    participant CC as Claude Code
    participant PH as proxy_handler<br/>(:8888)
    participant Config as UpstreamConfig<br/>+ ModelPricing
    participant DU as dispatch_upstream
    participant SSE as SseParser
    participant API as 上游 API
    participant DB as SQLite
    participant WS as WebSocket<br/>Broadcast
    participant TW as TeeWriter

    CC->>PH: POST /v1/messages<br/>{"model":"claude-sonnet",...}
    PH->>PH: 提取 session_id<br/>(metadata.user_id.session_id)
    PH->>DB: ensure_session(sid)
    PH->>Config: resolve("claude-sonnet", sid)
    Config-->>PH: ("anthropic", "claude-sonnet")
    PH->>Config: model_name_for_provider("anthropic")
    Config-->>PH: "claude-sonnet-4-6"
    PH->>PH: Effort 注入（如 active_effort != "auto"）
    PH->>DU: dispatch_upstream(url, headers, body)

    DU->>API: POST https://api.anthropic.com/v1/messages
    API-->>DU: 200 OK (SSE stream)

    loop 每个 SSE chunk
        DU->>SSE: push(bytes)
        SSE-->>DU: SseEvent[]
        DU->>WS: broadcast SseEvent
        DU->>TW: write(chunk)
    end

    DU->>SSE: finish()
    SSE-->>DU: merged content_text + token stats
    DU->>DU: compute_cost(tokens, ModelPricing)
    DU->>DB: insert_request() + upsert session_daily_usage
    DU->>WS: broadcast RequestUpdated
    DU-->>PH: ProxiedRequest
    PH-->>CC: 200 OK (stream end)
```

## 数据清理流程

```mermaid
sequenceDiagram
    participant Loop as cleanup_loop<br/>(每30min)
    participant DB as SQLite
    participant Export as export::flush_yaml
    participant Dir as sessions/

    Loop->>DB: sessions_with_old_requests(hours, keep_sid)
    DB-->>Loop: [sid1, sid2, ...]

    Note over Loop,Export: Pre-flush: 先归档再清理

    Loop->>DB: list_requests_with_body(sid)
    Loop->>Export: flush_yaml(session, requests)
    Export->>Dir: write sessions/<sid>.yaml

    Note over Loop,DB: cleanup_old_requests 6步

    Loop->>DB: Step 1: 聚合 token 到 sessions 表
    Loop->>DB: Step 1.5: 生成 summary_json
    Loop->>DB: Step 1.6: INSERT OR REPLACE session_daily_usage
    Loop->>DB: Step 1.7: 标记 Archived + ended_at
    Loop->>DB: Step 2: 保留最新请求（tombstone）
    Loop->>DB: Step 3: DELETE 其余旧请求

    Loop->>DB: cleanup_old_sessions(max_count)
    Loop->>DB: delete_old_sessions_by_age(days)
```

## WebSocket 连接生命周期

```mermaid
sequenceDiagram
    participant Client as 浏览器
    participant WS as ws_handler
    participant BC as broadcast::Sender
    participant DB as SQLite

    Client->>WS: GET /ws (Upgrade)
    WS-->>Client: 101 Switching Protocols
    WS->>DB: list_hooks()
    WS->>Client: HookHistory
    WS->>DB: list_mcp()
    WS->>Client: McpHistory
    WS->>Client: McpConfigChanged
    WS->>Client: UpstreamChanged
    WS->>Client: TeeStatusChanged

    loop 每10s
        WS->>Client: Ping
        Client-->>WS: Pong
    end

    loop 事件循环
        BC-->>WS: WsMessage
        WS->>Client: forward
    end

    Note over WS,Client: 300s 无 Pong → 断开<br/>200s/220s/240s 分级警告

    Client->>Client: 前端指数退避重连<br/>1s→2s→4s→...→30s
```

## 配置变更流程

```mermaid
sequenceDiagram
    participant Client as 前端
    participant API as api_handler
    participant State as AppState
    participant FS as config.toml
    participant BC as broadcast::Sender

    Client->>API: PUT /api/upstreams/:name/activate
    API->>State: active_upstream = name
    API->>State: active_effort = upstream.effort (if Some)
    API->>FS: persist_config()
    API->>BC: broadcast UpstreamChanged
    BC-->>Client: UpstreamChanged
    Client->>Client: 刷新 provider/upstream/effort 下拉
```

## Cost 查询去重逻辑

```mermaid
sequenceDiagram
    participant Client as 前端
    participant API as GET /api/costs
    participant DB as SQLite

    Client->>API: GET /api/costs?from=2026-07-01&to=2026-07-17
    API->>DB: get_cost_data(from, to)

    Note over DB: by_model: 直接从 requests 聚合
    Note over DB: by_provider: 直接从 requests 聚合
    Note over DB: by_session: requests LEFT JOIN sessions

    Note over DB: by_day: UNION ALL
    DB->>DB: 活跃请求: SELECT from requests<br/>WHERE input_tokens IS NOT NULL
    DB->>DB: 归档数据: SELECT from session_daily_usage<br/>WHERE (date, session_id) NOT IN<br/>(SELECT DISTINCT ... FROM requests)
    Note over DB: NOT IN 子查询防止双计<br/>（session 跨归档边界时）

    DB-->>API: CostData { by_model, by_provider, by_session, by_day }
    API-->>Client: JSON
```
