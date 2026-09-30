//! Zero-spend quota probing for both upstream families.
//!
//! Both endpoints are read-only and consume no inference quota, so they are safe
//! to call on a schedule (the Go implementation uses the same pair).
//!
//! | family | endpoint | credential requirement |
//! |---|---|---|
//! | GPT | `GET chatgpt.com/backend-api/wham/usage` | any ChatGPT OAuth token |
//! | Claude | `GET api.anthropic.com/api/oauth/usage` | needs the `user:profile` scope — a `setup-token` (`sk-ant-oat01-…`) is inference-only and will be rejected |
//!
//! Two separations keep this module small:
//!
//! * **Credential headers are not built here.** The caller passes the account's
//!   published [`UpstreamAuth`], so a probe carries exactly the headers a proxied
//!   request would. Only the URL selection and the *response shape* are
//!   family-specific.
//! * **Parsing is split from I/O**, so both response shapes are unit-testable
//!   without a network or an HTTP mock.

use proxy_common::auth::UpstreamAuth;
use proxy_common::config::AccountFamily;
use serde::{Deserialize, Deserializer};

use crate::error::{truncate, PlanxError, Result};
use crate::transport::{apply_emulation, engine, Impersonation};

/// ChatGPT backend usage endpoint.
pub const WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// Anthropic OAuth usage endpoint (the one Claude Code itself uses).
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Endpoint overrides, so tests can point at a local server.
#[derive(Debug, Clone)]
pub struct ProbeUrls {
    pub gpt: String,
    pub claude: String,
}

impl Default for ProbeUrls {
    fn default() -> Self {
        Self {
            gpt: WHAM_USAGE_URL.to_string(),
            claude: CLAUDE_USAGE_URL.to_string(),
        }
    }
}

impl ProbeUrls {
    /// The usage endpoint for a family.
    pub fn for_family(&self, family: AccountFamily) -> &str {
        match family {
            AccountFamily::Gpt => &self.gpt,
            AccountFamily::Claude => &self.claude,
        }
    }
}

/// One usage bucket, normalized across families.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotaWindow {
    /// Stable identifier (`5h`, `7d`, `7d_fable`).
    pub name: String,
    /// Human-facing label.
    pub label: String,
    /// Utilization percentage, clamped to `0..=100`.
    pub utilization: f64,
    /// Unix seconds when the window resets, when known.
    pub resets_at: Option<i64>,
    /// True for per-model-family buckets (Claude only).
    pub model_scoped: bool,
}

impl QuotaWindow {
    fn new(name: &str, label: &str, utilization: f64, resets_at: Option<i64>) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            utilization: clamp_percent(utilization),
            resets_at,
            model_scoped: false,
        }
    }

    fn model_scoped(mut self) -> Self {
        self.model_scoped = true;
        self
    }
}

fn clamp_percent(value: f64) -> f64 {
    if value.is_nan() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

/// Normalized quota snapshot for one account.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountQuota {
    pub family: AccountFamily,
    /// Subscription plan name when the upstream reports one.
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    /// True when the account has unlimited credits (GPT).
    pub credits_unlimited: bool,
    pub credits_balance: Option<String>,
    /// Remaining "active reset" credits (GPT).
    pub reset_credits: Option<u32>,
    /// Upstream says the rate limit is currently exhausted.
    pub limit_reached: bool,
    /// Unix seconds when this snapshot was taken.
    pub probed_at: i64,
    /// Populated instead of failing hard when the probe did not succeed.
    pub error: Option<String>,
}

impl AccountQuota {
    fn ok(family: AccountFamily, windows: Vec<QuotaWindow>) -> Self {
        Self {
            family,
            plan: None,
            windows,
            credits_unlimited: false,
            credits_balance: None,
            reset_credits: None,
            limit_reached: false,
            probed_at: chrono::Utc::now().timestamp(),
            error: None,
        }
    }

    /// A failed probe. Kept as data (rather than an `Err`) so one broken account
    /// does not break the whole quota listing.
    pub fn failed(family: AccountFamily, error: impl Into<String>) -> Self {
        let mut quota = Self::ok(family, Vec::new());
        quota.error = Some(error.into());
        quota
    }

