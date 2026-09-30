//! Credentials for a ChatGPT/Codex subscription account: parsing, OAuth refresh
//! and atomic write-back to `auth.json`.
//!
//! Harvested from `priv/plan2api/src/auth.rs`, with the single-account gateway
//! plumbing removed — here it is a plain mechanism used by [`crate::registry`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use proxy_common::config::AccountFamily;

use crate::error::{truncate, PlanxError, Result};
use crate::jwt::{self, TokenClaims};
use crate::transport::Impersonation;

/// OpenAI OAuth token endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Public Codex CLI OAuth client id.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Scopes requested when refreshing a Codex credential.
pub const REFRESH_SCOPES: &str = "openid profile email";

/// Anthropic OAuth token endpoint (shared by authorization-code exchange and
/// refresh; Claude Code goes through `platform.claude.com`).
pub const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Public Claude Code OAuth client id.
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Refresh this long before the access token actually expires.
pub const REFRESH_SKEW: Duration = Duration::from_secs(300);

/// In-memory credential snapshot.
///
/// `Debug` is hand-written so tokens can never leak through a `{:?}`.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Absolute expiry of `access_token`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

fn redact(secret: &Option<String>) -> Option<String> {
    secret.as_deref().map(|s| {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() <= 6 {
            format!("<redacted:{}>", chars.len())
        } else {
            let head: String = chars[..6].iter().collect();
            format!("{head}…<redacted:{}>", chars.len())
        }
    })
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &redact(&self.refresh_token))
            .field("id_token", &redact(&self.id_token))
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    /// Parse a Codex CLI `auth.json`.
    ///
    /// Accepts the official `{ "tokens": { .. } }` shape, a flat
    /// `{ "access_token": .. }` shape, and the camelCase form exported by
    /// `chatgpt.com/api/auth/session`.
    pub fn from_auth_json(raw: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| PlanxError::config(format!("auth.json is not valid JSON: {e}")))?;
        Ok(Self::from_value(&value))
    }

    /// Parse an already-decoded `auth.json` value.
    pub fn from_value(value: &serde_json::Value) -> Self {
        let pick = |names: &[&str]| -> Option<String> { pick_str(value, names) };
        let tokens = value.get("tokens").filter(|v| v.is_object());
        let pick_token =
            |names: &[&str]| -> Option<String> { tokens.and_then(|t| pick_str(t, names)) };

        let mut creds = Credentials {
            access_token: pick_token(&["access_token", "accessToken"])
                .or_else(|| pick(&["access_token", "accessToken"])),
            refresh_token: pick_token(&["refresh_token", "refreshToken"])
                .or_else(|| pick(&["refresh_token", "refreshToken"])),
            id_token: pick_token(&["id_token", "idToken"])
                .or_else(|| pick(&["id_token", "idToken"])),
            account_id: pick_token(&["account_id", "accountId"])
                .or_else(|| pick(&["account_id", "accountId", "workspace_id"])),
            email: pick(&["email"]),
            expires_at: None,
        };

        // Top up identity facts from the tokens themselves.
        if let Some(claims) = creds.best_claims() {
            if creds.email.is_none() {
                creds.email = claims.email;
            }
            if creds.account_id.is_none() {
                creds.account_id = claims.account_id;
            }
        }
        // `auth.json` carries no expiry, so pre-emptive refresh would never fire
        // for a token loaded from disk. The access token's own `exp` claim is the
        // authority; the id_token's expiry is not (it lives longer).
        if creds.expires_at.is_none() {
            creds.expires_at = creds
                .access_token_claims()
                .and_then(|claims| claims.expires_at);
        }
        creds
    }

    /// Read and parse `auth.json`.
    pub async fn from_file(path: &Path) -> Result<Self> {
        let raw = tokio::fs::read_to_string(path).await?;
        Self::from_auth_json(&raw)
    }

    /// Claims from the id_token, falling back to the access_token.
    pub fn best_claims(&self) -> Option<TokenClaims> {
        self.id_token_claims()
            .or_else(|| self.access_token_claims())
    }

    pub fn id_token_claims(&self) -> Option<TokenClaims> {
        self.id_token.as_deref().and_then(jwt::parse_id_token)
    }

    pub fn access_token_claims(&self) -> Option<TokenClaims> {
        self.access_token
            .as_deref()
            .and_then(jwt::parse_access_token)
    }

    /// Effective upstream plan value.
    pub fn plan_type(&self) -> String {
        self.best_claims()
            .and_then(|c| c.plan_type)
            .unwrap_or_default()
    }

    /// Workspace UUID: explicit field first, then token claims.
    pub fn effective_account_id(&self) -> Option<String> {
        self.account_id
            .clone()
            .or_else(|| self.best_claims().and_then(|c| c.account_id))
    }

    /// Whether the access token is missing or about to expire.
    pub fn needs_refresh(&self, now: DateTime<Utc>) -> bool {
        match (self.access_token.is_some(), self.expires_at) {
            (false, _) => true,
            // Expiry unknown: let an upstream 401 drive the refresh instead.
            (true, None) => false,
            (true, Some(expiry)) => {
                let skew = chrono::Duration::from_std(REFRESH_SKEW).unwrap_or_default();
                now + skew >= expiry
            }
        }
    }
}

