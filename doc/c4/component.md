# C4 Level 3 — 组件图（proxy-server）

```mermaid
C4Component
    title CC Proxy — proxy-server 组件

    Container_Boundary(proxy_server, "proxy-server crate") {

        Component(main, "main / AppState", "axum + tokio", "入口点、CLI 解析、3 端口启动、cleanup_loop、backfill、persist_config")

        Component(proxy_handler, "proxy::proxy_handler", "axum Handler", "三种代理模式：CONNECT tunnel / Forward / Reverse。提取 session_id、Tier 路由、effort 注入、模型翻译")

        Component(dispatch, "proxy::dispatch_upstream", "reqwest", "执行上游请求：重试（指数退避）、超时控制、流式/非流式分发、计费")

        Component(sse_parser, "sse::SseParser", "proxy-core", "SSE 字节流解析：\n\n /\r\n\r\n 分隔，提取 Anthropic 事件字段（delta_text、tokens、stop_reason）")

        Component(api_handler, "api::build_router", "axum Router", "~40 个 REST 端点：Providers/Upstreams/ModelPricing CRUD、Session、Request、Hook、MCP、Costs、Export、Archive、Flush、Cleanup")

        Component(ws_handler, "ws::ws_handler", "axum WebSocket", "WebSocket 生命周期：10s ping、300s dead、broadcast 转发、连接首态推送")

        Component(mcp_handler, "mcp::mcp_handler", "axum Handler", "MCP JSON-RPC 透传：header 过滤、目标 URL 配置、错误处理")

        Component(tee_writer, "tee::TeeWriter", "文件 I/O", "录制开关：按日期+session 写入 captures/，合并 SSE 内容块，文件句柄缓存")

        Component(config_lib, "config", "proxy-core", "AppConfig、ModelPricing、Provider、TierRule、UpstreamConfig、Retention、验证、默认值")

        Component(models_lib, "models", "proxy-core", "ProxiedRequest、Session、HookEvent、McpRequest、SseEvent、WsMessage、成本聚合类型")

        Component(db_lib, "db::Database", "proxy-core", "SQLite CRUD：迁移、请求/Session/Hook/MCP CRUD、清理（6 步）、聚合查询、摘要缓存")

        Component(summary_lib, "summary", "proxy-core", "SessionSummary 分析：消息解析、工具调用提取、文件操作统计")

        Component(export_lib, "export", "proxy-core", "4 种导出格式：JSON/HAR/Markdown/YAML + flush_yaml 归档")

        Component(store_lib, "store::RingBuffer", "proxy-core", "泛型线程安全环形缓冲区 RwLock<VecDeque<T>>")
    }

    Container(spa, "仪表盘 SPA", "Vanilla JS", "前端")
    ContainerDb(sqlite, "SQLite", "data.db", "持久化")
    ContainerDb(toml, "config.toml", "TOML", "配置文件")
    ContainerDb(sessions_dir, "sessions/", "YAML", "归档目录")
    ContainerDb(captures_dir, "captures/", "TXT", "录制目录")

    Rel(spa, api_handler, "REST / WebSocket", "JSON")
    Rel(api_handler, db_lib, "使用", "")
    Rel(api_handler, config_lib, "使用", "")
    Rel(ws_handler, db_lib, "使用", "")

    Rel(proxy_handler, dispatch, "调用", "")
    Rel(proxy_handler, config_lib, "使用", "")
    Rel(dispatch, sse_parser, "流式响应时使用", "")
    Rel(dispatch, db_lib, "持久化", "")

    Rel(mcp_handler, db_lib, "持久化", "")

    Rel(main, proxy_handler, "路由", "")
    Rel(main, api_handler, "路由", "")
    Rel(main, ws_handler, "路由", "")
    Rel(main, mcp_handler, "路由", "")
    Rel(main, tee_writer, "管理", "")
    Rel(main, config_lib, "加载/持久化", "")

    Rel(db_lib, sqlite, "读写", "SQL")
    Rel(main, toml, "persist_config()", "toml_edit")
    Rel(export_lib, sessions_dir, "flush_yaml()", "")
    Rel(tee_writer, captures_dir, "录制写入", "")

    UpdateLayoutConfig($c4ShapeInRow="3", $c4BoundaryInRow="1")
```

## 组件交互矩阵

| 调用方 | 被调用方 | 关系 |
|--------|---------|------|
| `main` | `proxy_handler` | 路由 HTTP :8888 请求 |
| `main` | `api_handler` | 路由 HTTP :5000 请求 |
| `main` | `ws_handler` | 路由 WS :5000 升级 |
| `main` | `mcp_handler` | 路由 HTTP :9999 请求 |
| `proxy_handler` | `dispatch` | 执行上游请求 |
| `proxy_handler` | `config` | Tier 路由、模型翻译、effort |
| `dispatch` | `sse_parser` | 流式响应时解析 SSE |
| `dispatch` | `db` | 持久化请求和 SSE 事件 |
| `api_handler` | `db` | 全部数据查询和 CRUD |
| `api_handler` | `config` | 读取/验证配置 |
| `ws_handler` | `db` | 读取首态（Hook、MCP 历史） |
| `main` | `config` | 加载 → 验证 → 持久化 |
| `export` | `db` | 读取 session 请求列表（含 body） |

## proxy-core 公共接口

```
config:  AppConfig, ModelPricing, Provider, TierRule, UpstreamConfig,
         ProxyConfig, ServerConfig, Retention, ResolvedPrice
models:  ProxiedRequest, Session, SessionStatus, HookEvent, McpRequest,
         SseEvent, WsMessage, HasId, CostData, ModelCost, ProviderCost,
         SessionCost, DailyCost, ProviderInfo, UpstreamInfo, TierRuleInfo
db:      Database, DbError, db_error, extract_latest_user_prompt
sse:     SseParser
export:  export_json, export_har, export_markdown, export_yaml,
         flush_yaml, read_yaml_meta
summary: analyze_request, SessionSummary, UserPrompt, AssistantAction,
         FileOperation, SessionStats
store:   RingBuffer<T>
```