    /// The tightest window (highest utilization), if any.
    pub fn worst_window(&self) -> Option<&QuotaWindow> {
        self.windows.iter().max_by(|a, b| {
            a.utilization
                .partial_cmp(&b.utilization)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

// ── Flexible JSON scalars ──
//
// Both upstreams mix types across fields and over time (number vs string, int vs
// float), so numbers and timestamps are decoded leniently.

fn flexible_f64<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Float(f64),
        Int(i64),
        Text(String),
    }
    Ok(match Raw::deserialize(deserializer)? {
        Raw::Float(value) => value,
        Raw::Int(value) => value as f64,
        Raw::Text(value) => value.trim().parse().unwrap_or(0.0),
    })
}

/// Parse a timestamp that may be an integer epoch, a float, or an RFC 3339 string.
fn optional_timestamp<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error> {
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|value| match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|seconds| seconds as i64)),
        serde_json::Value::String(text) => {
            let text = text.trim();
            if let Ok(epoch) = text.parse::<i64>() {
                return Some(epoch);
            }
            chrono::DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|dt| dt.timestamp())
        }
        _ => None,
    }))
}

// ── GPT: /backend-api/wham/usage ──

#[derive(Debug, Deserialize)]
struct WhamUsage {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<WhamRateLimit>,
    #[serde(default)]
    credits: Option<WhamCredits>,
    #[serde(default)]
    rate_limit_reset_credits: Option<WhamResetCredits>,
}

#[derive(Debug, Deserialize)]
struct WhamRateLimit {
    #[serde(default)]
    limit_reached: bool,
    #[serde(default)]
    primary_window: Option<WhamWindow>,
    #[serde(default)]
    secondary_window: Option<WhamWindow>,
}

#[derive(Debug, Deserialize)]
struct WhamWindow {
    #[serde(default, deserialize_with = "flexible_f64")]
    used_percent: f64,
    #[serde(default, deserialize_with = "optional_timestamp")]
    reset_at: Option<i64>,
    #[serde(default, deserialize_with = "flexible_f64")]
    reset_after_seconds: f64,
    /// Authoritative window length. Preferred over `reset_after_seconds`, which
    /// shrinks as the window drains and can therefore mislabel a 30d window.
    #[serde(default, deserialize_with = "flexible_f64")]
    limit_window_seconds: f64,
}

