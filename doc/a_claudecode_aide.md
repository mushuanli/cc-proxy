# CC Proxy: Turbocharged Debugging for AI Coding Agents

> A fully transparent Claude Code proxy — a browser dashboard to switch models mid-session, observe every API call in real time, and build your own digital asset library.

---

LLM coding agents are magical — until they aren't. **Claude Code** is one of the best agents out there, but after a few weeks of daily driving it, three pain points become impossible to ignore:

1. **Black box**: you have zero visibility into what Claude Code is actually doing under the hood — what API calls, what tool invocations, what SSE events.

2. **Vendor lock-in**: you're stuck with one provider's pricing and performance. Want to use DeepSeek for simple refactors and switch to Opus for complex architecture? Good luck editing config files mid-session.

3. **No memory**: every session disappears into the ether. Can't review your best debugging sessions, can't analyze your token spending patterns, can't learn from past agent interactions.

**CC Proxy** exists to fix all three. It's a transparent HTTP proxy that sits between Claude Code and upstream AI providers, with a browser dashboard that gives you real-time visibility, on-the-fly model switching, and persistent session analytics.

---

## Switch Models in One Click — Even Mid-Session

This is the headline feature. Claude Code connects to CC Proxy thinking it's talking to the Anthropic API. But the proxy can route different model requests to different providers based on keywords in the model ID — all configurable from the browser.

### Tier routing: mix and match providers

```
High tier    → keyword "opus"   → Anthropic (claude-opus-4-6)
Mid tier     → keyword "sonnet" → Anthropic (claude-sonnet-4-6)
Low tier     → keyword "haiku"  → DeepSeek (deepseek-chat)
Default fallback → OpenAI (gpt-4o)
```

Here's what makes this powerful: **you don't need to restart Claude Code or edit config files.** Open the dashboard, click a dropdown, activate a different upstream. Claude Code's next API call gets routed to the new provider instantly.

Real use case: you're working on a complex refactor (Opus), then need to do 20 simple find-and-replace operations. Switch to DeepSeek or Haiku for 1/10th the cost. Switch back for the next hard problem. All in the same session.

The dashboard also shows you exactly which upstream is active and how much you've spent today/this month — so there are no surprises.

![CC Proxy Dashboard](cc-proxy.png)

---

## White-Box Your Claude Code Sessions

Ever wonder what Claude Code is *really* doing when you ask it to fix a bug?

### Real-time request table

The Inspector view shows every API request live:

| Time | Method | Path | Status | Model | Session | In/Out | Cost | Duration | TTFT |
|------|--------|------|--------|-------|---------|--------|------|----------|------|

Click any row to drill into the full request/response body and the SSE event stream — every `content_block_delta` and `tool_use` event is captured and timestamped. You can finally see *why* an agent went down a particular path.

### Conversation timeline

The Timeline view merges API requests, Hook events (PreToolUse, PostToolUse, etc.), and MCP calls into a single chronological feed. This is incredibly valuable for debugging agent behavior — you can spot patterns like "the agent called `read` 8 times before deciding to `edit`" or "the PostToolUse hook returned an error that the agent ignored".

### Cost analytics

Independent cost dashboard with time-range presets (Today / This Week / This Month), breakdown by Model, Session, and Provider. Every request is priced against configurable per-model rates (input / cache-write / cache-read / output tokens, in USD per million tokens). You'll know exactly which sessions burned through your budget.

---

## Build Your Digital Asset Library

Every Claude Code session is **persisted** in a local SQLite database. This isn't just a log — it's your personal knowledge base of AI-assisted engineering.

### Session review

Click any session in the Inspector to open the Summary panel:

- **User Prompts** — what you asked
- **Assistant Actions** — every tool call (Bash, Read, Write, Edit, Glob, Grep...)
- **Touched Files** — reads/writes/edits with counts
- **Final Response** — the agent's concluding output
- **Stats** — message count, thinking blocks, tool calls by type

This is a structured, searchable record of how you solved problems. Come back to a session from 3 months ago and instantly understand the approach.

### Export to readable YAML — one-click flush in Settings

Open the dashboard → **Settings** → Data Retention panel → click **Export All Now** (Flush). All sessions with recorded requests are exported as YAML files into the project's `sessions/` directory.

Flush does **not** delete data from SQLite — it's an export, not a migration. Each `.yaml` file corresponds to one complete session.

#### Actual YAML format

