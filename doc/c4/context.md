# C4 Level 1 — 系统上下文图

```mermaid
C4Context
    title CC Proxy — 系统上下文

    Person(user, "开发者", "使用 Claude Code 进行 AI 辅助编码")
    Person(admin, "管理员", "通过浏览器查看 API 流量和成本")

    System(cc_proxy, "CC Proxy", "拦截、可视化、分析 Claude Code 的 API 流量")

    System_Ext(anthropic_api, "Anthropic API", "Claude 模型推理服务")
    System_Ext(upstream_api, "第三方 API\n(OpenRouter / 自定义)", "替代上游 LLM 服务")
    System_Ext(claude_code, "Claude Code CLI", "AI Coding Agent 客户端")
    System_Ext(mcp_server, "MCP Server", "Model Context Protocol 服务端")

    Rel(user, claude_code, "使用", "CLI")
    Rel(claude_code, cc_proxy, "POST /v1/messages", "HTTP :8888 (Reverse Proxy)")
    Rel(claude_code, cc_proxy, "JSON-RPC", "HTTP :9999 (MCP Proxy)")
    Rel(admin, cc_proxy, "查看仪表盘", "HTTPS :5000 (SPA + WebSocket)")
    Rel(cc_proxy, anthropic_api, "转发 API 请求", "HTTPS")
    Rel(cc_proxy, upstream_api, "Tier 路由转发", "HTTPS")
    Rel(cc_proxy, mcp_server, "转发 MCP 请求", "HTTP")

    UpdateLayoutConfig($c4ShapeInRow="2", $c4BoundaryInRow="1")
```

## 说明

| 角色 | 描述 |
|------|------|
| **开发者** | 日常使用 Claude Code 的软件工程师，通过设置 `ANTHROPIC_BASE_URL` 环境变量将流量指向代理 |
| **管理员** | 通过浏览器打开仪表盘，查看实时请求、session 摘要、成本分析，管理配置 |
| **CC Proxy** | 透明代理核心，3 端口架构，SQLite 持久化 |
| **Anthropic API** | 默认上游，Claude 模型官方 API |
| **第三方 API** | OpenRouter 等兼容 Anthropic API 格式的第三方服务 |
| **Claude Code CLI** | Anthropic 官方 AI Coding Agent，发起 API 请求 |
| **MCP Server** | MCP 协议服务端，提供工具扩展 |
