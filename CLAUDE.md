# CC Proxy — AI 助手配置

## 语言
- 始终使用**中文**交流
- 代码注释用英文，Git commit 用 Conventional Commits (`type(scope): description`)

## 开发原则
SOLID / DRY / KISS / YAGNI / CoC / LoD — 函数≤30行，圈复杂度≤10

---

## 项目概述

Claude Code API 透明代理 — 拦截、可视化、分析 AI Coding Agent 的 API 流量。
- **语言**: Rust (2021 edition, Cargo workspace)
- **前端**: Vanilla JS/HTML/CSS，通过 `rust-embed` 内嵌到二进制，支持 i18n（`wwwroot/assets/zh.json`）
- **数据库**: SQLite（`data/datav2.db`，WAL 模式）
- **Session**: 相同 `session_id` 的一组请求（Request）,里面每个请求都是一个task。Session 状态机：Recording → Stopped/Archived（cleanup 时保留最新请求作为"墓碑"）
- **Model Pricing**: 全局模型定价（`ModelPricing`），含 `price: [input, output, cache_write?, cache_read?]` 和 `providers: {provider → [model_names]}` 映射

## 文档索引

| 文档 | 内容 |
|------|------|
| [设计文档](./doc/design.md) | 系统设计、Crate 分层、Config/Store 分离、事件驱动通信、模块职责、数据流、横切关注点 |
| [配置体系](./doc/config.md) | ModelPricing、Provider、TierRule、UpstreamConfig、ProxyConfig、持久化 |
| [Proxy 代理](./doc/proxy.md) | 三种代理模式、dispatch_upstream、Effort 注入、重试、SSE 解析、模型翻译、计费 |
| [数据库](./doc/database.md) | 表结构、数据清理流程、聚合查询、去重逻辑 |
| [API & WebSocket](./doc/api.md) | REST 端点、WS 消息类型、握手流程、生命周期 |
| [前端](./doc/frontend.md) | 7 个视图 Tab、JS 模块、状态变量、过滤逻辑、数据加载流程、实时更新 |
| [构建 & 安全](./doc/build.md) | 构建命令、端口配置、安全注意事项 |
| [C4 系统上下文](./doc/c4/context.md) | C4 Level 1 — 系统上下文图（Mermaid） |
| [C4 容器](./doc/c4/container.md) | C4 Level 2 — 容器图（Mermaid） |
| [C4 组件](./doc/c4/component.md) | C4 Level 3 — proxy-server 组件图（Mermaid） |
| [C4 动态](./doc/c4/dynamic.md) | C4 Level 4 — 请求代理、数据清理、WebSocket、配置变更、Cost 去重动态图 |

---

## 调试

定位错误时，在 Rust 代码中增加 `tracing` 日志，携带 session / request ID 前 8 字符上下文，不要靠猜测。日志前缀用 `[模块]`（`[proxy]`、`[summary]`、`[api]`），定位完毕移除临时日志。