#[derive(Debug, Deserialize)]
struct WhamCredits {
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    balance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhamResetCredits {
    #[serde(default)]
    available_count: u32,
}

/// Label a GPT window from its horizon.
///
/// `limit_window_seconds` is the declared window length and is authoritative;
/// `reset_after_seconds` is the fallback for older upstream responses. GPT rolls
/// quotas on a 5h / 7d / 30d horizon.
fn wham_label(window: &WhamWindow) -> (&'static str, &'static str) {
    const HOUR: f64 = 3600.0;
    const DAY: f64 = 24.0 * HOUR;
    let horizon = if window.limit_window_seconds > 0.0 {
        window.limit_window_seconds
    } else {
        window.reset_after_seconds
    };
    match horizon {
        seconds if seconds > 0.0 && seconds <= 6.0 * HOUR => ("5h", "5h"),
        seconds if seconds > 0.0 && seconds <= 8.0 * DAY => ("7d", "7d"),
        seconds if seconds > 0.0 && seconds <= 45.0 * DAY => ("30d", "30d"),
        _ => ("window", "window"),
    }
}

/// Normalize a `wham/usage` response body.
pub fn parse_wham_usage(body: &[u8]) -> Result<AccountQuota> {
    let parsed: WhamUsage = serde_json::from_slice(body)
        .map_err(|e| PlanxError::config(format!("invalid wham/usage response: {e}")))?;

    let mut quota = AccountQuota::ok(AccountFamily::Gpt, Vec::new());
    quota.plan = parsed.plan_type.filter(|plan| !plan.trim().is_empty());

    if let Some(rate_limit) = parsed.rate_limit {
        quota.limit_reached = rate_limit.limit_reached;
        for window in [rate_limit.primary_window, rate_limit.secondary_window]
            .into_iter()
            .flatten()
        {
            push_unique(&mut quota.windows, wham_window(&window));
        }
    }

    if let Some(credits) = parsed.credits {
        quota.credits_unlimited = credits.unlimited;
        quota.credits_balance = credits.balance.filter(|b| !b.trim().is_empty());
    }
    quota.reset_credits = parsed
        .rate_limit_reset_credits
        .map(|credits| credits.available_count);
    Ok(quota)
}

/// One raw window → one normalized window.
fn wham_window(window: &WhamWindow) -> QuotaWindow {
    let (name, label) = wham_label(window);
    // Prefer the absolute reset time; fall back to "now + reset_after".
    let resets_at = window.reset_at.or_else(|| {
        (window.reset_after_seconds > 0.0)
            .then(|| chrono::Utc::now().timestamp() + window.reset_after_seconds as i64)
    });
    QuotaWindow::new(name, label, window.used_percent, resets_at)
}

/// De-duplicate windows that normalize onto the same name (two windows can share
/// a horizon), keeping the first.
fn push_unique(windows: &mut Vec<QuotaWindow>, window: QuotaWindow) {
    if windows.iter().any(|existing| existing.name == window.name) {
        return;
    }
    windows.push(window);
}

// ── Claude: /api/oauth/usage ──

#[derive(Debug, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    five_hour: Option<ClaudeBucket>,
    #[serde(default)]
    seven_day: Option<ClaudeBucket>,
    #[serde(default)]
    limits: Vec<ClaudeLimit>,
}

#[derive(Debug, Deserialize)]
struct ClaudeBucket {
    #[serde(default, deserialize_with = "flexible_f64")]
    utilization: f64,
    #[serde(default, deserialize_with = "optional_timestamp")]
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ClaudeLimit {
    #[serde(default)]
    group: String,
    #[serde(default, deserialize_with = "flexible_f64")]
    percent: f64,
    #[serde(default, deserialize_with = "optional_timestamp")]
    resets_at: Option<i64>,
    #[serde(default)]
    scope: Option<ClaudeScope>,
}

#[derive(Debug, Deserialize)]
struct ClaudeScope {
    #[serde(default)]
    model: Option<ClaudeModel>,
}

#[derive(Debug, Deserialize)]
struct ClaudeModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: String,
}

/// Map a Claude model name onto a stable bucket suffix.
fn claude_model_family(name: &str) -> Option<&'static str> {
    let lowered = name.to_ascii_lowercase();
    if lowered.contains("fable") {
        Some("fable")
    } else if lowered.contains("opus") {
        Some("opus")
    } else if lowered.contains("sonnet") {
        Some("sonnet")
    } else if lowered.contains("haiku") {
        Some("haiku")
    } else {
        None
    }
}

