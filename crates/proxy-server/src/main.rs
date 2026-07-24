#![recursion_limit = "256"]

mod web;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use proxy_common::{ConfigStore, EventBus};
use proxy_relay::{CaptureControl, RelayHandler};
use proxy_store::{ProxyStore, ProxyStoreConfig};

pub struct AppState {
    pub config: ConfigStore,
    pub store: ProxyStore,
    pub events: EventBus,
    pub relay: RelayHandler,
    pub capture: CaptureControl,
}

impl AppState {
    pub async fn new(config_path: &str) -> anyhow::Result<Self> {
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

        let store = ProxyStore::open(ProxyStoreConfig {
            database_path: PathBuf::from("data/datav2.db"),
            archive_dir: PathBuf::from("data/archives"),
            busy_timeout_ms: 5000,
        })?;

        let events = EventBus::new(256);

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;

        let capture = CaptureControl::new(PathBuf::from("captures"), events.clone());

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

        Ok(Self {
            config,
            store,
            events,
            relay,
            capture,
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

    // ── Providers table ──
    let (pw, uw) = (20usize, 52usize);
    tracing::info!("{} provider(s):", config.proxy.providers.len());
    tracing::info!("┌{}┬{}┐", "─".repeat(pw), "─".repeat(uw));
    tracing::info!("│{:^pw$}│{:^uw$}│", " provider ", " url ");
    for p in &config.proxy.providers {
        tracing::info!("│ {:<pw1$}│ {:<uw1$} │", p.name, p.url, pw1 = pw - 1, uw1 = uw - 1);
    }
    tracing::info!("└{}┴{}┘", "─".repeat(pw), "─".repeat(uw));

    // ── Upstreams header ──
    tracing::info!("{} upstream(s) — * = active, (effort) = tier override:", config.proxy.upstreams.len());

    // Find the dominant keyword for each tier column
    let modal_kw = |rules: &[Option<&str>]| {
        let kws: Vec<&str> = rules.iter().filter_map(|&r| r).filter(|k| !k.is_empty()).collect();
        if kws.is_empty() { return "—".into(); }
        let first = kws[0];
        if kws.iter().all(|&k| k == first) { first.to_string() } else { "—".into() }
    };
    let h_kws: Vec<Option<&str>> = config.proxy.upstreams.iter().map(|u| u.high.as_ref().and_then(|t| t.keywords.first().map(|s| s.as_str()))).collect();
    let m_kws: Vec<Option<&str>> = config.proxy.upstreams.iter().map(|u| u.mid.as_ref().and_then(|t| t.keywords.first().map(|s| s.as_str()))).collect();
    let l_kws: Vec<Option<&str>> = config.proxy.upstreams.iter().map(|u| u.low.as_ref().and_then(|t| t.keywords.first().map(|s| s.as_str()))).collect();
    let hkw = modal_kw(&h_kws);
    let mkw = modal_kw(&m_kws);
    let lkw = modal_kw(&l_kws);

    let ww = [20, 22, 22, 22, 26];
    let hline = |w: usize| "─".repeat(w);
    let sep = |l: &str, m: &str, r: &str| {
        tracing::info!("{l}{0}{m}{1}{m}{2}{m}{3}{m}{4}{r}",
            hline(ww[0]), hline(ww[1]), hline(ww[2]), hline(ww[3]), hline(ww[4]));
    };
    let row = |cells: [&str; 5]| {
        let trunc = |s: &str, w: usize| {
            if s.len() > w - 3 { format!("{}…", &s[..w - 4]) } else { s.to_string() }
        };
        tracing::info!(
            "│{:^w0$}│{:^w1$}│{:^w2$}│{:^w3$}│{:^w4$}│",
            trunc(cells[0], ww[0]),
            trunc(cells[1], ww[1]),
            trunc(cells[2], ww[2]),
            trunc(cells[3], ww[3]),
            trunc(cells[4], ww[4]),
            w0 = ww[0], w1 = ww[1], w2 = ww[2], w3 = ww[3], w4 = ww[4],
        );
    };

    sep("┌", "┬", "┐");
    row(["upstream", &format!("high ({hkw})"), &format!("mid ({mkw})"), &format!("low ({lkw})"), "default"]);
    sep("├", "┼", "┤");
    for u in &config.proxy.upstreams {
        let dp = u.default.as_ref().map(|d| d.provider.as_str()).unwrap_or("");
        let cell = |t: Option<&proxy_common::TierRule>| -> String {
            match t {
                Some(r) if !r.keywords.is_empty() => {
                    if r.provider == dp || r.provider.is_empty() {
                        r.model.clone()
                    } else {
                        format!("{}/{}", r.provider, r.model)
                    }
                }
                _ => "—".into(),
            }
        };
        let star = if u.name == config.proxy.active_upstream { "*" } else { " " };
        let effort_note = u.effort.as_deref().unwrap_or("");
        let name_cell = if effort_note.is_empty() || effort_note == "auto" {
            format!("{}{}", u.name, star)
        } else {
            format!("{}{} ({})", u.name, star, effort_note)
        };
        row([
            &name_cell,
            &cell(u.high.as_ref()),
            &cell(u.mid.as_ref()),
            &cell(u.low.as_ref()),
            &u.default.as_ref()
                .map(|d| format!("{}/{}", d.provider, d.model))
                .unwrap_or_else(|| "—".into()),
        ]);
    }
    sep("└", "┴", "┘");

    // ── Listen ──
    let is_loopback = config.server.listen_address == "127.0.0.1"
        || config.server.listen_address == "::1"
        || config.server.listen_address == "localhost";
    if !is_loopback {
        if config.server.auth_token.as_deref().unwrap_or("").is_empty() {
            anyhow::bail!(
                "server.auth_token is required when listen_address '{}' is not loopback",
                config.server.listen_address
            );
        }
        tracing::info!("[server] listen_address={} with auth_token", config.server.listen_address);
    }

    let http_router = web::build_router(state.clone());
    let http_addr = SocketAddr::new(config.server.listen_address.parse()?, config.server.http_port);
    let proxy_router = state.relay.clone().build_router();
    let proxy_addr = SocketAddr::new(config.server.listen_address.parse()?, config.server.proxy_port);

    tracing::info!("Dashboard: http://{http_addr}");
    tracing::info!("Anthropic proxy: http://{proxy_addr}");

    let http_listener = TcpListener::bind(http_addr).await?;
    let proxy_listener = TcpListener::bind(proxy_addr).await?;

    tokio::try_join!(
        axum::serve(http_listener, http_router),
        axum::serve(proxy_listener, proxy_router),
    )?;

    Ok(())
}
