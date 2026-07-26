#![recursion_limit = "256"]

mod web;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::fmt::FormatEvent;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::layer::Layer as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
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

// ── Custom log format: HH:MM:SS.mmm [I] module: message ──

struct CompactFormat;

impl<S, N> FormatEvent<S, N> for CompactFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        // Timestamp
        let now = chrono::Local::now();
        write!(writer, "{} ", now.format("%H:%M:%S%.3f"))?;
        // Level
        let meta = event.metadata();
        let level = match *meta.level() {
            tracing::Level::ERROR => 'E',
            tracing::Level::WARN => 'W',
            tracing::Level::INFO => 'I',
            tracing::Level::DEBUG => 'D',
            tracing::Level::TRACE => 'T',
        };
        let target = meta.target().trim_start_matches("proxy_");
        write!(writer, "[{level}] {target}: ")?;
        // Fields (message)
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(CompactFormat)
                .with_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info")),
                ),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    let state = Arc::new(AppState::new(&config_path).await?);
    let config = state.config.get().await;

    // ── Recover interrupted tasks from previous run ──
    let process_start_ms = chrono::Utc::now().timestamp_millis();
    let _ = state
        .store
        .recover_interrupted_tasks(process_start_ms)
        .await;

    // ── Providers table ──
    let (pw, uw) = (20usize, 52usize);
    tracing::info!("{} provider(s):", config.proxy.providers.len());
    tracing::info!("┌{}┬{}┐", "─".repeat(pw), "─".repeat(uw));
    tracing::info!("│{:^pw$}│{:^uw$}│", " provider ", " url ");
    for p in &config.proxy.providers {
        tracing::info!(
            "│ {:<pw1$}│ {:<uw1$} │",
            p.name,
            p.url,
            pw1 = pw - 1,
            uw1 = uw - 1
        );
    }
    tracing::info!("└{}┴{}┘", "─".repeat(pw), "─".repeat(uw));

    // ── Upstreams: validate + table ──
    for u in &config.proxy.upstreams {
        if u.default.is_none() {
            anyhow::bail!("upstream '{}' is missing a default tier", u.name);
        }
    }
    tracing::info!(
        "{} upstream(s) — * = active, (effort) = tier override:",
        config.proxy.upstreams.len()
    );

    let ww = [20, 20, 20, 20, 32];
    let hline = |w: usize| "─".repeat(w);
    let sep = |l: &str, m: &str, r: &str| {
        tracing::info!(
            "{l}{0}{m}{1}{m}{2}{m}{3}{m}{4}{r}",
            hline(ww[0]),
            hline(ww[1]),
            hline(ww[2]),
            hline(ww[3]),
            hline(ww[4])
        );
    };
    let row = |cells: [&str; 5]| {
        let trunc = |s: &str, w: usize| {
            if s.len() > w - 3 {
                format!("{}…", &s[..w - 4])
            } else {
                s.to_string()
            }
        };
        tracing::info!(
            "│{:^w0$}│{:^w1$}│{:^w2$}│{:^w3$}│{:^w4$}│",
            trunc(cells[0], ww[0]),
            trunc(cells[1], ww[1]),
            trunc(cells[2], ww[2]),
            trunc(cells[3], ww[3]),
            trunc(cells[4], ww[4]),
            w0 = ww[0],
            w1 = ww[1],
            w2 = ww[2],
            w3 = ww[3],
            w4 = ww[4],
        );
    };

    sep("┌", "┬", "┐");
    row(["upstream", "Opus", "Sonnet", "Haiku", "default"]);
    sep("├", "┼", "┤");
    for u in &config.proxy.upstreams {
        let def = u.default.as_ref();
        let cell = |t: Option<&proxy_common::TierRule>| -> String {
            match t {
                Some(r) if r.is_active() => {
                    let dp = r.provider_or(def);
                    match def {
                        Some(d) if r.provider == d.provider && r.model == d.model => "—".into(),
                        Some(d) if r.provider.is_empty() || r.provider == d.provider => r.model.clone(),
                        _ => format!("{}/{}", dp, r.model),
                    }
                }
                _ => "—".into(),
            }
        };
        let star = if u.name == config.proxy.active_upstream {
            "*"
        } else {
            " "
        };
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
            &u.default
                .as_ref()
                .map(|d| {
                    if d.model.is_empty() {
                        format!("{} (passthrough)", d.provider)
                    } else {
                        format!("{}/{}", d.provider, d.model)
                    }
                })
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
        tracing::info!(
            "[server] listen_address={} with auth_token",
            config.server.listen_address
        );
    }

    let http_router = web::build_router(state.clone());
    let http_addr = SocketAddr::new(
        config.server.listen_address.parse()?,
        config.server.http_port,
    );
    let proxy_router = state.relay.clone().build_router();
    let proxy_addr = SocketAddr::new(
        config.server.listen_address.parse()?,
        config.server.proxy_port,
    );

    tracing::info!("Dashboard: http://{http_addr}");
    tracing::info!("API relay: http://{proxy_addr}");

    let http_listener = TcpListener::bind(http_addr).await?;
    let proxy_listener = TcpListener::bind(proxy_addr).await?;

    tokio::try_join!(
        axum::serve(http_listener, http_router),
        axum::serve(proxy_listener, proxy_router),
    )?;

    Ok(())
}
