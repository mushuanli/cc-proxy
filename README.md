# CC Proxy

Claude Code API 透明代理 — 拦截、可视化、分析 AI Coding Agent 的 API 流量。

> [:us: English version](./README_en.md)

![screenshot](cc-proxy.png)

## 特点

### 1. 网页端配置，会话中无缝切换供应商

Claude Code 的 Provider、Upstream、Tier 路由全部在**网页仪表盘**上管理，无需修改配置文件或重启代理。支持在**同一个会话中**随时切换上游供应商 — 复杂任务切 Opus、简单任务切 Haiku 或 DeepSeek，兼顾性能与成本。不仅支持同供应商不同模型，更可以**跨供应商组合**（如 High tier → Anthropic Opus、Low tier → DeepSeek），代理根据请求模型 ID 自动路由到正确供应商。

### 2. 实时跟踪交互流程

WebSocket 推送每一笔 API 请求的完整生命周期 — 从发起请求、流式 SSE 事件，到 Hook 触发、MCP 工具调用，全链路实时可见。实时会话视图按时间线混合展示 API + Hook + MCP 事件，还原 Claude Code 的真实执行过程。

### 3. 以可读方式查看会话内容

点击任意 Session 即可在 Summary 面板中查看结构化会话内容：User Prompts、Assistant Actions（工具调用）、Touched Files（文件读写统计）、Final Response。支持 JSON/YAML 导出会话，以及批量 flush 到 `sessions/` 目录，方便离线复盘。

### 4. 实时计费

基于可配置的模型定价（input / cache-write / cache-read / output，USD/百万 token），实时计算每次请求的费用。会话管理工具栏展示**当日/本月**的 Token 消耗和费用统计。独立 Cost 视图支持按日期范围、按 Model、按 Session、按 Provider 多维度聚合分析。

### 5. 事后分析与复盘

所有请求和会话持久化在本地 SQLite 数据库中。可以回看历史 Session 的完整交互记录，复盘问题解决思路、工具调用链、文件修改范围，用于总结和提高 Claude Code 使用技能。

---

## 快速开始

### 1. 下载预编译版本（推荐）