fn pick_str(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(found) = value
            .get(*name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(found.to_string());
        }
    }
    None
}

/// Response of the OAuth refresh call (both families).
#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// Holds one account's credentials and refreshes them on demand.
///
/// Concurrency: `refresh_lock` serialises refreshes (single-flight), and
/// `generation` records how many successful refreshes have happened. A caller
/// that waited on the lock while somebody else refreshed — including a forced
/// refresh from the relay's 401 path — observes the new token instead of issuing
/// a second upstream call. A burst of concurrent 401s therefore costs exactly one
/// refresh.
pub struct TokenStore {
    creds: RwLock<Credentials>,
    refresh_lock: Mutex<()>,
    /// Bumped after every successful refresh; read to detect "someone else just
    /// refreshed while I was waiting on the lock".
    generation: AtomicU64,
    http: crate::transport::engine::Client,
    family: AccountFamily,
    token_url: String,
    client_id: String,
    persist_path: Option<PathBuf>,
    /// Browser profile applied to the refresh request. The token endpoints sit
    /// behind the same edge as the usage endpoints, so they need the same shape.
    impersonation: Impersonation,
}

impl std::fmt::Debug for TokenStore {
    /// Hand-written: the engine client is not `Debug` under every backend, and
    /// credentials must never be printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("persist_path", &self.persist_path)
            .finish_non_exhaustive()
    }
}

impl TokenStore {
    pub fn new(
        creds: Credentials,
        http: crate::transport::engine::Client,
        family: AccountFamily,
    ) -> Self {
        let (token_url, client_id) = match family {
            AccountFamily::Gpt => (TOKEN_URL, CLIENT_ID),
            AccountFamily::Claude => (CLAUDE_TOKEN_URL, CLAUDE_CLIENT_ID),
        };
        Self {
            creds: RwLock::new(creds),
            refresh_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            http,
            family,
            token_url: token_url.to_string(),
            client_id: client_id.to_string(),
            persist_path: None,
            impersonation: Impersonation::Off,
        }
    }

    /// Which family this store refreshes for.
    pub fn family(&self) -> AccountFamily {
        self.family
    }

    /// Write rotated credentials back to this `auth.json` after a refresh.
    pub fn with_persist_path(mut self, path: Option<PathBuf>) -> Self {
        self.persist_path = path;
        self
    }

    /// Apply a browser TLS/HTTP2 profile to refresh requests.
    pub fn with_impersonation(mut self, impersonation: Impersonation) -> Self {
        self.impersonation = impersonation;
        self
    }

    /// Override the token endpoint (reverse proxies / tests).
    pub fn with_endpoint(
        mut self,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        self.token_url = token_url.into();
        self.client_id = client_id.into();
        self
    }

    pub async fn snapshot(&self) -> Credentials {
        self.creds.read().await.clone()
    }

    /// Non-blocking credential snapshot for the synchronous hot path.
    ///
    /// Returns `None` when the lock is contended; callers treat that the same as
    /// "no credential yet" and the background refresher catches up.
    pub fn try_snapshot(&self) -> Option<Credentials> {
        self.creds.try_read().ok().map(|guard| guard.clone())
    }

    pub async fn access_token(&self) -> Option<String> {
        self.creds.read().await.access_token.clone()
    }

    pub async fn needs_refresh(&self) -> bool {
        self.creds.read().await.needs_refresh(Utc::now())
    }