fn family_label(family: &str) -> String {
    let mut chars = family.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// Normalize an Anthropic `/api/oauth/usage` response body.
pub fn parse_claude_usage(body: &[u8]) -> Result<AccountQuota> {
    let parsed: ClaudeUsage = serde_json::from_slice(body)
        .map_err(|e| PlanxError::config(format!("invalid claude usage response: {e}")))?;

    let mut windows = Vec::new();
    if let Some(bucket) = parsed.five_hour {
        push_unique(
            &mut windows,
            QuotaWindow::new("5h", "5h", bucket.utilization, bucket.resets_at),
        );
    }
    if let Some(bucket) = parsed.seven_day {
        push_unique(
            &mut windows,
            QuotaWindow::new("7d", "7d", bucket.utilization, bucket.resets_at),
        );
    }

    // Weekly per-model-family limits. Several models can share one bucket
    // (Fable 5 and 5.1 are a single weekly limit), so names are de-duplicated.
    for limit in parsed.limits {
        if let Some(window) = claude_limit_window(&limit) {
            push_unique(&mut windows, window);
        }
    }

    Ok(AccountQuota::ok(AccountFamily::Claude, windows))
}

/// One weekly per-model limit → one normalized window, or `None` when the entry
/// is not a model-scoped weekly bucket.
fn claude_limit_window(limit: &ClaudeLimit) -> Option<QuotaWindow> {
    if !limit.group.eq_ignore_ascii_case("weekly") {
        return None;
    }
    let model = limit.scope.as_ref()?.model.as_ref()?;
    let family = claude_model_family(&format!("{} {}", model.display_name, model.id))?;
    Some(
        QuotaWindow::new(
            &format!("7d_{family}"),
            &format!("{} 5.x", family_label(family)),
            limit.percent,
            limit.resets_at,
        )
        .model_scoped(),
    )
}

// ── I/O ──

/// Everything one probe needs. The credential and identity headers come from the
/// account's published auth, never from a second construction path.
pub struct ProbeRequest<'a> {
    pub client: &'a engine::Client,
    pub family: AccountFamily,
    pub url: &'a str,
    pub auth: &'a UpstreamAuth,
    pub impersonation: Impersonation,
}

/// Probe one account's quota.
///
/// Returns `AccountQuota::failed` rather than an error so a single unreachable
/// account cannot break a listing of all accounts.
pub async fn probe(request: ProbeRequest<'_>) -> AccountQuota {
    let response = match send(&request).await {
        Ok(response) => response,
        Err(error) => return AccountQuota::failed(request.family, error.to_string()),
    };
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        return AccountQuota::failed(
            request.family,
            failure_message(request.family, request.url, status.as_u16(), &body),
        );
    }
    parse_body(request.family, &body)
}

async fn send(request: &ProbeRequest<'_>) -> Result<engine::Response> {
    let mut builder = request
        .client
        .get(request.url)
        .header(engine::header::ACCEPT, "application/json");
    for (name, value) in request.auth.header_pairs() {
        match (
            engine::header::HeaderName::from_bytes(name.as_bytes()),
            engine::header::HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => builder = builder.header(name, value),
            _ => tracing::warn!("[planx] dropping invalid probe header '{name}'"),
        }
    }
    Ok(apply_emulation(builder, request.impersonation)
        .send()
        .await?)
}

fn failure_message(family: AccountFamily, url: &str, status: u16, body: &[u8]) -> String {
    let hint = match (family, status) {
        // A setup-token is inference-only; say so instead of echoing a bare 403.
        (AccountFamily::Claude, 403) => {
            " (Claude usage needs the user:profile scope; a setup-token cannot read it)"
        }
        _ => "",
    };
    format!(
        "{url} returned {status}{hint}: {}",
        truncate(&String::from_utf8_lossy(body), 200)
    )
}

