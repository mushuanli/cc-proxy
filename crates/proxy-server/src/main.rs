#![recursion_limit = "256"]

mod watcher;
mod web;
mod ws;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
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

/// How often the planx registry refreshes credentials that are about to expire.
/// An upstream 401 is handled immediately by the relay's in-request retry, so
/// this is a safety net rather than the primary refresh path.
const PLANX_REFRESH_INTERVAL_SECS: u64 = 300;

pub struct AppState {
    pub config: ConfigStore,
    pub store: ProxyStore,
    pub events: EventBus,
    pub relay: RelayHandler,
    pub capture: CaptureControl,
    pub session: std::sync::Arc<proxy_session::SessionRepo>,
    pub summary_jobs: Arc<SummaryJobs>,
    /// Live planx account registry. Always present; it may hold zero accounts,
    /// which is what allows the first account to be added without a restart.
    pub planx: Arc<proxy_planx::PlanxRegistry>,
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
                .await
                // Without context this surfaces as a bare `io error: Permission
                // denied`, which says nothing about *what* could not be written.
                .map_err(|error| {
                    anyhow::anyhow!(
                        "cannot persist the generated auth_token to '{config_path}': {error}.\n\
                         The working directory must be writable on first start: the token, \
                         data/datav2.db and captures/ are created there.\n\
                         Set server.auth_token in the config yourself to skip this write."
                    )
                })?;
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

        // Session timeline collector (independent connection, own tables).
        let session_repo = std::sync::Arc::new(
            proxy_session::SessionRepo::open(proxy_session::SessionRepoConfig {
                database_path: PathBuf::from("data/datav2.db"),
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("failed to open session repo: {e}"))?,
        );
        let session_ingest: std::sync::Arc<dyn proxy_session::SessionIngest> = session_repo.clone();

        // planx: subscription accounts as upstream credentials.
        //
        // The registry is built even with zero accounts. A registry that only
        // exists when the startup config already had a working account could
        // never learn about the *first* account added later — the dashboard CRUD
        // path and the config watcher both converge on `PlanxRegistry::reload`,
        // which needs a live registry to reload into.
        //
        // Injecting an empty registry is behaviour-neutral: a provider without an
        // `account` never consults it.
        let planx = std::sync::Arc::new(
            proxy_planx::PlanxRegistry::from_config(
                &config_snapshot.proxy.accounts,
                config_snapshot.proxy.http_proxy.as_deref(),
                proxy_planx::ProbeUrls::default(),
            )
            .await?,
        );
        proxy_planx::spawn_refresher(planx.clone(), PLANX_REFRESH_INTERVAL_SECS);
        tracing::info!("[planx] {} account(s) active", planx.len());
        let account_auth: proxy_common::PlanAuthHandle =
            Some(planx.clone() as std::sync::Arc<dyn proxy_common::PlanAuthProvider>);

        let relay = RelayHandler::new(
            config.clone(),
            store.clone(),
            events.clone(),
            client.clone(),
            capture.clone(),
        )
        .with_session_ingest(session_ingest)
        .with_account_auth(account_auth)
        .with_protocol_adapter(Some(std::sync::Arc::new(
            proxy_bridge::AnthropicCodexAdapter::new(),
        )))
        .with_retry_config(
            config_snapshot.proxy.retry_count,
            config_snapshot.proxy.request_timeout_secs,
        );

        // Hot-reload watcher. Two triggers converge on the same reload path:
        //   1. an API edit (the dashboard) emits `UpstreamChanged` on the bus;
        //   2. a hand-edit of config.toml changes the file mtime.
        // Without (2) a manual edit would silently require a restart.
        watcher::spawn(
            config.clone(),
            events.clone(),
            planx.clone(),
            std::path::PathBuf::from(config_path),
        );

        Ok(Self {
            config,
            store,
            events,
            relay,
            capture,
            session: session_repo,
            summary_jobs: Arc::new(SummaryJobs::new()),
            planx,
        })
    }
}

// ── Background summary-generation jobs ──

/// Registry of background summary jobs so the dashboard can poll status
/// without blocking on a long-running archive pass.
pub struct SummaryJobs {
    inner: Mutex<HashMap<u64, Arc<SummaryJob>>>,
    next_id: AtomicU64,
}

pub struct SummaryJob {
    state: Mutex<SummaryJobState>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SummaryJobState {
    Running,
    Done {
        summarized: Vec<String>,
        errors: Vec<String>,
    },
    Failed(String),
}

impl SummaryJobs {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a running job and return its id and handle.
    pub fn start(&self) -> (u64, Arc<SummaryJob>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(SummaryJob {
            state: Mutex::new(SummaryJobState::Running),
        });
        self.inner.lock().unwrap().insert(id, job.clone());
        (id, job)
    }

    pub fn get(&self, id: u64) -> Option<Arc<SummaryJob>> {
        self.inner.lock().unwrap().get(&id).cloned()
    }
}

impl SummaryJob {
    pub fn snapshot(&self) -> SummaryJobState {
        self.state.lock().unwrap().clone()
    }