    /// Replace the stored credentials outright.
    pub async fn store(&self, creds: Credentials) {
        *self.creds.write().await = creds;
    }

    /// Issue the family-specific refresh request.
    ///
    /// The two families differ in both encoding and scope handling:
    /// - **GPT** posts `application/x-www-form-urlencoded` and sends
    ///   `scope = openid profile email`.
    /// - **Claude** posts JSON and deliberately **omits** `scope`, so the refresh
    ///   inherits the original grant (RFC 6749 §6) — web-session grants can be
    ///   narrower than the full Claude Code scope list.
    async fn send_refresh(
        &self,
        refresh_token: &str,
    ) -> Result<crate::transport::engine::Response> {
        let request = match self.family {
            AccountFamily::Gpt => {
                let form = [
                    ("grant_type", "refresh_token"),
                    ("client_id", self.client_id.as_str()),
                    ("refresh_token", refresh_token),
                    ("scope", REFRESH_SCOPES),
                ];
                self.http
                    .post(&self.token_url)
                    .header(
                        crate::transport::engine::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .header(crate::transport::engine::header::ACCEPT, "application/json")
                    .form(&form)
            }
            AccountFamily::Claude => {
                let payload = serde_json::json!({
                    "client_id": self.client_id,
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                });
                self.http
                    .post(&self.token_url)
                    .header(
                        crate::transport::engine::header::CONTENT_TYPE,
                        "application/json",
                    )
                    .header(crate::transport::engine::header::ACCEPT, "application/json")
                    .json(&payload)
            }
        };
        Ok(
            crate::transport::apply_emulation(request, self.impersonation)
                .send()
                .await?,
        )
    }

    /// Refresh the access token.
    ///
    /// `force = false` returns early when the current token is still valid.
    /// `force = true` is used by the relay's 401 retry path.
    pub async fn refresh(&self, force: bool) -> Result<Credentials> {
        // Read the generation *before* queueing, so a refresh that completes
        // while we wait is visible as a change.
        let generation_before = self.generation.load(Ordering::SeqCst);
        let _guard = self.refresh_lock.lock().await;

        // Double-checked: someone else may have refreshed while we waited. This
        // also collapses a burst of forced (401-driven) refreshes into one.
        {
            let refreshed_while_waiting =
                self.generation.load(Ordering::SeqCst) != generation_before;
            let current = self.creds.read().await;
            if refreshed_while_waiting || (!force && !current.needs_refresh(Utc::now())) {
                return Ok(current.clone());
            }
        }

        let refresh_token = {
            let current = self.creds.read().await;
            current.refresh_token.clone()
        };
        let refresh_token = refresh_token
            .filter(|token| !token.trim().is_empty())
            .ok_or(PlanxError::MissingRefreshToken)?;

        let response = self.send_refresh(&refresh_token).await?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(PlanxError::Refresh {
                status: status.as_u16(),
                body: truncate(&body, 400),
            });
        }

        let parsed: RefreshResponse = serde_json::from_str(&body)?;
        if parsed.access_token.trim().is_empty() {
            return Err(PlanxError::RefreshNoAccessToken);
        }
        let expires_in = if parsed.expires_in > 0 {
            parsed.expires_in
        } else {
            3600
        };

        let mut next = self.creds.read().await.clone();
        next.access_token = Some(parsed.access_token.trim().to_string());
        // Keep the old refresh token when the upstream does not rotate it.
        if !parsed.refresh_token.trim().is_empty() {
            next.refresh_token = Some(parsed.refresh_token.trim().to_string());
        }
        if !parsed.id_token.trim().is_empty() {
            next.id_token = Some(parsed.id_token.trim().to_string());
        }
        next.expires_at = Some(Utc::now() + chrono::Duration::seconds(expires_in));

        if let Some(claims) = next.best_claims() {
            if next.email.is_none() {
                next.email = claims.email;
            }
            if next.account_id.is_none() {
                next.account_id = claims.account_id;
            }
        }
        // Claude access tokens are not JWTs, so `plan_type()` is simply empty for
        // them; nothing to backfill.

        *self.creds.write().await = next.clone();
        self.generation.fetch_add(1, Ordering::SeqCst);

        if let Some(path) = self
            .persist_path
            .clone()
            .filter(|_| self.family == AccountFamily::Gpt)
        {
            match persist_credentials(&path, &next).await {
                Ok(()) => {
                    tracing::debug!("[planx] wrote refreshed credentials to {}", path.display())
                }
                Err(error) => tracing::warn!(
                    "[planx] refreshed OK but failed to write {}: {}",
                    path.display(),
                    error
                ),
            }
        }

        Ok(next)
    }
}