Here's a real session export from this project:

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
        # ── Layer 1: user prompt + system context (CLAUDE.md, skill list, etc.) ──
        - role: user
          content:
            - type: text
              text: |
                <system-reminder>
                As you answer the user's questions, ...
                Contents of /Users/.../CLAUDE.md:
                # Project overview ...
                </system-reminder>

        # ── Layer 2: agent decides which tools to call ──
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

        # ── Layer 3: tool execution results (command output, file contents) ──
        - role: user
          content:
            - tool_use_id: call_00_...
              content: "api.md\narchitecture.md\nbuild.md\n..."
            - tool_use_id: call_01_...
              content: "a4a85f3 feat(cc-proxy): add i18n support..."

        # ── Layer 4: agent analyzes results, calls next batch of tools ──
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

        # ... continues until stop_reason = "end_turn"
    response_body:  # final API response
      stop_reason: tool_use
      usage: { input_tokens: 104, output_tokens: 273 }
```

#### Why this is valuable

This YAML is not just a log — it's a **complete record of AI-assisted engineering thought**:

- **Your prompts** — including the full CLAUDE.md context injected by Claude Code (project conventions, skill definitions, code guidelines). You see exactly what the agent received.
- **Agent's decision chain** — which files it Read, what commands it ran via Bash, what it Grepped for. Each tool call has a `tool_use_id` so you can trace "this Read was to validate that Edit".
- **Tool execution results** — command stdout, file contents, git log output, preserved verbatim. What you see is what the agent saw.
- **Thinking evolution** — the full `tool_use` → read → understand → `tool_use` → modify → verify loop, every iteration visible.
- **Token distribution** — input/output tokens per request, down to the integer. Pinpoint which step burned the most.

Claude Code's terminal shows result summaries ("edited 6 files"). The flushed YAML preserves the **complete process**. When you face a similar problem next time, a 3-month-old session file is far more reliable than memory.

### Why this matters

Your agent sessions are **digital assets**. They capture:
- Architecture decisions and their rationale
- Debugging steps that led to the root cause
- Code patterns you discovered and applied
- Failed approaches that you can avoid next time

Most developers lose this institutional knowledge the moment the terminal window closes. CC Proxy makes it persistent and reviewable.

---

## How It Works

```
Claude Code ──► :8888 proxy ──► Anthropic / DeepSeek / OpenAI / custom
Browser     ──► :5000 dashboard (SPA + REST + WebSocket)
Claude Code ──► :9999 MCP proxy
```

Three ports, one binary. The proxy intercepts Anthropic-format API calls and routes them according to your tier configuration. All request/response data is stored in SQLite (WAL mode) and pushed to the browser via WebSocket in real time.

### Getting started

```bash
# Download from GitHub Releases, or build from source:
git clone https://github.com/mushuanli/cc-proxy.git
cd cc-proxy
cargo build -p proxy-server --release

# Configure Claude Code to route through the proxy
cp settings.json ~/.claude/

# Start the proxy
./target/release/proxy-server config.toml

# Open http://localhost:5000
```

### What you'll see

1. **会话管理** (Session Management) — request table, session folding, detail panel, summary sidebar
2. **费用** (Cost) — multi-dimension cost analytics
3. **设置** (Settings) — provider/upstream/tier/pricing CRUD
4. **实时会话** (Live Timeline) — unified API + Hook + MCP feed
5. **MCP 观察器** (MCP Observer) — MCP JSON-RPC traffic
6. **钩子** (Hooks) — hook event log

The UI auto-detects your browser language (zh/en).

---

## What This Unlocks

**Cost optimization with surgical precision**: you can quantify exactly how much each model costs per session and switch in real time. One user reported dropping their daily Claude Code bill by 60% after routing non-critical tasks to cheaper models.

**Agent debugging for the first time**: Claude Code's agent loop runs several steps ahead of what you see in the terminal. The proxy reveals every step — including tool calls that failed silently, retries the agent didn't report, and the actual SSE stream that led to a hallucinated file edit.

**Knowledge preservation**: your best agent interactions become a searchable, exportable asset. When onboarding a new team member, show them actual session records. When you forget how you fixed that obscure bug, pull up the session from last month.

---

CC Proxy is open source (MIT), runs locally on `127.0.0.1`, and never sends your data anywhere. [GitHub → mushuanli/cc-proxy](https://github.com/mushuanli/cc-proxy)
