# Claude Code 的透明代理：在浏览器里切换模型、观察行为、沉淀数字资产

> 一个开源的 Claude Code 透明代理——在网页端实时切换模型供应商、白盒观察每一次 API 调用、把会话记录变成可检索的数字资产。

---

用了几个月 Claude Code 之后，有三个痛点越来越难以忍受：

1. **黑盒运作**：你完全不知道 Claude Code 在底层做了什么——发了什么 API 请求、调了哪些工具、SSE 流里传回了什么数据。

2. **供应商锁定**：想复杂任务用 Opus、简单重构用 DeepSeek 省钱？对不起，得退出会话、改配置文件、重启。要么全用贵的，要么全用便宜的。

3. **记忆消失**：每次会话结束就没了。回看不了最精彩的调试过程，分析不了 token 花在了哪里，学不到过去的 agent 交互经验。

**CC Proxy** 就是来解决这三个问题的。它是一个透明 HTTP 代理，架在 Claude Code 和上游 AI 供应商之间，配合浏览器仪表盘提供实时可见性、动态模型切换和持久化会话分析。

---

## 一键切换模型——甚至不用退出会话

这是最核心的功能。Claude Code 以为自己在跟 Anthropic API 对话，但实际上代理可以根据模型 ID 中的关键词，把不同模型的请求路由到不同供应商——全部在浏览器里配置。

### Tier 路由：跨供应商混搭

```
高优先级  → 匹配 "opus"   → Anthropic (claude-opus-4-6)
中优先级  → 匹配 "sonnet" → Anthropic (claude-sonnet-4-6)
低优先级  → 匹配 "haiku"  → DeepSeek (deepseek-chat)
默认回退  → 其他情况      → OpenAI (gpt-4o)
```

关键之处在于：**不需要重启 Claude Code，不需要手动改配置文件。** 打开仪表盘，点一下下拉框，激活另一个上游。Claude Code 的下一个 API 请求立刻走新的供应商。

真实场景：你正在搞一个复杂的架构重构（Opus），然后需要做 20 个简单的查找替换。切到 DeepSeek 或 Haiku，花 1/10 的钱。下一个难题来了再切回去。全程在同一个会话里完成。

仪表盘还会实时显示当前激活的上游、今日花了多少钱、本月用了多少 token——不会有意外账单。

![CC Proxy Dashboard](../cc-proxy.png)

---

## 白盒观察 Claude Code 的每一次呼吸

你有没有好奇过：当你让 Claude Code 修一个 bug 时，它到底在干什么？

### 实时请求表格

会话管理页面展示每一笔 API 请求：

| 时间 | 方法 | 路径 | 状态 | 模型 | 会话 | 输入/输出 | 费用 | 耗时 | 首 Token |
|------|------|------|------|------|------|-----------|------|------|----------|

点击任意一行，展开查看完整的请求体、响应体和 SSE 事件流——每一个 `content_block_delta`、每一次 `tool_use` 都被捕获并打上时间戳。你终于能看到 agent 为什么选择了某条路径。

### 对话时间线

实时会话视图将 API 请求、Hook 事件（PreToolUse、PostToolUse 等）、MCP 调用混合展示为一条时间线。这对于调试 agent 行为极有价值——你能发现诸如「agent 调了 8 次 Read 才决定 Edit」或「PostToolUse hook 返回了错误但 agent 忽略了」这类模式。

### 费用分析

独立的费用看板，支持时间范围预设（今天 / 本周 / 本月），按模型、会话、供应商三个维度拆分。每个请求按可配置的模型定价实时计费（输入 / cache-write / cache-read / output 四个维度，USD/百万 token）。你能精确知道哪个会话烧掉了预算。

---

## 沉淀你的数字资产

每一次 Claude Code 会话都会**持久化**到本地的 SQLite 数据库里。这不只是日志——这是你 AI 辅助编程的个人知识库。

### 会话复盘

在会话管理中点击任意 Session，右侧 Summary 面板展开：

- **用户提示** — 你问了什么
- **助手动作** — 每一次工具调用（Bash、Read、Write、Edit、Glob、Grep…）
- **涉及文件** — 读了哪些、写了哪些、编辑了哪些，带计数
- **最终响应** — agent 的输出结论
- **统计** — 消息总数、思考块数量、按工具分类的调用次数

这是一份结构化、可检索的问题解决记录。三个月后回来翻看，秒懂当时的思路。

### 导出为可读格式 — Settings 面板一键 Flush

打开仪表盘 → **设置** → Data Retention 面板 → 点击 **Export All Now**（Flush），所有有过请求记录的会话就会导出为 YAML 文件，写入项目根目录的 `sessions/` 下。

Flush 不会删除 SQLite 数据库中的原始数据，只是导出副本。每个 `.yaml` 文件对应一个完整会话。

#### 实际 YAML 文件格式

以本项目的一次真实 Claude Code 会话为例：