从 [GitHub Releases](https://github.com/mushuanli/cc-proxy/releases) 下载对应平台的压缩包，解压后得到 3 个文件：

| 文件 | 用途 |
|------|------|
| `proxy-server` / `proxy-server.exe` | 代理主程序 |
| `config.toml` | 代理配置模板，编辑填入 Provider API Key，启动后可在网页端可视化编辑 |
| `settings.json` | Claude Code 配置，覆盖 `~/.claude/settings.json`（详见步骤 3） |

```bash
# 1. 编辑 config.toml，填入你的 API Key
# 2. 启动
./proxy-server config.toml          # macOS / Linux
proxy-server.exe config.toml        # Windows
```

> **升级提醒**：升级前请先备份 `config.toml` 和 `data.db`，避免配置或历史数据丢失。

### 2. 从源码构建

需要 Rust 1.80+

```bash
git clone git@github.com:mushuanli/anki-tookit.git
cd cc-proxy

cargo build -p proxy-server --release
cargo run -p proxy-server --release -- config.toml
```

### 3. 配置 Claude Code

**方式一：一条命令自动配置（推荐）**

```bash
./proxy-server --install          # 默认代理端口 8888
./proxy-server --install --port 8888   # 指定端口
```

会自动修改 `~/.claude/settings.json`（修改前备份为 `settings.json.bak`，多次执行生成 `.bak.1`, `.bak.2` ...），写入：

| 字段 | 值 |
|------|-----|
| `ANTHROPIC_BASE_URL` | `http://localhost:8888` |
| `ANTHROPIC_AUTH_TOKEN` | `sk-dummy` |
| `ANTHROPIC_MODEL` | `claude-sonnet-pro[1m]` |
| `ANTHROPIC_SMALL_FAST_MODEL` | `claude-sonnet-flash[1m]` |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | `claude-opus-pro[1m]` |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | `claude-sonnet-flash[1m]` |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `claude-haiku-flash[1m]` |
| `CLAUDE_CODE_SUBAGENT_MODEL` | `claude-sonnet-pro[1m]` |

```bash
# 如需还原，卸载会将以上字段从 settings.json 中移除（同样会先备份）
./proxy-server --uninstall
```

**方式二：手动覆盖 settings.json**

```bash
cp ~/.claude/settings.json ~/.claude/settings.json.bak   # 备份原有配置
cp settings.json ~/.claude/
```

也可以不覆盖，手动在原有 `settings.json` 中添加关键环境变量：

```json
"env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8888",
    "ANTHROPIC_AUTH_TOKEN": "sk-dummy"
}
```

> **Windows 用户注意**：`settings.json` 位于 `%USERPROFILE%\.claude\` 目录下。

### 4. 打开仪表盘

浏览器访问 **http://localhost:5000**

---

## 使用流程

```
Claude Code ──► :8888 代理 ──► 上游 Provider（Anthropic / DeepSeek / 自定义）
浏览器    ──► :5000 仪表盘 ──► 实时查看请求、切换 Provider、查看费用
Claude Code ──► :9999 MCP 代理
```

### 配置 Provider

1. 打开仪表盘 → **Settings** → **Providers** 面板
2. 点击 **+ Add**，填入名称、API 地址、API Key
3. 添加模型及四字段定价（in / cache-write / cache-read / out，USD/百万 token）
4. 可添加多个 Provider（Anthropic、DeepSeek、第三方代理等）

### 配置 Tier 路由

1. Settings → **Upstreams** 面板 → 点击 **+ Add**
2. 配置四层 Tier 路由：
   - **High** — 匹配 `opus` 等关键词 → 路由到指定 Provider/Model
   - **Mid** — 匹配 `sonnet` 等关键词
   - **Low** — 匹配 `haiku` 等关键词
   - **Default** — 都不匹配时的回退
3. 点击 **Activate** 或会话管理顶部下拉框一键切换

### 查看费用

- 会话管理表格 **Cost** 列实时显示每个请求费用
- 工具栏展示**当日/本月** Token 消耗与费用汇总
- **Cost** 标签页支持按日期范围、Model、Session、Provider 多维度分析

### 查看会话内容

- 点击 Session 行 → 右侧 **Summary 面板**展示 User Prompts、Assistant Actions、Touched Files、Final Response
- 面板内可直接 Rename / Export（JSON/YAML）/ Delete 会话
- **实时会话**标签页 — API + Hook + MCP 混合时间线，按 Session 过滤

---

## 仪表盘功能

| 标签 | 功能 |
|------|------|
| **会话管理** | 请求表格（分页/筛选/多选删除），Session 分组折叠，行内详情（Request/Response/SSE），Upstream/Effort 选择器，Cost 统计，右侧 Summary 面板 |
| **费用** | 成本分析 — 日期预设（Today/Week/Month）、摘要卡片、按 Model/Session/Provider 分组明细 |
| **设置** | 全宽 accordion 4 面板：Model Pricing（独立定价 CRUD）+ Providers + Upstreams + Data Retention |
| **实时会话** | 实时时间线（API + Hook + MCP 混合），按 Session 过滤，最多 100 条 |
| **MCP Observer** | MCP JSON-RPC 请求列表 + 目标地址配置 |
| **Hooks** | Hook 事件表格（Event/Session/CWD/ExitCode），最多 200 条 |

---

## 端口

| 端口 | 用途 |
|------|------|
| **5000** | 仪表盘 SPA + REST API + WebSocket |
| **8888** | Anthropic API 透明代理 |
| **9999** | MCP JSON-RPC 透明代理 |

---

## 配置参考

```toml
# config.toml
[logging]
level = "info"

[proxy]
active_upstream = "deepseek"

[[proxy.providers]]
name = "deepseek"
url = "https://api.deepseek.com/anthropic"
token = "sk-xxx"

[[proxy.providers.models]]
id = "deepseek-v4-pro"
price_per_million_input = 3.0
price_per_million_cache_write = 3.75
price_per_million_cache_read = 0.3
price_per_million_output = 6.0

[[proxy.upstreams]]
name = "deepseek"
default_provider = "deepseek"
default_model = "deepseek-v4-pro"

[proxy.upstreams.high]
keywords = ["opus"]
provider = "deepseek"
model = "deepseek-v4-pro"

[server]
http_port = 5000
proxy_port = 8888
mcp_proxy_port = 9999
listen_address = "127.0.0.1"
```

详细配置见 `config.toml.template`。

---

## 安全

- 所有端口仅监听 `127.0.0.1`
- `x-api-key`、`Authorization` header 自动脱敏为 `[REDACTED]`
- API token 存本地 TOML 文件，前端仅展示 `has_token: bool`
- 数据存本地 SQLite（`data.db`），不对外暴露

## License

MIT
