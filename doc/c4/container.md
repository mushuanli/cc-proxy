# C4 Level 2 — 容器图

```mermaid
C4Container
    title CC Proxy — 容器

    Person(admin, "管理员", "浏览器用户")

    System_Boundary(cc_proxy, "CC Proxy") {
        Container(spa, "仪表盘 SPA", "Vanilla JS/HTML/CSS", "前端单页应用，7 个视图 Tab，内嵌到二进制")
        Container(api, "REST API", "axum 0.7", "配置 CRUD、Session 管理、成本聚合、导出、Archive")
        Container(ws, "WebSocket", "axum WebSocket", "实时推送：请求、SSE 事件、Hook、MCP、配置变更")
        Container(proxy, "API 代理", "reqwest 0.12", "Anthropic API 透明代理，三种模式，Tier 路由，SSE 解析")
        Container(mcp_proxy, "MCP 代理", "reqwest", "MCP JSON-RPC 透传代理")
        Container(db, "SQLite", "rusqlite (bundled)", "WAL 模式，6 张表，请求/Session/Hook/MCP 持久化")
        Container(config, "config.toml", "toml_edit", "运行时配置持久化，Provider/Upstream/Pricing/Retention")
        Container(hook_agent, "proxy-hook-agent", "Rust CLI", "独立二进制，stdin 读取 Hook JSON → POST 到仪表盘")
        Container(tee, "Tee Recorder", "文件 I/O", "录制开关，捕获原始请求/响应到 captures/ 目录")
    }

    System_Ext(anthropic, "Anthropic API", "Claude 推理服务")
    System_Ext(upstream, "第三方 API", "OpenRouter 等")
    System_Ext(mcp_dest, "MCP Server", "JSON-RPC 服务")

    Rel(admin, spa, "查看", "HTTPS :5000")
    Rel(spa, api, "REST 调用", "JSON")
    Rel(spa, ws, "实时推送", "WSS")

    Rel(api, db, "CRUD", "SQL")
    Rel(api, config, "读写配置", "TOML")
    Rel(ws, db, "读取首态", "SQL")

    Rel(proxy, db, "写请求/SSE", "SQL")
    Rel(proxy, anthropic, "转发", "HTTPS")
    Rel(proxy, upstream, "Tier 路由", "HTTPS")
    Rel(proxy, tee, "录制", "文件写入")

    Rel(mcp_proxy, mcp_dest, "转发 JSON-RPC", "HTTP")
    Rel(mcp_proxy, db, "写 MCP 记录", "SQL")

    Rel(hook_agent, api, "POST hook-event", "HTTP :5000")

    UpdateLayoutConfig($c4ShapeInRow="3", $c4BoundaryInRow="1")
```

## 容器职责

| 容器 | 端口 | 技术 | 说明 |
|------|------|------|------|
| **仪表盘 SPA** | :5000（静态文件） | Vanilla JS | 7 个视图，rust-embed 内嵌 |
| **REST API** | :5000 | axum 0.7 | ~40 个端点，配置 CRUD + 数据查询 |
| **WebSocket** | :5000 (/ws) | axum WS | 18 种消息类型，10s ping，broadcast 频道 |
| **API 代理** | :8888 | reqwest 0.12 | CONNECT/Forward/Reverse 三种模式，SSE 解析，重试 |
| **MCP 代理** | :9999 | reqwest | JSON-RPC 透传 |
| **SQLite** | 本地文件 | rusqlite (bundled) | WAL 模式，6 张表，busy_timeout 5s |
| **config.toml** | 本地文件 | toml_edit | 保留格式和注释的持久化 |
| **hook-agent** | 独立 CLI | reqwest + clap | stdin → POST，静默失败 |
| **Tee Recorder** | 本地文件 | 标准 I/O | 按日期+session 组织，合并 SSE 内容块 |