```yaml
exported_at: 2026-06-27T12:22:39Z
requests:
  - id: 7712694e-d91
    method: POST
    path: /v1/messages
    model: deepseek-v4-pro
    duration_ms: 3590
    input_tokens: 104
    output_tokens: 273
    message_id: 528c4ec6-...
    request_body:
      max_tokens: 32000
      messages:
        # ── 第一层：用户 prompt + 系统提示（CLAUDE.md 内容、skill 列表等）──
        - role: user
          content:
            - type: text
              text: |
                <system-reminder>
                As you answer the user's questions, ...
                Contents of /Users/.../CLAUDE.md:
                # 项目概述 ...
                </system-reminder>

        # ── 第二层：agent 的工具调用决策 ──
        - role: assistant
          content:
            - type: tool_use
              name: Bash
              input:
                command: "ls doc/"
            - type: tool_use
              name: Bash
              input:
                command: "git log --oneline -10"
            - type: tool_use
              name: Read
              input:
                file_path: "doc/architecture.md"

        # ── 第三层：工具执行结果（命令输出、文件内容）──
        - role: user
          content:
            - tool_use_id: call_00_...
              content: "api.md\narchitecture.md\nbuild.md\n..."
            - tool_use_id: call_01_...
              content: "a4a85f3 feat(cc-proxy): add i18n support..."

        # ── 第四层：agent 继续分析结果，调用下一批工具 ──
        - role: assistant
          content:
            - type: tool_use
              name: Read
              input:
                file_path: "CLAUDE.md"
            - type: tool_use
              name: Read
              input:
                file_path: "doc/frontend.md"

        # ... 以此类推，直到 stop_reason = "end_turn"
    response_body:  # 最终的 API 响应
      stop_reason: tool_use
      usage: { input_tokens: 104, output_tokens: 273 }
```

#### 为什么这很有价值

这份 YAML 不只是日志——它是**一次 AI 辅助编程的完整思维记录**：

- **你的 Prompt** — 包含 CLAUDE.md 注入的完整上下文（项目约定、代码规范、可以调用的 skill），可以看到 agent 收到了哪些信息
- **Agent 的决策链** — Read 了哪些文件、Bash 执行了什么命令、Grep 搜了什么关键词。每一步工具调用都有 `tool_use_id`，可以追踪「这个 Read 是为了验证那个 Edit」
- **工具执行结果** — 命令输出、文件内容、git log 结果，原样保留。你看到的就是 agent 看到的
- **思考进化** — `tool_use` → 读文件 → 理解 → 再 `tool_use` → 修改 → 验证，循环迭代过程完整可见
- **Token 消耗分布** — 每次请求的 input/output token 精确到个位数，找出哪一步最费 token

Claude Code 终端只显示结果摘要（"编辑了 6 个文件"），flush 出来的 YAML 保留了**完整过程**。下次面对类似问题时，翻出旧 session 比凭记忆靠谱得多。

### 为什么这件事重要

你的 agent 会话是**数字资产**。它们记录了：

- 架构决策及其推演过程
- 定位到根因的调试步骤
- 发现并应用的代码模式
- 碰了壁的失败方案（下次可以避开）

大多数开发者在终端窗口关闭的那一刻就丢掉了这些知识。CC Proxy 让它持久化、可检索、可复盘。

---

## 工作原理

```
Claude Code ──► :8888 代理 ──► Anthropic / DeepSeek / OpenAI / 自定义
浏览器     ──► :5000 仪表盘（SPA + REST + WebSocket）
Claude Code ──► :9999 MCP 代理
```

三个端口，一个二进制文件。代理拦截 Anthropic 格式的 API 调用，按你的 Tier 配置路由到对应供应商。所有请求/响应数据存入 SQLite（WAL 模式），通过 WebSocket 实时推送到浏览器。

### 快速开始

```bash
# 从 GitHub Releases 下载预编译版本，或从源码构建：
git clone https://github.com/mushuanli/cc-proxy.git
cd cc-proxy
cargo build -p proxy-server --release

# 将 Claude Code 指向代理
cp settings.json ~/.claude/

# 启动代理
./target/release/proxy-server config.toml

# 浏览器打开 http://localhost:5000
```

### 仪表盘功能一览

| 标签 | 功能 |
|------|------|
| **会话管理** | 请求表格、Session 分组折叠、请求详情、Summary 侧边栏 |
| **费用** | 多维度成本分析，日期范围查询 |
| **设置** | Provider / Upstream / Tier / 模型定价 CRUD |
| **实时会话** | API + Hook + MCP 统一时间线 |
| **MCP 观察器** | MCP JSON-RPC 流量监控 |
| **钩子** | Hook 事件日志 |

界面自动检测浏览器语言（中/英）。

---

## 这能解锁什么

**精准的成本优化**：你可以精确量化每个模型在每个会话中的费用，并实时切换。有用户反馈，将非关键任务路由到更便宜的模型后，每日 Claude Code 账单下降了约 60%。

**首次实现 agent 调试**：Claude Code 的 agent 循环跑得比你终端里看到的快好几步。代理揭示了每一步——包括静默失败的工具调用、agent 没有汇报的重试、以及导致幻觉文件编辑的实际 SSE 流。

**知识留存**：你最好的 agent 交互变成了可检索、可导出的资产。新同事入职时，给他看真实的会话记录。忘记那个奇怪的 bug 怎么修的时候，翻出上个月的会话。

---

CC Proxy 开源（MIT），纯本地运行（`127.0.0.1`），你的数据永远不会离开你的机器。

[GitHub → mushuanli/cc-proxy](https://github.com/mushuanli/cc-proxy)
