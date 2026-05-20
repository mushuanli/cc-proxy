# 构建、运行 & 安全

## 构建

```bash
cargo build -p proxy-server --release     # 编译主服务
cargo build -p proxy-hook-agent --release # 编译 Hook CLI
```

## 运行

```bash
./target/release/proxy-server config.toml  # 启动（默认端口 5000/8888/9999）
```

### 端口配置

通过 `config.toml` 的 `[server]` 段覆盖：

```toml
[server]
listen_address = "127.0.0.1"
http_port = 5000
proxy_port = 8888
mcp_proxy_port = 9999
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
