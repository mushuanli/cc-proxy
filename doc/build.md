# 构建、运行 & 安全

## 构建

```bash
cargo build -p proxy-server --release     # 编译主服务
cargo build -p proxy-hook-agent --release # 编译 Hook CLI
```

## 运行

```bash
./target/release/cc-proxy config.toml       # 前台启动（默认端口 5000/8888/9999）
./target/release/cc-proxy -d config.toml    # 后台守护（日志 data/cc-proxy.log，PID data/cc-proxy.pid）
```

后台模式在解析参数后、创建 tokio runtime 之前双 fork，因此不改变工作目录——`config.toml`、`data/`、`captures/` 仍相对启动目录解析。

### 静态编译（跨 glibc 版本部署）

在较新的系统上编译、拿到旧系统运行时，会报 `GLIBC_2.xx not found`。改用 musl 目标产出全静态二进制即可：

```bash
cargo build -p proxy-server --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/cc-proxy   # 应显示 statically linked
```

### 端口配置

通过 `config.toml` 的 `[server]` 段覆盖：

```toml
[server]
listen_address = "127.0.0.1"   # 或 0.0.0.0 / :: 监听所有网卡（需 auth_token）
http_port = 5000
proxy_port = 8888
```

### 配置 Claude Code 拦截

```bash
export ANTHROPIC_BASE_URL="http://localhost:8888"
```

## 安全

- 所有端口仅监听 `127.0.0.1`
- `x-api-key`、`Authorization` header 自动脱敏为 `[REDACTED]`
- API token 存储于 TOML 文件，前端 `has_token: bool`（不暴露 token 内容）
- SQLite 本地存储，不对外暴露