/// Write credentials back to a Codex CLI `auth.json`.
///
/// - preserves unknown fields (only `tokens.*` and `last_refresh` are touched);
/// - refuses to overwrite a file that is not valid JSON;
/// - writes atomically (temp file + rename);
/// - sets mode 0600 on Unix.
pub async fn persist_credentials(path: &Path, creds: &Credentials) -> Result<()> {
    let existing = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(PlanxError::Io(error)),
    };

    let mut root: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&existing).map_err(|e| {
            PlanxError::config(format!(
                "{} is not valid JSON, refusing to overwrite: {e}",
                path.display()
            ))
        })?
    };

    let object = root
        .as_object_mut()
        .ok_or_else(|| PlanxError::config(format!("{} is not a JSON object", path.display())))?;
    let tokens = object
        .entry("tokens")
        .or_insert_with(|| serde_json::json!({}));
    let tokens = tokens
        .as_object_mut()
        .ok_or_else(|| PlanxError::config(format!("{} tokens is not an object", path.display())))?;

    for (key, value) in [
        ("access_token", creds.access_token.as_deref()),
        ("refresh_token", creds.refresh_token.as_deref()),
        ("id_token", creds.id_token.as_deref()),
    ] {
        if let Some(value) = value {
            tokens.insert(key.to_string(), value.into());
        }
    }
    if let Some(account_id) = creds.effective_account_id() {
        tokens.insert("account_id".into(), account_id.into());
    }
    object.insert(
        "last_refresh".into(),
        Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .into(),
    );

    let mut payload = serde_json::to_vec_pretty(&root)?;
    payload.push(b'\n');

    let tmp = path.with_extension("json.planx.tmp");
    tokio::fs::write(&tmp, &payload).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Expand a leading `~/` using `$HOME`.
