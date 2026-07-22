# Archive 搜索增强 — 多关键字 & `user:` 角色过滤

## 背景

当前 archive 搜索 (`/api/archive/search`) 是对 YAML 文件做逐行子字符串匹配，功能单一：

| 现状 | 缺失 |
|------|------|
| 单关键字搜索 | 多关键字 AND 组合 |
| 逐行文本匹配（无结构感知） | 引号括起来的精确短语 |
| 搜索所有内容（含工具调用、系统提醒、AI 回复） | 按角色过滤（只看用户发送的内容） |

用户希望：
- **空格分隔 = 多关键字 AND**：`hello world` → 同时包含 "hello" 和 "world"
- **双引号 = 精确短语**：`"hello world"` → 精确匹配该短语
- **`user:关键字` 前缀**：仅搜索用户真实 prompt，排除 Claude Code 自动注入的系统内容

---

## 现有消息角色判断

### 三类 "user" 角色消息

`proxy-core/src/summary.rs` 中已有 `is_real_user_prompt()` 和 `extract_user_text()`：

| 消息类型 | `role` | 内容特征 | 判定/提取函数 |
|---------|--------|---------|-------------|
| **真实用户 prompt** | `user` | 全部为 `text` block，过滤 `<system-reminder>` 后非空 | `is_real_user_prompt()` / `extract_user_text()` |
| **工具执行结果** | `user` | 包含 `tool_result` block | `is_tool_result()` |
| **系统提醒** | `user` | text 以 `<system-reminder>` 开头 | 在 `is_real_user_prompt()` 中过滤 |

### 判断逻辑

```rust
pub fn is_real_user_prompt(msg: &Value) -> bool {
    // 1. content 必须是数组
    // 2. 所有 block type == "text"（排除 tool_result）
    // 3. 过滤 <system-reminder> 开头的 block
    // 4. 剩余文本非空 → true
}

pub fn extract_user_text(msg: &Value) -> String {
    // 提取 type=text 的块文本，过滤 <system-reminder>，拼接返回
}
```

**结论**：`is_real_user_prompt()` 能正确区分"用户发送的"和"Claude Code 自动产生的 prompt"；`extract_user_text()` 已处理 system-reminder 过滤，复用它来提取用户消息文本。

### 当前可见性

这两个函数目前是 `fn`（private），仅 `summary.rs` 内部使用。需要改为 `pub fn` 以供 `api.rs` 的 archive 搜索调用。

---

## 事件流

### 用户交互 → 数据流