fn parse_body(family: AccountFamily, body: &[u8]) -> AccountQuota {
    let parsed = match family {
        AccountFamily::Gpt => parse_wham_usage(body),
        AccountFamily::Claude => parse_claude_usage(body),
    };
    parsed.unwrap_or_else(|error| AccountQuota::failed(family, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(headers: &[(&str, &str)]) -> UpstreamAuth {
        UpstreamAuth::Headers {
            set: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            append: Vec::new(),
        }
    }

    // ── GPT ──

    #[test]
    fn wham_usage_normalizes_both_windows() {
        let body = br#"{
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 23.5,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 3600,
                    "reset_at": 1800000000
                },
                "secondary_window": {
                    "used_percent": 61,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 300000,
                    "reset_at": 1800600000
                }
            }
        }"#;

        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.family, AccountFamily::Gpt);
        assert_eq!(quota.plan.as_deref(), Some("plus"));
        assert!(!quota.limit_reached);
        assert_eq!(quota.windows.len(), 2);
        assert_eq!(quota.windows[0].name, "5h");
        assert_eq!(quota.windows[0].utilization, 23.5);
        assert_eq!(quota.windows[0].resets_at, Some(1800000000));
        assert_eq!(quota.windows[1].name, "7d");
        assert_eq!(quota.windows[1].utilization, 61.0, "int coerced to float");
    }

    #[test]
    fn wham_usage_reads_credits_and_reset_credits() {
        let body = br#"{
            "rate_limit": {"limit_reached": true, "primary_window": null, "secondary_window": null},
            "credits": {"unlimited": false, "balance": "12.50"},
            "rate_limit_reset_credits": {"available_count": 2, "applicable_available_count": 1}
        }"#;
        let quota = parse_wham_usage(body).unwrap();
        assert!(quota.limit_reached);
        assert_eq!(quota.credits_balance.as_deref(), Some("12.50"));
        assert_eq!(quota.reset_credits, Some(2));
        assert!(quota.windows.is_empty());
    }

    #[test]
    fn wham_usage_falls_back_to_reset_after_for_label_and_time() {
        let body = br#"{
            "rate_limit": {
                "primary_window": {"used_percent": 5, "reset_after_seconds": 120},
                "secondary_window": {"used_percent": 7, "reset_after_seconds": 604800}
            }
        }"#;
        let before = chrono::Utc::now().timestamp();
        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.windows[0].name, "5h");
        assert_eq!(quota.windows[1].name, "7d");
        let reset = quota.windows[0]
            .resets_at
            .expect("derived from reset_after");
        assert!(reset >= before + 100 && reset <= before + 200, "{reset}");
    }

    #[test]
    fn wham_usage_labels_a_thirty_day_window_from_the_declared_length() {
        // `reset_after_seconds` has shrunk to 9 days; the declared 30d window is
        // still the authoritative label.
        let body = br#"{
            "rate_limit": {
                "secondary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 2592000,
                    "reset_after_seconds": 777600
                }
            }
        }"#;
        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.windows[0].name, "30d");
    }

    #[test]
    fn wham_usage_drops_a_duplicate_horizon() {
        let body = br#"{
            "rate_limit": {
                "primary_window": {"used_percent": 1, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": 2, "limit_window_seconds": 18000}
            }
        }"#;
        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.windows.len(), 1);
    }

    #[test]
    fn wham_usage_tolerates_string_numbers_and_null_reset() {
        let body = br#"{
            "plan_type": "",
            "rate_limit": {
                "primary_window": {"used_percent": "42.5", "reset_at": null}
            }
        }"#;
        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.windows[0].utilization, 42.5);
        assert_eq!(quota.windows[0].resets_at, None);
        assert_eq!(quota.plan, None, "empty plan is dropped");
    }

    #[test]
    fn wham_usage_clamps_out_of_range_percentages() {
        let body = br#"{
            "rate_limit": {"primary_window": {"used_percent": 140, "reset_after_seconds": 60}}
        }"#;
        let quota = parse_wham_usage(body).unwrap();
        assert_eq!(quota.windows[0].utilization, 100.0);
    }

    #[test]
    fn wham_usage_rejects_a_non_json_body() {
        let error = parse_wham_usage(b"<html>oops</html>").unwrap_err();
        assert!(error.to_string().contains("invalid wham/usage"));
    }

    #[test]
    fn wham_usage_accepts_a_minimal_body() {
        let quota = parse_wham_usage(b"{}").unwrap();
        assert!(quota.windows.is_empty());
        assert_eq!(quota.plan, None);
    }

    // ── Claude ──

    #[test]
    fn claude_usage_normalizes_windows_and_weekly_model_limits() {
        let body = br#"{
            "five_hour": {"utilization": 12.0, "resets_at": "2030-01-01T00:00:00Z"},
            "seven_day": {"utilization": 45.5, "resets_at": 1893456000},
            "limits": [
                {
                    "group": "weekly",
                    "percent": 30,
                    "resets_at": 1893456000,
                    "scope": {"model": {"id": "claude-fable-5", "display_name": "Fable 5"}}
                },
                {
                    "group": "weekly",
                    "percent": 31,
                    "scope": {"model": {"id": "claude-fable-5-1", "display_name": "Fable 5.1"}}
                },
                {
                    "group": "daily",
                    "percent": 99,
                    "scope": {"model": {"id": "claude-opus-4", "display_name": "Opus 4"}}
                },
                {
                    "group": "weekly",
                    "percent": 10,
                    "scope": {"model": {"id": "unknown-model", "display_name": "Mystery"}}
                }
            ]
        }"#;

        let quota = parse_claude_usage(body).unwrap();
        assert_eq!(quota.family, AccountFamily::Claude);
        let names: Vec<&str> = quota.windows.iter().map(|w| w.name.as_str()).collect();
        // Fable 5 and 5.1 share one weekly bucket; the daily limit is ignored.
        assert_eq!(names, vec!["5h", "7d", "7d_fable"]);

        assert_eq!(quota.windows[0].utilization, 12.0);
        assert_eq!(
            quota.windows[0].resets_at,
            Some(1893456000),
            "RFC3339 parsed to epoch"
        );
        assert_eq!(quota.windows[2].label, "Fable 5.x");
        assert!(quota.windows[2].model_scoped);
        assert!(!quota.windows[1].model_scoped);
    }

    #[test]
    fn claude_usage_accepts_an_empty_body() {
        let quota = parse_claude_usage(b"{}").unwrap();
        assert!(quota.windows.is_empty());
        assert!(quota.error.is_none());
    }

    #[test]
    fn claude_usage_rejects_a_non_json_body() {
        assert!(parse_claude_usage(b"nope").is_err());
    }

    #[test]
    fn claude_model_families_are_matched_by_name() {
        assert_eq!(claude_model_family("claude-fable-5"), Some("fable"));
        assert_eq!(claude_model_family("Claude Opus 4"), Some("opus"));
        assert_eq!(claude_model_family("sonnet"), Some("sonnet"));
        assert_eq!(claude_model_family("haiku-3"), Some("haiku"));
        assert_eq!(claude_model_family("gpt-5"), None);
    }

    // ── shared ──

    #[test]
    fn failed_quota_keeps_the_error_and_an_empty_window_list() {
        let quota = AccountQuota::failed(AccountFamily::Claude, "boom");
        assert_eq!(quota.error.as_deref(), Some("boom"));
        assert!(quota.windows.is_empty());
        assert!(quota.worst_window().is_none());
    }

    #[test]
    fn worst_window_picks_the_highest_utilization() {
        let quota = AccountQuota::ok(
            AccountFamily::Gpt,
            vec![
                QuotaWindow::new("5h", "5h", 10.0, None),
                QuotaWindow::new("7d", "7d", 80.0, None),
            ],
        );
        assert_eq!(quota.worst_window().unwrap().name, "7d");
    }

    #[test]
    fn probe_urls_default_to_the_real_endpoints() {
        let urls = ProbeUrls::default();
        assert_eq!(urls.gpt, WHAM_USAGE_URL);
        assert_eq!(urls.claude, CLAUDE_USAGE_URL);
        assert_eq!(urls.for_family(AccountFamily::Gpt), WHAM_USAGE_URL);
        assert_eq!(urls.for_family(AccountFamily::Claude), CLAUDE_USAGE_URL);
    }

    #[test]
    fn a_claude_403_names_the_scope_requirement() {
        let message = failure_message(AccountFamily::Claude, "https://x", 403, b"denied");
        assert!(message.contains("user:profile"), "{message}");
        let message = failure_message(AccountFamily::Gpt, "https://x", 403, b"denied");
        assert!(!message.contains("user:profile"), "{message}");
    }

    #[tokio::test]
    async fn probe_reports_a_transport_failure_as_data() {
        // Port 1 is not listening, so this fails fast without a network.
        let client = engine::Client::new();
        let quota = probe(ProbeRequest {
            client: &client,
            family: AccountFamily::Gpt,
            url: "http://127.0.0.1:1/usage",
            auth: &auth(&[("authorization", "Bearer at")]),
            impersonation: Impersonation::Off,
        })
        .await;
        assert!(quota.error.is_some());
        assert!(quota.windows.is_empty());
    }
}
