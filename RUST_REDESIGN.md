# CC Proxy — Rust 重构设计方案

## 1. 项目概述

**原始项目**: [CodingAgentExplorer](https://github.com/tndata/CodingAgentExplorer)（现更名为 cc-proxy）— 基于 .NET 10 + ASP.NET Core + YARP 的 AI Coding Agent API 流量拦截与可视化工具。

**核心功能**:
- 透明反向代理，拦截 Claude Code ↔ Anthropic API 的全量请求/响应
- SSE 流式响应实时解析与捕获
- MCP (Model Context Protocol) 代理观测器
- Hook 事件采集（通过配套 CLI 工具）
- 实时仪表盘（WebSocket 推送 + 原生前端 SPA）
- **会话录制与导出**（JSON / HAR / Markdown），支持 Tee Writer 旁路抓包

**重构目标**: 用 Rust 重写，保持功能等价，获得更低的资源开销、单二进制分发、更强的类型安全。

---

## 2. 技术栈映射

| 层级 | .NET 10 原始 | Rust 替代 | 选型理由 |
|------|-------------|-----------|---------|
| **运行时** | .NET 10 CLR | Tokio | 异步事实标准，io_uring / epoll / kqueue |
| **Web 框架** | ASP.NET Core (Kestrel) | Axum 0.8 | Tower 生态，类型安全路由，原生 WebSocket |
| **反向代理** | YARP | Axum handler + reqwest | 流量需深度解析，handler 模式比中间件更可控 |
| **实时推送** | SignalR | Axum WebSocket + `tokio::sync::broadcast` | 轻量 pub/sub，零依赖 |
| **序列化** | System.Text.Json | serde + serde_json | 编译期 derive，零成本反序列化 |
| **HTTP 客户端** | YARP Forwarder | reqwest (hyper 后端) | 成熟稳定，支持流式 body |
| **CLI** | .NET Console | clap 4 + reqwest | 类型安全参数解析 |
| **日志** | ILogger | tracing + tracing-subscriber | 结构化日志，span 追踪 |
| **配置** | appsettings.json | figment 或 config crate | TOML/YAML 多源配置 |
| **TLS** | Kestrel HTTPS | rustls + axum-server | 纯 Rust TLS，无需 OpenSSL |
| **前端** | Vanilla JS/HTML/CSS | 保持不变，嵌入 binary | 零构建步骤，已足够好 |

---

## 3. 架构总览

### 3.1 进程与端口模型

```
┌─────────────────────────────────────────────────────────────┐
│                     Rust 单体二进制                           │
│                                                             │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                    Axum Router                        │  │
│  │                                                       │  │
│  │  :8888 ──► proxy::anthropic::handler                  │  │
│  │  :9999 ──► proxy::mcp::handler                        │  │
│  │  :5000 ──► ws::hub + api::* + static_files           │  │
│  │  :5001 ──► ws::hub + api::* + static_files (TLS)     │  │
│  └───────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌───────────────────────────────────────────────────────┐  │
│  │                   共享状态层 (AppState)                │  │
│  │                                                       │  │
│  │  store::RequestStore   (Arc<RwLock<RingBuffer>>)      │  │
│  │  store::HookStore      (Arc<RwLock<RingBuffer>>)      │  │
│  │  store::McpStore       (Arc<RwLock<RingBuffer>>)      │  │
│  │  config::McpTarget     (Arc<RwLock<Option<Url>>>)     │  │
│  │  ws::Broadcaster       (broadcast::Sender<Event>)     │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘

外部:
  Claude Code ──► :8888 ──► api.anthropic.com
  Claude Code ──► :9999 ──► MCP Server (用户配置)
  浏览器      ──► :5000/:5001 (仪表盘 SPA)
  HookAgent   ──► :5000/api/hook-event (hook 事件)
```

### 3.2 Crate 拆分策略

```
proxy/
├── Cargo.toml               # workspace root
├── crates/
│   ├── proxy-core/          # 核心库：store、models、config、broadcaster
│   ├── proxy-server/        # Axum 服务：路由、handler、middleware、static files
│   └── proxy-hook-agent/    # HookAgent CLI binary
├── wwwroot/                 # 前端静态资源（嵌入）
└── config.toml              # 默认配置文件
```

**拆分理由**:
- `proxy-core`: 纯逻辑，无 I/O 依赖（除 serde/tokio 基础），可独立测试
- `proxy-server`: 组装层，依赖 axum/reqwest/rustls，启动入口
- `proxy-hook-agent`: 独立 CLI，最小依赖（clap + reqwest + serde_json）

---

## 4. 核心模块设计

### 4.1 请求/响应捕获代理 (`proxy::anthropic`)

```
                     ┌──── Incoming ────┐
Claude Code ──► axum handler            │
                │                       │
                ├─ 1. 读取 request body  │
                ├─ 2. 存入 RequestStore  │──► WebSocket 推送
                ├─ 3. 构建 reqwest 请求  │
                ├─ 4. 转发至 Anthropic   │
                │                       │
                ├─ 5a. 非流式:           │
                │   读取完整 response    │
                │   解析 tokens/stop     │
                │   返回 JSON body       │
                │                       │
                └─ 5b. 流式 (SSE):       │
                    逐行读取 response    │
                    解析 content_block_* │
                    记录 TimeToFirstToken│
                    返回 StreamingBody   │──► Claude Code
```

**核心 Handler 签名**:

```rust
// 统一代理入口：根据 Content-Type / anthropic-version header 判断路由
async fn anthropic_proxy(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    // 1. 捕获请求 → ProxiedRequest
    // 2. 转发 → reqwest
    // 3. 解析响应 → 更新 ProxiedRequest
    // 4. Store + Broadcast
    // 5. 返回
}
```

**SSE 解析器** (`proxy::sse`):

```rust
/// 将 SSE 字节流解析为结构化事件
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent>;
    fn parse_content_block_delta(data: &str) -> Option<ContentDelta>;
    fn parse_message_start(data: &str) -> Option<MessageStart>;
    fn parse_message_delta(data: &str) -> Option<MessageDelta>;
}
```

**响应体解压** — 需要处理 `gzip`、`brotli`、`deflate` 三种压缩：

```rust
use async_compression::tokio::bufread::{GzipDecoder, BrotliDecoder, DeflateDecoder};

async fn decompress_body(encoding: &str, body: Bytes) -> Result<Vec<u8>> {
    match encoding {
        "gzip" => { /* GzipDecoder */ }
        "br"   => { /* BrotliDecoder */ }
        "deflate" => { /* DeflateDecoder */ }
        _ => Ok(body.to_vec()),
    }
}
```

### 4.2 MCP 代理 (`proxy::mcp`)

MCP 使用 JSON-RPC 2.0 over HTTP，无流式：

```rust
async fn mcp_proxy(
    state: AppState,
    req: Request<Body>,
) -> Response<Body> {
    // 1. 读取 body → 解析 JSON-RPC method
    //    常见: initialize, tools/list, tools/call, resources/read
    // 2. 存入 McpStore
    // 3. 转发到配置的 MCP destination
    // 4. 返回原始响应
}
```

**未配置目标时的兜底**（JSON-RPC 错误格式）：

```rust
fn mcp_not_configured() -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "error": {
            "code": -32603,
            "message": "MCP proxy destination not configured"
        },
        "id": null
    }))
}
```

### 4.3 存储层 (`store`)

三种存储逻辑完全一致，抽象为泛型环形缓冲区：

```rust
/// 线程安全的有界环形缓冲区（无锁版本）
pub struct RingBuffer<T> {
    inner: Arc<RwLock<VecDeque<T>>>,
    capacity: usize,
}

impl<T: Clone + Send + Sync + 'static> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self;
    pub fn push(&self, item: T);        // 超量自动 evict
    pub fn get_all(&self) -> Vec<T>;    // 快照
    pub fn get_by_id(&self, id: &str) -> Option<T> where T: Identifiable;
    pub fn clear(&self);
    pub fn len(&self) -> usize;
}

// 类型别名
pub type RequestStore = RingBuffer<ProxiedRequest>;  // capacity = 1000
pub type HookStore    = RingBuffer<HookEvent>;       // capacity = 1000
pub type McpStore     = RingBuffer<ProxiedRequest>;  // capacity = 500
```

对比原版 `ConcurrentQueue`，`RwLock<VecDeque>` 对于读多写少的场景（仪表盘读取远多于 API 写入）性能更好。

### 4.4 WebSocket 实时推送 (`ws`)

SignalR 替换为原生 WebSocket + broadcast channel：

```rust
pub struct Broadcaster {
    tx: broadcast::Sender<WsMessage>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum WsMessage {
    NewRequest(ProxiedRequest),
    NewHook(HookEvent),
    NewMcp(ProxiedRequest),
    Cleared,
    McpCleared,
    McpConfigChanged { destination_url: Option<String> },
    History { requests: Vec<ProxiedRequest> },
    HookHistory { events: Vec<HookEvent> },
    McpHistory { requests: Vec<ProxiedRequest> },
}
```

**Hub 连接生命周期**:

```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    state: AppState,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    // 1. 发送历史数据 (History / HookHistory / McpHistory / McpConfigChanged)
    // 2. 订阅 broadcast::Receiver
    // 3. select! { ws.recv() | broadcast.recv() }
    //    - 收到 broadcast → 转为 WsMessage::Text → ws.send()
    //    - ws 断开 → 退出
}
```

### 4.5 API 端点 (`api`)

| 方法 | 路径 | 功能 | 对应原版 |
|------|------|------|---------|
| POST | `/api/hook-event` | 接收 HookAgent 上报的 hook 事件 | HookAgent → Dashboard |
| PUT | `/api/hook-event/{id}` | 编辑 hook 响应（exitCode/stdout/stderr） | 预留 |
| POST | `/api/clear` | 清空所有 store | DashboardHub.ClearAll |
| POST | `/api/clear-mcp` | 清空 MCP store | DashboardHub.ClearMcp |
| PUT | `/api/mcp-destination` | 设置 MCP 目标 URL | McpProxyConfig.SetDestination |
| GET | `/api/mcp-destination` | 获取当前 MCP 目标 | — |
| GET | `/api/health` | 健康检查 | — |
| GET | `/api/sessions` | 列出所有已录制的会话 | 新增 |
| GET | `/api/session/{id}` | 获取单个会话的完整请求列表 | 新增 |
| GET | `/api/session/{id}/export?format=json` | 导出会话（JSON 格式） | 新增 |
| GET | `/api/session/{id}/export?format=har` | 导出会话（HAR 格式） | 新增 |
| GET | `/api/session/{id}/export?format=markdown` | 导出会话（可读 Markdown） | 新增 |
| POST | `/api/session/start` | 开始录制新会话（可选标签） | 新增 |
| POST | `/api/session/{id}/stop` | 停止录制 | 新增 |
| POST | `/api/request/capture` | 切换 Tee Writer 开关（实时写文件） | 新增 |

### 4.6 模型层 (`models`)

```rust
// — ProxiedRequest —
pub struct ProxiedRequest {
    pub id: String,                    // UUID 前12位
    pub timestamp: DateTime<Utc>,
    // Request
    pub method: String,
    pub path: String,
    pub request_headers: HashMap<String, String>,
    pub request_body: Option<String>,
    pub model: Option<String>,
    pub is_streaming: bool,
    pub max_tokens: Option<u32>,
    // Response
    pub status_code: Option<u16>,
    pub response_headers: HashMap<String, String>,
    pub response_body: Option<String>,
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    // Streaming
    pub sse_events: Vec<SseEvent>,
    // Timing
    pub duration_ms: Option<u64>,
    pub time_to_first_token_ms: Option<u64>,
    // Error
    pub error: Option<String>,
}

// — HookEvent —
pub struct HookEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub hook_event_name: String,
    pub session_id: String,
    pub cwd: String,
    pub permission_mode: String,
    pub transcript_path: String,
    pub hook_input: serde_json::Value,  // 原始 JSON
    pub environment_variables: HashMap<String, String>,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

// — SseEvent —
pub struct SseEvent {
    pub event_type: Option<String>,
    pub data: Option<String>,
}

// — MCP JSON-RPC 请求（用于解析展示） —
pub struct McpRequest {
    pub jsonrpc: String,
    pub method: String,
    pub params: Option<serde_json::Value>,
    pub id: Option<serde_json::Value>,
}
```

### 4.7 安全措施

```rust
const REDACTED_HEADERS: &[&str] = &["x-api-key", "authorization"];

fn redact_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers.iter()
        .map(|(k, v)| {
            let value = if REDACTED_HEADERS.contains(&k.as_str().to_lowercase().as_str()) {
                "[REDACTED]".to_string()
            } else {
                v.to_str().unwrap_or("[binary]").to_string()
            };
            (k.to_string(), value)
        })
        .collect()
}
```

端口绑定策略和白名单与原始一致 — 仅监听 localhost：

```rust
let addr_8888 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8888);
let addr_9999 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9999);
```

---

### 4.8 会话录制与导出 (`export`)

#### 4.8.1 会话模型

```rust
/// 一次录制会话
pub struct Session {
    pub id: String,
    pub label: Option<String>,        // 用户可选的标签
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub request_ids: Vec<String>,     // 关联的请求 ID 列表
    pub status: SessionStatus,
}

pub enum SessionStatus {
    Recording,
    Stopped,
}
```

#### 4.8.2 三种导出格式

| 格式 | 用途 | 特点 |
|------|------|------|
| **JSON** | 程序化分析、重放 | 完整结构化数据，可重新导入 |
| **HAR** (HTTP Archive) | 标准格式、与 Chrome DevTools 互通 | 兼容 HAR Viewer、Charles Proxy |
| **Markdown** | 人类阅读、分享给同事 | 对话时间线、tool call 展开、token 统计 |

**HAR 导出示例结构**：

```rust
fn build_har(session: &Session, requests: &[ProxiedRequest]) -> Value {
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "cc-proxy", "version": env!("CARGO_PKG_VERSION") },
            "entries": requests.iter().map(|r| {
                json!({
                    "startedDateTime": r.timestamp.to_rfc3339(),
                    "request": {
                        "method": r.method,
                        "url": format!("https://api.anthropic.com{}", r.path),
                        "headers": r.request_headers.iter().map(|(k,v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
                        "postData": { "mimeType": "application/json", "text": r.request_body }
                    },
                    "response": {
                        "status": r.status_code.unwrap_or(0),
                        "headers": r.response_headers.iter().map(|(k,v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
                        "content": { "text": r.response_body, "size": 0 }
                    },
                    "time": r.duration_ms.unwrap_or(0),
                    "timings": { "send": 0, "wait": r.time_to_first_token_ms.unwrap_or(0), "receive": 0 }
                })
            }).collect::<Vec<_>>()
        }
    })
}
```

**Markdown 导出示例结构**：

```markdown
# Session: debug-claude-2026-05-16
Label: debugging tool_use loop fix
Duration: 00:03:22 | Requests: 12 | Total tokens: 45,230

## Request #1 — POST /v1/messages — 200 OK — 2.3s
Model: claude-sonnet-4-6 | Streaming: true | Tokens: 1,200 in / 800 out

### Request Body
```json
{ "model": "claude-sonnet-4-6", "messages": [...] }
```

### Response
> **Assistant**: Let me read the file to understand the issue...
>
> *tool_use: Read(path="/src/main.rs")*
>
> Content block delta: ...

### SSE Events (5 events)
| # | Type | Data |
|---|------|------|
| 1 | message_start | ... |
| 2 | content_block_start | ... |

---
```

#### 4.8.3 Tee Writer（旁路实时抓包）

在代理层增加可选 "T 型分流" — 所有流量在转发的同时复制一份写入磁盘文件。

```rust
pub struct TeeWriter {
    enabled: Arc<AtomicBool>,
    output_dir: PathBuf,
    current_file: Mutex<Option<tokio::fs::File>>,
    format: TeeFormat,
}

pub enum TeeFormat {
    Raw,      // 原始 HTTP 报文（request line + headers + body）
    Jsonl,    // 每行一个 JSON（方便 grep/jq 分析）
    Pcap,     // 伪 pcap 格式（需额外 crate）
}

impl TeeWriter {
    /// 开始新文件，文件名含时间戳
    pub async fn start_new_file(&self) -> Result<()> {
        let path = self.output_dir.join(format!("capture_{}.{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            self.format.extension()
        ));
        let file = tokio::fs::File::create(&path).await?;
        *self.current_file.lock().await = Some(file);
        Ok(())
    }

    /// 写入一条请求/响应对
    pub async fn write_exchange(&self, request: &ProxiedRequest) -> Result<()> {
        if !self.enabled.load(Ordering::Relaxed) { return Ok(()); }
        let mut guard = self.current_file.lock().await;
        if let Some(ref mut file) = *guard {
            match self.format {
                TeeFormat::Raw => write_raw_exchange(file, request).await?,
                TeeFormat::Jsonl => write_jsonl_exchange(file, request).await?,
                TeeFormat::Pcap => write_pcap_exchange(file, request).await?,
            }
        }
        Ok(())
    }
}
```

**Raw 格式示例**（类 tcpdump/wireshark 抓包输出）：

```
============================================================
2026-05-16T14:32:01.234Z POST /v1/messages
Host: api.anthropic.com
Content-Type: application/json
x-api-key: [REDACTED]
anthropic-version: 2023-06-01

{"model":"claude-sonnet-4-6","max_tokens":4096,"stream":true,"messages":[...]}
============================================================
2026-05-16T14:32:03.456Z 200 OK — 2,222ms
Content-Type: text/event-stream

event: message_start
data: {"type":"message_start","message":{"id":"msg_xxx",...}}

event: content_block_start
data: {"type":"content_block_start","content_block":{"type":"text",...}}

event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Let"}}
...
============================================================
```

#### 4.8.4 对现有存储层的扩展

```rust
// AppState 增加字段
pub struct AppState {
    // ... 原有字段
    pub sessions: Arc<RwLock<Vec<Session>>>,       // 会话列表
    pub tee_writer: Arc<TeeWriter>,                 // Tee 旁路写
}

// RingBuffer 不变，Session 引用 request_id
// 查找流程: session → request_ids → request_store.get_by_id(id)
```

**设计决策**：Session 和 RequestStore 通过 `request_id` 做松耦合引用，而非嵌套对象。这样：
- 同一请求可被多个会话引用（不复制数据）
- 录制前/后的请求都可以事后编组到会话中
- 清空 request_store 不会丢失 session 元数据（但内容不可用）

---

## 5. HookAgent 设计

与原始设计一致：独立的轻量 CLI，通过 stdin 读取 Claude Code hook JSON，POST 到仪表盘。

```rust
// crates/proxy-hook-agent/src/main.rs
use clap::Parser;

#[derive(Parser)]
#[command(name = "hook-agent")]
struct Args {
    #[arg(long, default_value = "http://localhost:5000")]
    dashboard_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 从 stdin 读取 hook JSON
    let stdin = io::read_to_string(io::stdin()).await?;
    let hook_input: Value = serde_json::from_str(&stdin).ok();

    // 2. 采集 CLAUDE_* 环境变量
    let env_vars = ["CLAUDE_PROJECT_DIR", "CLAUDE_REMOTE", "CLAUDE_ENV_FILE", "CLAUDE_PLUGIN_DIRS"]
        .iter()
        .filter_map(|&k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();

    // 3. 构建 payload + POST
    let payload = json!({ /* ... */ });
    let client = reqwest::Client::new();
    let resp = client.post(format!("{}/api/hook-event", args.dashboard_url))
        .json(&payload)
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    // 4. 容错：服务器不可用时静默成功，不阻塞 Claude Code
    match resp {
        Ok(r) => {
            let result: Value = r.json().await?;
            print!("{}", result["stdout"].as_str().unwrap_or(""));
            eprint!("{}", result["stderr"].as_str().unwrap_or(""));
            std::process::exit(result["exitCode"].as_i64().unwrap_or(0) as i32);
        }
        Err(_) => std::process::exit(0), // 静默降级
    }
}
```

---

## 6. 与优秀 Rust 项目的设计对齐

| 参考项目 | 借鉴点 | 应用 |
|---------|--------|------|
| **pingora** (Cloudflare) | 代理层错误处理策略、连接池管理 | reqwest client pool 复用、`Error::proxy_error()` 分类 |
| **axum** 官方示例 | `WebSocket` + `broadcast` 模式 | Hub 实现 |
| **miniserve** | 嵌入式静态文件服务 (`rust-embed`) | `wwwroot/` 嵌入二进制 |
| **vector** (Datadog) | 结构化日志 pipeline | tracing span 用于请求追踪 |
| **bore** | 简洁的 CLI + 配置文件设计 | HookAgent 的 clap 参数定义 |

---

## 7. 依赖清单

### proxy-core (`Cargo.toml`)

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
tokio = { version = "1", features = ["sync"] }
tracing = "0.1"
```

### proxy-server (`Cargo.toml`)

```toml
[dependencies]
proxy-core = { path = "../proxy-core" }
axum = { version = "0.8", features = ["ws"] }
axum-server = { version = "0.8", features = ["tls-rustls"] }
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["stream", "rustls-tls"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "fs"] }
rustls = "0.23"
rustls-pemfile = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
async-compression = { version = "0.4", features = ["tokio", "gzip", "brotli", "deflate"] }
rust-embed = "8"       # 嵌入 wwwroot/
figment = "0.10"       # 配置加载
bytes = "1"
http = "1"
futures = "0.3"
uuid = "1"
```

### proxy-hook-agent (`Cargo.toml`)

```toml
[dependencies]
clap = { version = "4", features = ["derive", "env"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

---

## 8. 配置设计

```toml
# config.toml
[proxy]
api_target = "https://api.anthropic.com"
request_store_capacity = 1000
mcp_store_capacity = 500

[server]
http_port = 5000
https_port = 5001
proxy_port = 8888
mcp_proxy_port = 9999
listen_address = "127.0.0.1"

[tls]
cert_path = "certs/cert.pem"
key_path = "certs/key.pem"
# auto_generate = true  # 可选：自动生成自签名证书

[logging]
level = "info"  # trace | debug | info | warn | error
format = "pretty"  # json | pretty | compact
```

---

## 9. 构建与分发

```bash
# 开发
cargo run -p proxy-server

# 构建 release（单二进制，含嵌入前端）
cargo build -p proxy-server --release
# → target/release/proxy-server (~15MB, stripped)
# → target/release/proxy-hook-agent (~5MB, stripped)

# 配置文件
cp config.toml /usr/local/etc/cc-proxy/config.toml

# 运行
./proxy-server --config config.toml
```

### 对比原版的优势

| 指标 | .NET 10 版本 | Rust 版本 |
|------|------------|----------|
| 运行时依赖 | .NET 10 Runtime (~200MB) | 无 |
| 二进制大小 | ~15MB (自包含发布 ~80MB) | ~15MB (stripped) |
| 内存占用 | ~80-120MB (JIT) | ~15-30MB (AOT, 无 GC) |
| 冷启动 | ~2-3s (JIT) | <10ms |
| 跨平台构建 | 需各平台 SDK | `cross build` / CI matrix |
| 并发模型 | ThreadPool + async | Tokio work-stealing |

---

## 10. 实现路线

### 阶段 1: 核心骨架 (Week 1)

- [ ] workspace + `proxy-core` crate (models, store, config)
- [ ] `proxy-server` 主框架 (axum, 多端口, tracing)
- [ ] 静态文件服务 + `index.html` 嵌入

### 阶段 2: 代理核心 (Week 2)

- [ ] Anthropic API 代理 handler（非流式）
- [ ] SSE 流式代理 handler + 解析
- [ ] 响应体解压 (gzip/brotli/deflate)
- [ ] header 脱敏

### 阶段 3: WebSocket 仪表盘 (Week 2-3)

- [ ] Broadcaster + WebSocket hub
- [ ] 历史推送 + 实时推送
- [ ] 清空、MCP 配置端点

### 阶段 4: MCP 代理 + Hook (Week 3)

- [ ] MCP 代理 handler（JSON-RPC 透传）
- [ ] MCP 未配置兜底响应
- [ ] `/api/hook-event` 端点
- [ ] HookAgent CLI

### 阶段 5: TLS + 打磨 (Week 3-4)

- [ ] HTTPS 端口（rustls, 自签名证书自动生成）
- [ ] 配置热重载（MCP target）
- [ ] 会话录制与导出（JSON / HAR / Markdown）
- [ ] Tee Writer（旁路实时抓包 → jsonl / raw 格式）
- [ ] 集成测试 + e2e 测试
- [ ] CI/CD (GitHub Actions, cross-compile matrix)

---

## 11. 架构对比总结

### 原版 (.NET) 架构
```
ASP.NET Core Host (Kestrel)
  ├── YARP Reverse Proxy (IProxyConfigProvider, ITransformProvider)
  ├── SignalR Hub (DashboardHub)
  ├── Singleton Services (RequestStore, HookEventStore, McpRequestStore, McpProxyConfig)
  └── wwwroot/ SPA 前端
HookAgent (独立 .NET CLI)
```

### Rust 重构架构
```
Axum Server (4 sockets)
  ├── Axum Handlers (anthropic_proxy, mcp_proxy)
  ├── WebSocket Hub (broadcast channel)
  ├── AppState (Arc<RwLock<RingBuffer>> × 3, Arc<RwLock<McpTarget>>, broadcast::Sender)
  └── Embedded wwwroot/ (rust-embed)
proxy-hook-agent (独立 Rust CLI → clap + reqwest)
```

### 核心差异

| 维度 | .NET | Rust |
|------|------|------|
| **代理策略** | YARP 中间件管道 | Axum handler（更直接可控） |
| **SSE 处理** | TransformContext.Body 逐行读 | `reqwest::Response::bytes_stream()` + 自定义 SseParser |
| **实时推送** | SignalR Hub（自动重连、组管理） | 原生 WebSocket + `tokio::sync::broadcast`（更轻量） |
| **状态管理** | DI 容器 Singleton | `Arc<RwLock<T>>` 手动共享 |
| **配置热更新** | `CancellationChangeToken` | `tokio::sync::watch` channel |
| **二进制分发** | 需 .NET Runtime 或 self-contained (~80MB) | 静态链接 binary (~15MB) |
| **前端** | 文件系统 `UseStaticFiles` | `rust-embed` 嵌入 (release 时最佳) |
| **TLS** | ASP.NET Core Kestrel | rustls (纯 Rust，跨平台编译友好) |

方案的设计理念是 **保持功能等价但不机械复制**。.NET 的 DI 容器、YARP 管道、SignalR 在 Rust 生态中没有直接等价物，但通过 trait、tower layer、broadcast channel 可以以更显式的方式达成相同目标，且运行成本更低。