    /// Transition the job to Done or Failed based on the archive result.
    pub fn finish(&self, result: Result<Vec<proxy_store::ArchiveInfo>, proxy_store::StoreError>) {
        let state = match result {
            Ok(items) => SummaryJobState::Done {
                summarized: items
                    .iter()
                    .map(|a| a.session_id.as_str().to_string())
                    .collect(),
                errors: Vec::new(),
            },
            Err(error) => SummaryJobState::Failed(error.to_string()),
        };
        *self.state.lock().unwrap() = state;
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
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
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
                        Some(d) if r.provider.is_empty() || r.provider == d.provider => {
                            r.model.clone()
                        }
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
    // `listen_address` is validated as an IP literal, so this parse cannot fail
    // for a config that got this far. `is_loopback()` is used instead of string
    // comparison so 127.0.0.0/8 and ::1 are all recognised, and `0.0.0.0` / `::`
    // (all interfaces) correctly count as *not* loopback.
    let listen_ip: std::net::IpAddr = config.server.listen_address.trim().parse()?;
    if !listen_ip.is_loopback() {
        // Defence in depth: AppState::new generates a token when none is set, so
        // in practice this never fires — but a non-loopback bind without a token
        // must not be possible.
        if config.server.auth_token.as_deref().unwrap_or("").is_empty() {
            anyhow::bail!(
                "server.auth_token is required when listen_address '{}' is not loopback",
                config.server.listen_address
            );
        }
        tracing::info!(
            "[server] listening on {} (non-loopback, auth_token enforced on /api and /ws)",
            config.server.listen_address
        );
    }

    let http_router = web::build_router(state.clone());
    let http_addr = SocketAddr::new(listen_ip, config.server.http_port);
    // Browser clients cannot call the proxy port cross-origin without CORS, and a
    // preflight is an `OPTIONS` that would otherwise be forwarded upstream. The
    // allowlist is opt-in because this port is unauthenticated: `["*"]` would let
    // any page the operator visits spend their upstream quota.
    let proxy_router = match cors_layer(&config.server.cors_origins) {
        Some(cors) => state.relay.clone().build_router().layer(cors),
        None => state.relay.clone().build_router(),
    };
    let proxy_addr = SocketAddr::new(listen_ip, config.server.proxy_port);

    let http_listener = TcpListener::bind(http_addr).await?;
    let proxy_listener = TcpListener::bind(proxy_addr).await?;

    // Log the *bound* address, not the requested one: with `http_port = 0` the OS
    // picks the port, and the requested address would be a lie.
    tracing::info!("Dashboard: {}", listen_hint(http_listener.local_addr()?));
    tracing::info!("API relay: {}", listen_hint(proxy_listener.local_addr()?));

    // `with_connect_info` is required by the auth middleware, which needs the
    // peer address to decide whether the dashboard cookie may be handed out.
    tokio::try_join!(
        axum::serve(
            http_listener,
            http_router.into_make_service_with_connect_info::<SocketAddr>(),
        ),
        axum::serve(proxy_listener, proxy_router),
    )?;

    Ok(())
}

/// A human-usable URL for a bound listener.
///
/// `0.0.0.0` / `::` are valid bind addresses but not URLs you can open, so an
/// all-interfaces bind also reports the loopback URL to try locally.
/// CORS layer for the proxy port, or `None` when no origin is allowed.
///
/// `["*"]` allows any origin, which is a documented footgun: this port has no
/// auth. Otherwise origins match exactly, tolerating a stored trailing slash
/// because browsers never send one.
fn cors_layer(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    let cleaned: Vec<&str> = origins
        .iter()
        .map(|origin| origin.trim().trim_end_matches('/'))
        .filter(|origin| !origin.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.contains(&"*") {
        tracing::warn!(
            "[server] CORS allows every origin on the proxy port, which has no auth; \
             set server.cors_origins to exact origins if others can reach this port"
        );
        // `permissive` also answers the preflight for the headers browser clients
        // send: authorization, x-api-key, anthropic-version, content-type.
        return Some(tower_http::cors::CorsLayer::permissive());
    }

    let allowed: Vec<axum::http::HeaderValue> = cleaned
        .iter()
        .filter_map(|origin| match origin.parse() {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!("[server] ignoring invalid CORS origin '{origin}'");
                None
            }
        })
        .collect();
    if allowed.is_empty() {
        return None;
    }
    Some(
        tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::AllowOrigin::list(allowed))
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any),
    )
}

fn listen_hint(bound: SocketAddr) -> String {
    if bound.ip().is_unspecified() {
        format!(
            "http://{bound}  (all interfaces; locally: http://127.0.0.1:{})",
            bound.port()
        )
    } else {
        format!("http://{bound}")
    }
}

#[cfg(test)]
mod tests {
    use super::listen_hint;
    use std::net::SocketAddr;

    #[test]
    fn an_unspecified_bind_reports_a_usable_loopback_url() {
        let hint = listen_hint("0.0.0.0:5000".parse::<SocketAddr>().unwrap());
        assert!(hint.contains("0.0.0.0:5000"), "{hint}");
        assert!(hint.contains("http://127.0.0.1:5000"), "{hint}");
    }

    #[test]
    fn a_specific_bind_is_reported_as_is() {
        assert_eq!(
            listen_hint("127.0.0.1:5000".parse::<SocketAddr>().unwrap()),
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            listen_hint("[::1]:5000".parse::<SocketAddr>().unwrap()),
            "http://[::1]:5000"
        );
    }
}