```
┌─ archive.js ─────────────────────────────────────────────────────┐
│                                                                    │
│  #archive-search-input                                             │
│    │ input event (debounce 300ms)                                  │
│    ▼                                                               │
│  runArchiveSearch(q)                                               │
│    │ q 原样传给后端，前端不解析                                       │
│    │                                                               │
│    │ GET /api/archive/search?q=<encoded>                           │
│    ▼                                                               │
│                                                                     │
└──────────────────────┬──────────────────────────────────────────────┘
                       │ HTTP GET
                       ▼
┌─ api.rs ───────────────────────────────────────────────────────────┐
│                                                                     │
│  archive_search(Query(params))                                      │
│    │                                                                │
│    │ parse_archive_query(q)  ← [新增] 后端解析                      │
│    │   ├─ 剥离 user: 前缀 → role_filter = Some("user")              │
│    │   ├─ 按空格分词                                                │
│    │   └─ 双引号内容保持为一个 keyword                               │
│    │   输出: ParsedArchiveQuery { keywords: Vec<String>,            │
│    │                              role_filter: Option<String> }      │
│    │                                                                │
│    │ for each sessions/*.yaml:                                      │
│    │   ├─ read_yaml_meta() → name, last_active_at                  │
│    │   ├─ serde_yaml::from_str(&content) → Value  ← [重构] 结构解析 │
│    │   │                                                            │
│    │   │ for each request in requests[]:                           │
│    │   │   for each msg in request_body.messages[]:                │
│    │   │     │                                                     │
│    │   │     ├─ match role_filter:                                 │
│    │   │     │   Some("user") → is_real_user_prompt(msg) (含role检查)│
│    │   │     │   None         → 所有 role 都搜索                    │
│    │   │     │                                                     │
│    │   │     ├─ extract_message_text(msg) → String  ← [新增]       │
│    │   │     │   user:      调用 extract_user_text()（已有，过滤SR） │
│    │   │     │   assistant: 提取 text + thinking blocks            │
│    │   │     │   其他:      返回空字符串                              │
│    │   │     │                                                     │
│    │   │     └─ all keywords match? (case-insensitive AND)         │
│    │   │         YES → collect snippet { role, text, req_idx }     │
│    │   │         NO  → continue                                    │
│    │   │                                                           │
│    │   │ snippets 达到 5 条 → 停止遍历该文件                         │
│    │   └─ snippets.len() > 0 → push to results                     │
│    │                                                                │
│    │ return JSON [...]                                              │
│    ▼                                                               │
│                                                                     │
└──────────────────────┬──────────────────────────────────────────────┘
                       │ HTTP Response (JSON)
                       ▼
┌─ archive.js ───────────────────────────────────────────────────────┐
│                                                                     │
│  renderArchiveSearch(results, q)  ← [重构]                          │
│    │                                                                │
│    │ results[].keywords  ← [新增] 后端返回 keyword 列表              │
│    │ results[].role_filter ← [新增] 当前过滤模式                     │
│    │ results[].snippets[].role  ← [新增] 消息角色                   │
│    │                                                                │
│    │ 高亮渲染:                                                      │
│    │   for each keyword in keywords:                                │
│    │     escHtml(s.text).replace(keyword_re, <mark>)                │
│    │                                                                │
│    │ role 标签:                                                     │
│    │   显示 "user" / "assistant" 角色标签                           │
│    │                                                                │
│    ▼                                                               │
│  DOM: .archive-card > .archive-snippet (.snippet-role + .snippet-text)│
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 通信时序

```
User                archive.js           api.rs              sessions/*.yaml
 │                      │                    │                      │
 │  输入 "user:hello"    │                    │                      │
 │─────────────────────►│                    │                      │
 │                      │                    │                      │
 │                      │  GET /api/archive/ │                      │
 │                      │  search?q=user:hello                      │
 │                      │───────────────────►│                      │
 │                      │                    │                      │
 │                      │                    │  read_yaml_meta()    │
 │                      │                    │─────────────────────►│
 │                      │                    │◄─────────────────────│
 │                      │                    │                      │
 │                      │                    │  serde_yaml::from_str│
 │                      │                    │  → parse struct      │
 │                      │                    │  → iterate messages  │
 │                      │                    │  → role filter       │
 │                      │                    │  → keyword match     │
 │                      │                    │                      │
 │                      │◄───────────────────│                      │
 │                      │  [{ file, snippets:[{role,text}],         │
 │                      │     keywords:["hello"], match_count }]    │
 │                      │                    │                      │
 │                      │  renderArchiveSearch                      │
 │                      │  → 高亮 "hello"                           │
 │                      │  → 显示 role 标签                          │
 │                      │                    │                      │
 │  DOM 更新             │                    │                      │
 │◄─────────────────────│                    │                      │
 │                      │                    │                      │
```

### 状态变量（前端）

```
state.archiveSearchQuery    // 当前搜索词（原始，直接传后端）
     .archiveSearchResults  // API 返回结果数组
     .archiveFiles          // 全量文件列表（无搜索时显示）
     .archiveSearchTimer    // debounce timer id
```

---

## 查询语法

### 解析规则

```
输入                             →  role_filter        keywords
─────────────────────────────────────────────────────────────────
hello                            →  None              ["hello"]
hello world                      →  None              ["hello", "world"]
"hello world" foo                →  None              ["hello world", "foo"]
user:hello                       →  Some("user")      ["hello"]
user:hello world "foo bar"       →  Some("user")      ["hello", "world", "foo bar"]
user:"hello world"               →  Some("user")      ["hello world"]
user:                            →  Some("user")      []   → 匹配所有用户消息
```

规则：
- `user:` 前缀仅放在**查询开头**，影响整个查询范围
- 空格分隔多个 keyword → AND 逻辑
- 双引号 `"..."` 内的内容视为一个整体 keyword（空格不切开）
- 大小写不敏感
- `keywords` 为空时匹配所有消息（空 AND 集合 = 全匹配）

### 查询解析函数

```rust
struct ParsedArchiveQuery {
    keywords: Vec<String>,       // AND logic; empty = match all
    role_filter: Option<String>, // None | Some("user")
}

fn parse_archive_query(q: &str) -> ParsedArchiveQuery {
    let q = q.trim();
    // 1. detect user: prefix
    let (role_filter, rest) = match q.strip_prefix("user:") {
        Some(rest) => (Some("user".to_string()), rest),
        None => (None, q),
    };
    // 2. split by whitespace, respect double-quoted phrases; empty tokens filtered
    let keywords = split_query_tokens(rest);
    ParsedArchiveQuery { keywords, role_filter }
}

fn message_matches_all_keywords(text: &str, keywords: &[String]) -> bool {
    // Empty keyword list = match all (user: with no terms)
    keywords.iter().all(|kw| text.to_lowercase().contains(kw.as_str()))
}
```

---

## 消息文本提取

### `extract_message_text()`

复用 `proxy-core` 已有的 `extract_user_text()`，仅为 assistant 分支新增逻辑：

```rust
/// Extract searchable text from a message.
/// For user role: delegates to proxy_core::summary::extract_user_text()
///   which filters <system-reminder> blocks automatically.
/// For assistant role: extracts text + thinking blocks.
fn extract_message_text(msg: &Value) -> String {
    match msg["role"].as_str().unwrap_or("") {
        "user" => proxy_core::summary::extract_user_text(msg),
        "assistant" => {
            let content = match msg["content"].as_array() {
                Some(c) => c,
                None => return msg["content"].as_str().unwrap_or("").to_string(),
            };
            content.iter()
                .filter_map(|b| {
                    if b["type"] == "text" { b["text"].as_str() }
                    else if b["type"] == "thinking" { b["thinking"].as_str() }
                    else { None }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}
```

### 无 `user:` 过滤时的搜索范围

| 消息类型 | 是否参与搜索 | 说明 |
|---------|------------|------|
| 真实用户 prompt | ✅ | `extract_user_text()` 提取，过滤 system-reminder |
| tool_result（工具输出） | ✅ | `extract_user_text()` 返回空（无 text block），不会命中 |
| 系统提醒 | ❌ | `extract_user_text()` 过滤掉 |
| assistant 文本/思考 | ✅ | `extract_message_text()` assistant 分支提取 |

---

## API 响应变更

### 现有响应格式

```json
[
  {
    "file": "abc123.yaml",
    "name": "my-session",
    "last_active_at": "2026-07-12T10:00:00+00:00",
    "match_count": 3,
    "snippets": [
      { "line": 42, "text": "some matching line" }
    ]
  }
]
```

### 新响应格式

```json
[
  {
    "file": "abc123.yaml",
    "name": "my-session",
    "last_active_at": "2026-07-12T10:00:00+00:00",
    "match_count": 3,
    "keywords": ["hello", "world"],
    "role_filter": "user",
    "snippets": [
      {
        "role": "user",
        "text": "hello world, how are you?",
        "request_index": 0,
        "message_index": 1
      }
    ]
  }
]
```

变更说明：
- `snippets[].line`（行号）→ 移除；`snippets[].text`（行文本）→ 改为完整消息文本
- `snippets[]` 新增 `role`、`request_index`、`message_index`
- 顶层新增 `keywords`（供前端多关键字高亮）和 `role_filter`（供前端显示过滤标签）
- **前端 `renderArchiveSearch` 需同步修改**：`r.snippets` 的字段访问从 `.line` 改为 `.role` + `.request_index`

---

## 前端改动

### `renderArchiveSearch()` 多关键字高亮

签名由 `renderArchiveSearch(results, q)` 保持不变，内部改为从后端返回的 `keywords` 字段驱动高亮，不再前端解析 `q`：

```javascript
function renderArchiveSearch(results, q) {
    // ...
    // keywords come from backend; fall back to [q] only if absent (compat)
    const keywords = results[0]?.keywords?.length ? results[0].keywords : [q];
    const snippetsHtml = r.snippets.map(s => {
        let hi = escHtml(s.text);
        keywords.forEach(kw => {
            const re = new RegExp(kw.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
            hi = hi.replace(re, m => `<mark>${m}</mark>`);
        });
        const roleTag = s.role
            ? `<span class="snippet-role snippet-role--${s.role}">${s.role}</span>`
            : '';
        return `<div class="archive-snippet">${roleTag}${hi}</div>`;
    }).join('');
    // ...
}
```

### 搜索状态指示

当响应顶层 `role_filter === "user"` 时，在搜索框旁显示小标签 "仅用户消息"，方便用户感知当前过滤状态。

### 搜索框 placeholder 更新

```
"搜索会话 (空格=AND, \"短语\", user:=仅用户消息)"
```

---

## 实现步骤

### Step 1: proxy-core — 公开函数

**文件**：`crates/proxy-core/src/summary.rs`

- `is_real_user_prompt()` → `pub fn is_real_user_prompt()`
- `extract_user_text()` → `pub fn extract_user_text()`

### Step 2: proxy-server — 显式声明依赖

**文件**：`crates/proxy-server/Cargo.toml`

```toml
serde_yaml = "0.9"
```

> `serde_yaml` 已是 `proxy-core` 的直接依赖，`proxy-server` 已通过传递依赖获得，此处仅做显式声明。

### Step 3: proxy-server — 新增查询解析辅助函数

**文件**：`crates/proxy-server/src/api.rs`

新增函数：
- `parse_archive_query(q: &str) -> ParsedArchiveQuery` — 查询解析
- `split_query_tokens(s: &str) -> Vec<String>` — 分词（含引号处理，过滤空 token）
- `extract_message_text(msg: &Value) -> String` — 消息文本提取（user 分支复用 `extract_user_text`）
- `message_matches_all_keywords(text: &str, keywords: &[String]) -> bool` — keyword AND 匹配（空集返回 true）

### Step 4: proxy-server — 重写 `archive_search()`

**文件**：`crates/proxy-server/src/api.rs`（替换第 1078–1113 行）

核心逻辑变更：
```
逐行字符串匹配 → serde_yaml 结构解析 → 遍历 messages → role 过滤 → keyword AND 匹配
```

### Step 5: 前端 — 多关键字高亮 & 角色标签

**文件**：`wwwroot/js/archive.js`

- `renderArchiveSearch(results, q)`：从后端 `keywords` 字段驱动高亮，添加 role 标签渲染
- `runArchiveSearch(q)`：无需前端解析，q 原样传后端；从响应 `role_filter` 字段驱动 UI 状态标签

### Step 6: i18n

**文件**：`wwwroot/assets/zh.json`

在 `archive` 节点新增 key：
```json
{
    "search_placeholder": "搜索会话 (空格=AND, \"短语\", user:=仅用户消息)",
    "role_user": "用户",
    "role_assistant": "AI",
    "filter_user_only": "仅用户消息"
}
```

---

## 边界情况

| 场景 | 行为 |
|------|------|
| 空查询 | 返回全量文件列表（现有行为不变） |
| 查询仅为 `user:` | `keywords=[]`，匹配所有用户消息（空 AND = 全匹配） |
| keyword 为空字符串（多余空格） | `split_query_tokens` 过滤空 token |
| YAML 解析失败 | 跳过该文件，不影响其他文件的搜索 |
| 某条 message 无 `content` | `extract_message_text()` 返回空字符串，不匹配 |
| keyword 含正则特殊字符 | 后端做 substring match（不用 regex），无注入风险 |
| `user:` 在查询中间（如 `hello user:world`） | 不被识别为 role 过滤，"user:" 作为普通文本搜索 |
| snippets 上限 | 每个文件最多 5 条，跨所有 request/message 累计，达到即停止遍历该文件 |

---

## 关键文件

| 文件 | 改动 |
|------|------|
| `crates/proxy-core/src/summary.rs` | `pub` 两个函数 |
| `crates/proxy-server/Cargo.toml` | 显式声明 `serde_yaml` 依赖 |
| `crates/proxy-server/src/api.rs` | 重写 `archive_search()`，新增 4 个辅助函数 |
| `wwwroot/js/archive.js` | 多关键字高亮（后端 keywords 驱动）、role 标签、`user:` UI 状态 |
| `wwwroot/assets/zh.json` | 新增 i18n key |