pub fn expand_tilde(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(rest);
            }
        }
    }
    PathBuf::from(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn jws(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{body}.sig")
    }

    fn temp_auth_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "planx-auth-{tag}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn parses_official_auth_json() {
        let id = jws(serde_json::json!({
            "email": "u@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-1",
                "chatgpt_plan_type": "team"
            }
        }));
        let raw = serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id,
                "access_token": "at-123",
                "refresh_token": "rt-456",
                "account_id": "acct-1"
            }
        })
        .to_string();

        let creds = Credentials::from_auth_json(&raw).unwrap();
        assert_eq!(creds.access_token.as_deref(), Some("at-123"));
        assert_eq!(creds.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(creds.plan_type(), "team");
        assert_eq!(creds.effective_account_id().as_deref(), Some("acct-1"));
    }

    #[test]
    fn parses_camel_case_session_json() {
        let raw = serde_json::json!({ "accessToken": "at-9" }).to_string();
        let creds = Credentials::from_auth_json(&raw).unwrap();
        assert_eq!(creds.access_token.as_deref(), Some("at-9"));
    }

    #[test]
    fn debug_never_leaks_tokens() {
        let creds = Credentials {
            access_token: Some("at-super-secret-value".into()),
            refresh_token: Some("rt-super-secret-value".into()),
            ..Default::default()
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn needs_refresh_respects_skew() {
        let mut creds = Credentials {
            access_token: Some("at".into()),
            ..Default::default()
        };
        assert!(!creds.needs_refresh(Utc::now()));
        creds.expires_at = Some(Utc::now() + chrono::Duration::seconds(60));
        assert!(creds.needs_refresh(Utc::now()));
        creds.expires_at = Some(Utc::now() + chrono::Duration::hours(2));
        assert!(!creds.needs_refresh(Utc::now()));
        creds.access_token = None;
        assert!(creds.needs_refresh(Utc::now()));
    }

    #[test]
    fn expand_tilde_uses_home() {
        std::env::set_var("HOME", "/tmp/planx-home");
        assert_eq!(
            expand_tilde("~/.codex/auth.json"),
            PathBuf::from("/tmp/planx-home/.codex/auth.json")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn expiry_is_taken_from_the_access_token_claim() {
        // auth.json has no expiry field; without this a token loaded from disk
        // would never be proactively refreshed.
        let token = jws(serde_json::json!({ "exp": 1_893_456_000_i64 }));
        let raw = serde_json::json!({ "tokens": { "access_token": token, "refresh_token": "rt" } })
            .to_string();
        let creds = Credentials::from_auth_json(&raw).unwrap();
        assert_eq!(
            creds.expires_at.map(|dt| dt.timestamp()),
            Some(1_893_456_000)
        );
    }

    /// A local token endpoint: answers every request with `body` after a short
    /// delay, and counts the connections it served.
    struct TokenStub {
        url: String,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TokenStub {
        async fn start(body: &'static str) -> Self {
            use tokio::io::AsyncWriteExt;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = hits.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    counter.fetch_add(1, Ordering::SeqCst);
                    // Hold the connection open so concurrent callers overlap.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            Self {
                url: format!("http://{addr}/token"),
                hits,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn store_for(url: &str) -> TokenStore {
        TokenStore::new(
            Credentials {
                access_token: Some("at-old".into()),
                refresh_token: Some("rt-1".into()),
                ..Default::default()
            },
            crate::transport::engine::Client::new(),
            AccountFamily::Gpt,
        )
        .with_endpoint(url, "client-id")
    }

    #[tokio::test]
    async fn a_burst_of_forced_refreshes_costs_one_upstream_call() {
        // The documented guarantee of the 401 retry path: many requests rejected
        // at once must not each refresh.
        let stub = TokenStub::start(r#"{"access_token":"at-new","expires_in":3600}"#).await;
        let store = std::sync::Arc::new(store_for(&stub.url));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move { store.refresh(true).await }));
        }
        for task in tasks {
            let creds = task.await.unwrap().expect("refresh succeeds");
            assert_eq!(creds.access_token.as_deref(), Some("at-new"));
        }
        assert_eq!(stub.hits(), 1, "single-flight must collapse the burst");
        assert_eq!(
            store.snapshot().await.access_token.as_deref(),
            Some("at-new")
        );
    }

    #[tokio::test]
    async fn a_non_forced_refresh_skips_the_network_when_the_token_is_fresh() {
        let stub = TokenStub::start(r#"{"access_token":"at-new","expires_in":3600}"#).await;
        let store = store_for(&stub.url);
        store.refresh(true).await.unwrap();
        assert_eq!(stub.hits(), 1);

        store.refresh(false).await.unwrap();
        assert_eq!(stub.hits(), 1, "a fresh token needs no second call");
    }

    #[tokio::test]
    async fn a_refresh_without_a_refresh_token_is_typed() {
        // The mechanism has no account name; the registry adds it.
        let store = store_for("http://127.0.0.1:1/token");
        store
            .store(Credentials {
                access_token: None,
                ..Default::default()
            })
            .await;
        let error = store.refresh(true).await.unwrap_err();
        assert!(
            matches!(error, PlanxError::MissingRefreshToken),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn persist_preserves_unknown_fields() {
        let path = temp_auth_path("preserve");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "OPENAI_API_KEY": null,
                "tokens": { "access_token": "old", "future_field": "keep" },
                "unknown_top": { "nested": true }
            }))
            .unwrap(),
        )
        .unwrap();

        let creds = Credentials {
            access_token: Some("new".into()),
            refresh_token: Some("rt-new".into()),
            ..Default::default()
        };
        persist_credentials(&path, &creds).await.unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["tokens"]["access_token"], "new");
        assert_eq!(written["tokens"]["refresh_token"], "rt-new");
        assert_eq!(written["tokens"]["future_field"], "keep");
        assert_eq!(written["unknown_top"]["nested"], true);
        assert!(written.get("OPENAI_API_KEY").is_some());
        assert!(!path.with_extension("json.planx.tmp").exists());

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn persist_refuses_invalid_json() {
        let path = temp_auth_path("invalid");
        std::fs::write(&path, b"{ not json").unwrap();
        let creds = Credentials {
            access_token: Some("at".into()),
            ..Default::default()
        };
        assert!(persist_credentials(&path, &creds).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persist_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_auth_path("mode");
        persist_credentials(
            &path,
            &Credentials {
                access_token: Some("at".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
        let _ = std::fs::remove_file(&path);
    }
}
