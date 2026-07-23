mod web;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use proxy_common::{ConfigStore, EventBus};
use proxy_relay::{CaptureControl, HookReceiver, McpRelay, RelayHandler};
use proxy_store::{ProxyStore, ProxyStoreConfig};

pub struct AppState {
    pub config: ConfigStore,
    pub store: ProxyStore,
    pub events: EventBus,
    pub relay: RelayHandler,
    pub mcp: McpRelay,
    pub capture: CaptureControl,
    pub hook_receiver: HookReceiver,
}

impl AppState {
    pub async fn new(config_path: &str) -> anyhow::Result<Self> {
        // ── Config ──
        let config = ConfigStore::open(config_path).await?;
        if config
            .get()
            .await
            .server
            .auth_token
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            let generated = format!(
                "{}{}",
                proxy_common::TaskId::generate(),
                proxy_common::TaskId::generate()
            );
            config
                .update(move |candidate| {
                    candidate.server.auth_token = Some(generated);
                    Ok(())
                })
                .await?;
        }
        let config_snapshot = config.get().await;

        // ── Store ──
        let store = ProxyStore::open(ProxyStoreConfig {
            database_path: PathBuf::from("data/datav2.db"),
            archive_dir: PathBuf::from("data/archives"),
            busy_timeout_ms: 5000,
        })?;

        // ── Events ──
        let events = EventBus::new(256);

        // ── HTTP client ──
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;

        // ── Capture ──
        let capture = CaptureControl::new(PathBuf::from("captures"), events.clone());

        // ── Proxy relay ──
        let relay = RelayHandler::new(
            config.clone(),
            store.clone(),
            events.clone(),
            client.clone(),
            capture.clone(),
        )
        .with_retry_config(
            config_snapshot.proxy.retry_count,
            config_snapshot.proxy.request_timeout_secs,
        );

        // ── MCP relay ──
        let mcp = McpRelay::new(store.clone(), events.clone(), client);
        if let Some(ref dest) = config_snapshot.server.mcp_destination {
            if !dest.is_empty() {
                mcp.set_destination(Some(dest.clone())).await;
            }
        }

        // ── Hook receiver ──
        let hook_receiver = HookReceiver::new(events.clone());

        Ok(Self {
            config,
            store,
            events,
            relay,
            mcp,
            capture,
            hook_receiver,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let state = Arc::new(AppState::new(&config_path).await?);
    let config = state.config.get().await;

    // ── Startup logging ──
    let provider_count = config.proxy.providers.len();
    tracing::info!("{} provider(s) configured", provider_count);
    for p in &config.proxy.providers {
        tracing::info!("  provider '{}' → {}", p.name, p.url);
    }

    let upstream_count = config.proxy.upstreams.len();
    tracing::info!(
        "{} upstream(s) configured, active = '{}'",
        upstream_count,
        config.proxy.active_upstream,
    );
    tracing::info!("Effort level = '{}'", config.proxy.active_effort);

    // Refuse an externally reachable control plane without authentication.
    let is_loopback = config.server.listen_address == "127.0.0.1"
        || config.server.listen_address == "::1"
        || config.server.listen_address == "localhost";
    if !is_loopback {
        if config.server.auth_token.as_deref().unwrap_or("").is_empty() {
            anyhow::bail!(
                "server.auth_token is required when listen_address '{}' is not loopback",
                config.server.listen_address
            );
        } else {
            tracing::info!(
                "[server] listen_address={} with auth_token configured",
                config.server.listen_address,
            );
        }
    }

    for u in &config.proxy.upstreams {
        let high = u
            .high
            .as_ref()
            .map(|r| {
                format!(
                    "{}→{}/{}",
                    r.keywords.first().unwrap_or(&"-".into()),
                    r.provider,
                    r.model
                )
            })
            .unwrap_or_else(|| "-".into());
        let mid = u
            .mid
            .as_ref()
            .map(|r| {
                format!(
                    "{}→{}/{}",
                    r.keywords.first().unwrap_or(&"-".into()),
                    r.provider,
                    r.model
                )
            })
            .unwrap_or_else(|| "-".into());
        let low = u
            .low
            .as_ref()
            .map(|r| {
                format!(
                    "{}→{}/{}",
                    r.keywords.first().unwrap_or(&"-".into()),
                    r.provider,
                    r.model
                )
            })
            .unwrap_or_else(|| "-".into());
        let default = u
            .default
            .as_ref()
            .map(|r| format!("{}/{}", r.provider, r.model))
            .unwrap_or_else(|| "-".into());
        tracing::info!(
            "  upstream '{}' H:[{}] M:[{}] L:[{}] default→{}",
            u.name,
            high,
            mid,
            low,
            default,
        );
    }

    // ── HTTP router (dashboard + REST API + WebSocket) ──
    let http_router = web::build_router(state.clone());
    let http_addr = SocketAddr::new(
        config.server.listen_address.parse()?,
        config.server.http_port,
    );

    // ── Proxy router (Anthropic API proxy :8888) ──
    let proxy_router = state.relay.clone().build_router();
    let proxy_addr = SocketAddr::new(
        config.server.listen_address.parse()?,
        config.server.proxy_port,
    );

    // ── MCP router (:9999) ──
    let mcp_router = state.mcp.clone().build_router();
    let mcp_addr = SocketAddr::new(
        config.server.listen_address.parse()?,
        config.server.mcp_proxy_port,
    );

    tracing::info!(
        "Dashboard: http://{}:{}",
        config.server.listen_address,
        config.server.http_port
    );
    tracing::info!(
        "Anthropic proxy: http://{}:{}",
        config.server.listen_address,
        config.server.proxy_port
    );
    tracing::info!(
        "MCP proxy: http://{}:{}",
        config.server.listen_address,
        config.server.mcp_proxy_port
    );

    let http_listener = TcpListener::bind(http_addr).await?;
    let proxy_listener = TcpListener::bind(proxy_addr).await?;
    let mcp_listener = TcpListener::bind(mcp_addr).await?;

    tokio::try_join!(
        axum::serve(http_listener, http_router),
        axum::serve(proxy_listener, proxy_router),
        axum::serve(mcp_listener, mcp_router),
    )?;

    Ok(())
}
