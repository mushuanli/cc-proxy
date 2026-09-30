//! Runtime registry: turns configured accounts into live `UpstreamAuth`.
//!
//! This is the only type `proxy-server` needs to construct; `proxy-relay` sees
//! it solely through `proxy_common::auth::PlanAuthProvider`.
//!
//! # Two families × two modes
//!
//! An account resolves to a set of headers, and the placement is family- and
//! mode-specific:
//!
//! | family | `api_key` | `plan` |
//! |---|---|---|
//! | GPT | `authorization: Bearer sk-…` (or `x-api-key` for non-`sk-`) | `authorization: Bearer <AT>` + Codex identity |
//! | Claude | `x-api-key: sk-ant-…` | `authorization: Bearer <AT>` + Claude Code identity + merged `anthropic-beta` |
//!
//! # Why the auth is cached in a `std::sync::RwLock`
//!
//! The provider trait is **synchronous on the hot path** so a proxied request
//! never waits on a token refresh. The credential store is async (refresh
//! performs network I/O), so the two are decoupled: the store stays the source
//! of truth, and a plain `UpstreamAuth` snapshot is republished into a sync lock
//! whenever the credential changes. `resolve()` is then a cheap read with no
//! `await`, no `block_in_place` and no runtime assumptions.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use proxy_common::auth::{PlanAuthFuture, PlanAuthProvider, UpstreamAuth};
use proxy_common::config::{AccountConfig, AccountFamily, AccountMode};

use crate::credential::{expand_tilde, Credentials, TokenStore};
use crate::error::{PlanxError, Result};
use crate::identity::Identity;
use crate::probe::{self, AccountQuota, ProbeRequest, ProbeUrls};
use crate::transport::Impersonation;

/// One configured account plus its live credential store.
pub struct PlanAccount {
    /// The config this account was built from. Used by [`PlanxRegistry::reload`]
    /// to tell an unchanged account from a changed one.
    config: AccountConfig,
    name: String,
    family: AccountFamily,
    mode: AccountMode,
    identity: Identity,
    account_seed: String,
    /// Shared maintenance client (never pooled).
    http: crate::transport::engine::Client,
    /// `None` in api-key mode (nothing to refresh).
    store: Option<TokenStore>,
    /// Static credential for `api_key` mode.
    static_api_key: Option<String>,
    impersonate: Impersonation,
    /// Sync snapshot of the current auth material, republished on every change.
    current: RwLock<Option<UpstreamAuth>>,
    /// Last quota probe result, if any.
    quota: RwLock<Option<AccountQuota>>,
    /// Set when the local Codex CLI reports a version newer than the one we
    /// advertise — a stale version silently shrinks the upstream model manifest.
    stale_cli_version: Option<String>,
}

impl std::fmt::Debug for PlanAccount {
    /// Hand-written: the engine client is not `Debug` under every backend
    /// (`wreq::Client` is not), and credentials must never be printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanAccount")
            .field("name", &self.name)
            .field("family", &self.family)
            .field("mode", &self.mode)
            .field("identity", &self.identity.profile)
            .field("impersonate", &self.impersonate)
            .field("ready", &self.is_ready())
            .field("refreshable", &self.is_refreshable())
            .finish_non_exhaustive()
    }
}

impl PlanAccount {
    /// Build from configuration, loading initial credentials.
    pub async fn from_config(config: &AccountConfig, proxy: Option<&str>) -> Result<Self> {
        if let Some(problem) = config.credential_problem() {
            return Err(PlanxError::config(format!(
                "account '{}': {problem}",
                config.name
            )));
        }
        let http = crate::transport::maintenance_client(proxy)?;
        let store = Self::build_store(config, http.clone()).await?;
        let identity = Identity::from_profile_name(config.identity.as_deref(), config.family)
            .with_cli_version(config.cli_version.as_deref());
        // Best-effort drift check against the local Codex installation.
        let stale_cli_version = config
            .auth_json_path()
            .map(expand_tilde)
            .and_then(|path| crate::credential::local_cli_latest_version(&path))
            .filter(|latest| crate::identity::version_is_newer(latest, &identity.client_version));
        if let Some(latest) = stale_cli_version.as_deref() {
            tracing::warn!(
                "[planx] account '{}' advertises cli_version={} but the local Codex CLI reports {}; \
                 the upstream model manifest is version-gated, so set cli_version to follow it",
                config.name,
                identity.client_version,
                latest
            );
        }

        let account = Self {
            config: config.clone(),
            name: config.name.trim().to_string(),
            family: config.family,
            mode: config.mode,
            identity,
            account_seed: config.name.trim().to_string(),
            http,
            store,
            static_api_key: config.api_key.clone(),
            impersonate: config
                .impersonate
                .as_deref()
                .and_then(Impersonation::parse)
                .unwrap_or_default(),
            current: RwLock::new(None),
            quota: RwLock::new(None),
            stale_cli_version: None,
        };
        if !account.impersonate.is_off() && !crate::transport::IMPERSONATION_COMPILED {
            // Otherwise the operator sets a profile and silently gets the default
            // TLS stack, which looks identical to "impersonation didn't help".
            tracing::warn!(
                "[planx] account '{}' asks for impersonate={} but this build has no `impersonate` feature; the default TLS stack will be used",
                account.name,
                account.impersonate
            );
        }
        if account.credential_token().is_some() {
            account.republish();
        }
        Ok(account)
    }

    /// `Some` only for plan mode — api-key mode has nothing to refresh.
    async fn build_store(
        config: &AccountConfig,
        http: crate::transport::engine::Client,
    ) -> Result<Option<TokenStore>> {
        if config.mode == AccountMode::ApiKey {
            return Ok(None);
        }
        let credentials = Self::load_credentials(config).await?;
        let persist_path = config
            .should_persist()
            .then(|| config.auth_json_path())
            .flatten()
            .map(expand_tilde);
        let impersonation = config
            .impersonate
            .as_deref()
            .and_then(Impersonation::parse)
            .unwrap_or_default();
        Ok(Some(
            TokenStore::new(credentials, http, config.family)
                .with_persist_path(persist_path)
                .with_impersonation(impersonation),
        ))
    }

    /// Load the initial credential set for plan mode.
    ///
    /// Inline configuration wins over the `auth.json` file when both are present,
    /// including `account_id`.
    async fn load_credentials(config: &AccountConfig) -> Result<Credentials> {
        let mut credentials = match config.auth_json_path() {
            Some(raw_path) => {
                let path = expand_tilde(raw_path);
                if !path.exists() {
                    return Err(PlanxError::config(format!(
                        "account '{}': auth_json not found at {}",
                        config.name,
                        path.display()
                    )));
                }
                Credentials::from_file(&path).await?
            }
            None => Credentials::default(),
        };
        if let Some(token) = filled(&config.refresh_token) {
            credentials.refresh_token = Some(token.to_string());
        }
        if let Some(token) = filled(&config.access_token) {
            credentials.access_token = Some(token.to_string());
        }
        if let Some(id) = filled(&config.account_id) {
            credentials.account_id = Some(id.to_string());
        }
        Ok(credentials)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The configuration this account was built from.
    pub fn config(&self) -> &AccountConfig {
        &self.config
    }

    pub fn family(&self) -> AccountFamily {
        self.family
    }

    pub fn mode(&self) -> AccountMode {
        self.mode
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn impersonation(&self) -> Impersonation {
        self.impersonate
    }

    /// A newer version the local Codex CLI reported, when we are behind it.
    pub fn stale_cli_version(&self) -> Option<&str> {
        self.stale_cli_version.as_deref()
    }

    pub fn store(&self) -> Option<&TokenStore> {
        self.store.as_ref()
    }

    /// Whether a credential is currently published.
    pub fn is_ready(&self) -> bool {
        self.current_auth().is_some()
    }

    /// Whether this account can be refreshed after a 401.
    pub fn is_refreshable(&self) -> bool {
        self.mode == AccountMode::Plan && self.store.is_some()
    }

    /// The current access token (plan mode) or static key (api-key mode).
    fn credential_token(&self) -> Option<String> {
        match self.mode {
            AccountMode::ApiKey => filled(&self.static_api_key).map(str::to_string),
            AccountMode::Plan => self.store.as_ref()?.try_snapshot()?.access_token.clone(),
        }
    }

    /// Build the header set for this account's family and mode.
    fn build_auth(
        &self,
        api_key: Option<&str>,
        access_token: Option<&str>,
        account_id: Option<&str>,
    ) -> UpstreamAuth {
        let mut pairs: Vec<(String, String)> = Vec::new();
        match self.mode {
            AccountMode::ApiKey => {
                let Some(key) = api_key else {
                    return UpstreamAuth::Headers {
                        set: pairs,
                        append: Vec::new(),
                    };
                };
                match self.family {
                    // Anthropic API keys belong in `x-api-key`; sending them as a
                    // Bearer token is rejected.
                    AccountFamily::Claude => pairs.push(("x-api-key".to_string(), key.to_string())),
                    // Legacy Codex rule, shared with `UpstreamAuth::header_pairs`.
                    AccountFamily::Gpt => {
                        pairs = UpstreamAuth::Static(key.to_string()).header_pairs();
                    }
                }
            }
            AccountMode::Plan => {
                if let Some(token) = access_token {
                    pairs.push(("authorization".to_string(), format!("Bearer {token}")));
                }
                if let UpstreamAuth::Headers { set, append } =
                    self.identity
                        .headers(self.family, &self.account_seed, account_id)
                {
                    pairs.extend(set);
                    return UpstreamAuth::Headers { set: pairs, append };
                }
            }
        }
        UpstreamAuth::Headers {
            set: pairs,
            append: Vec::new(),
        }
    }

    /// Republish the sync snapshot from the current credential.
    ///
    /// Returns the published value, or `None` when there is no usable
    /// credential (the relay then injects nothing).
    fn republish(&self) -> Option<UpstreamAuth> {
        let auth = match self.mode {
            AccountMode::ApiKey => self
                .credential_token()
                .map(|key| self.build_auth(Some(&key), None, None)),
            AccountMode::Plan => {
                let credentials = self.store.as_ref()?.try_snapshot()?;
                let token = credentials.access_token.clone()?;
                let account_id = credentials.effective_account_id();
                Some(self.build_auth(None, Some(&token), account_id.as_deref()))
            }
        };
        if let Ok(mut guard) = self.current.write() {
            *guard = auth.clone();
        }
        auth
    }

    /// Add the account name to errors that the credential store cannot know.
    fn account_error(&self, error: PlanxError) -> PlanxError {
        match error {
            PlanxError::MissingRefreshToken => PlanxError::config(format!(
                "account '{}' has no refresh_token to refresh with",
                self.name
            )),
            other => other,
        }
    }

    /// Refresh the credential if it is missing or about to expire.
    ///
    /// Called at startup and by the background refresher — never on the relay's
    /// hot path. Returns whether a network refresh actually happened.
    pub async fn ensure_fresh(&self) -> Result<bool> {
        let Some(store) = self.store.as_ref() else {
            return Ok(false);
        };
        let refreshable = store
            .try_snapshot()
            .and_then(|creds| creds.refresh_token)
            .is_some();
        let refreshed = if refreshable && store.needs_refresh().await {
            store
                .refresh(false)
                .await
                .map_err(|e| self.account_error(e))?;
            true
        } else {
            false
        };
        self.republish();
        Ok(refreshed)
    }

    /// Force a refresh (used after an upstream 401) and republish.
    pub async fn refresh_now(&self) -> Result<UpstreamAuth> {
        let store = self.store.as_ref().ok_or_else(|| {
            PlanxError::config(format!(
                "account '{}' uses an api_key and cannot be refreshed",
                self.name
            ))
        })?;
        store
            .refresh(true)
            .await
            .map_err(|e| self.account_error(e))?;
        self.republish().ok_or(PlanxError::RefreshNoAccessToken)
    }

    /// Currently published auth (sync, cheap).
    pub fn current_auth(&self) -> Option<UpstreamAuth> {
        self.current.read().ok().and_then(|guard| guard.clone())
    }

    /// Last cached quota, if a probe has run.
    pub fn cached_quota(&self) -> Option<AccountQuota> {
        self.quota.read().ok().and_then(|guard| guard.clone())
    }

    /// Probe this account's quota (zero-spend) and cache the result.
    ///
    /// Probing reuses the *published* auth, so the probe carries exactly the
    /// credential and identity headers a proxied request would — there is no
    /// second header-building path to drift.
    ///
    /// An `api_key` account has no subscription quota to read, so it reports
    /// that as data without spending a request the upstream would reject.
    pub async fn probe_quota(&self, urls: &ProbeUrls) -> AccountQuota {
        let quota = if self.mode == AccountMode::ApiKey {
            AccountQuota::failed(
                self.family,
                format!(
                    "account '{}' uses an api_key, which carries no subscription quota",
                    self.name
                ),
            )
        } else {
            match self.current_auth() {
                Some(auth) => {
                    probe::probe(ProbeRequest {
                        client: &self.http,
                        family: self.family,
                        url: urls.for_family(self.family),
                        auth: &auth,
                        impersonation: self.impersonate,
                    })
                    .await
                }
                None => AccountQuota::failed(
                    self.family,
                    format!("account '{}' has no usable credential to probe", self.name),
                ),
            }
        };
        if let Ok(mut guard) = self.quota.write() {
            *guard = Some(quota.clone());
        }
        quota
    }
}

/// Trimmed non-empty value.
fn filled(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// All configured accounts.
///
/// Implements [`PlanAuthProvider`] so `proxy-relay` can consume it without
/// depending on this crate.
#[derive(Debug)]
pub struct PlanxRegistry {
    /// Reloadable in place so the relay's `Arc<dyn PlanAuthProvider>` handle
    /// stays valid across config edits — no handle swap, no request gap.
    accounts: RwLock<HashMap<String, Arc<PlanAccount>>>,
    /// Remembered so [`PlanxRegistry::reload`] can rebuild accounts.
    proxy: RwLock<Option<String>>,
    urls: ProbeUrls,
}

/// Outcome of a [`PlanxRegistry::reload`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// Accounts newly created.
    pub added: usize,
    /// Accounts rebuilt because their configuration changed.
    pub updated: usize,
    /// Accounts whose configuration was unchanged (credential state preserved).
    pub kept: usize,
    /// Accounts dropped because they are no longer configured.
    pub removed: usize,
}

impl ReloadReport {
    /// True when nothing changed.
    pub fn is_noop(&self) -> bool {
        self.added == 0 && self.updated == 0 && self.removed == 0
    }
}

impl std::fmt::Display for ReloadReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} added, {} updated, {} kept, {} removed",
            self.added, self.updated, self.kept, self.removed
        )
    }
}

impl PlanxRegistry {
    /// Build every configured account.
    ///
    /// One broken account must not take the whole proxy down: failures are logged
    /// and that account is skipped. A registry with zero accounts is still a
    /// valid registry — that is what lets the dashboard add the *first* account
    /// to a running server.
    pub async fn from_config(
        configs: &[AccountConfig],
        proxy: Option<&str>,
        urls: ProbeUrls,
    ) -> Result<Self> {
        let registry = Self {
            accounts: RwLock::new(HashMap::new()),
            proxy: RwLock::new(proxy.map(str::to_string)),
            urls,
        };
        registry.reload(configs).await;
        Ok(registry)
    }

    /// Apply a new account configuration set.
    ///
    /// Accounts whose configuration is equal keep their existing
    /// `PlanAccount` — and therefore their cached credential, refresh state and
    /// quota snapshot. Everything else is rebuilt. This is what makes a
    /// dashboard edit take effect without a restart and without disturbing
    /// accounts the operator did not touch.
    pub async fn reload(&self, configs: &[AccountConfig]) -> ReloadReport {
        let proxy = self.proxy.read().ok().and_then(|guard| guard.clone());
        let previous = self.snapshot();

        let mut report = ReloadReport::default();
        let mut next: HashMap<String, Arc<PlanAccount>> = HashMap::new();

        for config in configs {
            let name = config.name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            if let Some(existing) = previous.get(&name) {
                if existing.config() == config {
                    next.insert(name, existing.clone());
                    report.kept += 1;
                    continue;
                }
                report.updated += 1;
            } else {
                report.added += 1;
            }
            match PlanAccount::from_config(config, proxy.as_deref()).await {
                Ok(account) => {
                    next.insert(name, Arc::new(account));
                }
                Err(error) => {
                    // Keep serving the previous credential if there was one.
                    tracing::error!("[planx] account '{}' reload failed: {}", name, error);
                    if let Some(existing) = previous.get(&name) {
                        next.insert(name, existing.clone());
                    }
                }
            }
        }

        for name in previous.keys() {
            if !next.contains_key(name) {
                report.removed += 1;
            }
        }

        if let Ok(mut guard) = self.accounts.write() {
            *guard = next;
        }
        if !report.is_noop() {
            tracing::info!("[planx] accounts reloaded: {report}");
        }
        report
    }

    /// Cheap snapshot of the account map (Arc clones only).
    fn snapshot(&self) -> HashMap<String, Arc<PlanAccount>> {
        self.accounts
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.accounts
            .read()
            .map(|guard| guard.len())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up an account by name.
    pub fn account(&self, name: &str) -> Option<Arc<PlanAccount>> {
        self.accounts
            .read()
            .ok()
            .and_then(|guard| guard.get(name).cloned())
    }

    /// All accounts, sorted by name for stable output.
    pub fn accounts(&self) -> Vec<Arc<PlanAccount>> {
        let mut accounts: Vec<Arc<PlanAccount>> = self.snapshot().into_values().collect();
        accounts.sort_by(|a, b| a.name().cmp(b.name()));
        accounts
    }

    /// All account names, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .accounts
            .read()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Probe every account's quota and cache the results.
    pub async fn probe_all(&self) -> Vec<(String, AccountQuota)> {
        let mut results = Vec::new();
        for account in self.accounts() {
            let quota = account.probe_quota(&self.urls).await;
            results.push((account.name().to_string(), quota));
        }
        results
    }

    /// Probe one account's quota, or `None` when no such account is live.
    pub async fn probe_one(&self, name: &str) -> Option<AccountQuota> {
        let account = self.account(name)?;
        Some(account.probe_quota(&self.urls).await)
    }

    /// Refresh every refreshable account whose credential needs it, returning how
    /// many were actually refreshed.
    pub async fn refresh_due(&self) -> usize {
        let mut refreshed = 0;
        for account in self.snapshot().values() {
            match account.ensure_fresh().await {
                Ok(true) => refreshed += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    "[planx] background refresh of '{}' failed: {}",
                    account.name(),
                    error
                ),
            }
        }
        refreshed
    }
}

impl PlanAuthProvider for PlanxRegistry {
    fn has_account(&self, name: &str) -> bool {
        self.account(name).is_some()
    }

    fn resolve(&self, name: &str) -> Option<UpstreamAuth> {
        self.account(name)?.current_auth()
    }

    fn force_refresh<'a>(&'a self, name: &'a str) -> PlanAuthFuture<'a> {
        Box::pin(async move {
            let account = self.account(name)?;
            match account.refresh_now().await {
                Ok(auth) => {
                    tracing::info!("[planx] account '{name}' refreshed after upstream rejection");
                    Some(auth)
                }
                Err(error) => {
                    tracing::warn!("[planx] account '{name}' refresh failed: {error}");
                    None
                }
            }
        })
    }
}

/// Spawn the background refresher.
///
/// Returns immediately; the task lives until the runtime shuts down. Refreshes
/// happen on a fixed tick; an upstream 401 is handled in-request by the relay,
/// which calls [`PlanAuthProvider::force_refresh`].
pub fn spawn_refresher(
    registry: Arc<PlanxRegistry>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    let period = std::time::Duration::from_secs(interval_secs.max(30));
    tokio::spawn(async move {
        // Fill any account that only had a refresh token at startup.
        let refreshed = registry.refresh_due().await;
        if refreshed > 0 {
            tracing::info!(
                "[planx] initial refresh completed for {refreshed} of {} account(s)",
                registry.len()
            );
        }
        loop {
            tokio::time::sleep(period).await;
            registry.refresh_due().await;
        }
    })
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

    fn test_client() -> crate::transport::engine::Client {
        crate::transport::engine::Client::new()
    }

    fn plan_account(name: &str, family: AccountFamily, token: &str) -> PlanAccount {
        let id = jws(serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "ws-1",
                "chatgpt_plan_type": "plus"
            }
        }));
        let creds = Credentials {
            access_token: Some(token.to_string()),
            refresh_token: Some("rt-1".into()),
            id_token: Some(id),
            account_id: Some("ws-1".into()),
            ..Default::default()
        };
        PlanAccount {
            config: AccountConfig {
                name: name.to_string(),
                family,
                mode: AccountMode::Plan,
                access_token: Some(token.to_string()),
                ..Default::default()
            },
            name: name.to_string(),
            family,
            mode: AccountMode::Plan,
            identity: Identity::from_profile_name(None, family),
            account_seed: name.to_string(),
            http: test_client(),
            store: Some(TokenStore::new(creds, test_client(), family)),
            static_api_key: None,
            impersonate: Impersonation::Off,
            current: RwLock::new(None),
            quota: RwLock::new(None),
            stale_cli_version: None,
        }
    }

    fn api_key_account(name: &str, family: AccountFamily, key: &str) -> PlanAccount {
        let account = PlanAccount {
            config: AccountConfig {
                name: name.to_string(),
                family,
                mode: AccountMode::ApiKey,
                api_key: Some(key.to_string()),
                ..Default::default()
            },
            name: name.to_string(),
            family,
            mode: AccountMode::ApiKey,
            identity: Identity::from_profile_name(None, family),
            account_seed: name.to_string(),
            http: test_client(),
            store: None,
            static_api_key: Some(key.to_string()),
            impersonate: Impersonation::Off,
            current: RwLock::new(None),
            quota: RwLock::new(None),
            stale_cli_version: None,
        };
        account.republish();
        account
    }

    fn registry_with(accounts: Vec<(String, Arc<PlanAccount>)>) -> PlanxRegistry {
        PlanxRegistry {
            accounts: RwLock::new(accounts.into_iter().collect()),
            proxy: RwLock::new(None),
            urls: ProbeUrls::default(),
        }
    }

    /// Account URLs that cannot be reached, to prove a probe did not call out.
    fn offshore_urls() -> ProbeUrls {
        ProbeUrls {
            gpt: "http://127.0.0.1:1/usage".to_string(),
            claude: "http://127.0.0.1:1/usage".to_string(),
        }
    }

    type HeaderLists = (Vec<(String, String)>, Vec<(String, String)>);

    fn headers_of(auth: &UpstreamAuth) -> HeaderLists {
        match auth {
            UpstreamAuth::Headers { set, append } => (set.clone(), append.clone()),
            other => panic!("expected header auth, got {other:?}"),
        }
    }

    fn find<'a>(set: &'a [(String, String)], name: &str) -> Option<&'a str> {
        set.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // ── family × mode matrix ──

    #[tokio::test]
    async fn gpt_api_key_uses_bearer_for_sk_prefix() {
        let account = api_key_account("k", AccountFamily::Gpt, "sk-abc");
        let (set, append) = headers_of(&account.current_auth().unwrap());
        assert_eq!(find(&set, "authorization"), Some("Bearer sk-abc"));
        assert!(append.is_empty());
    }

    #[tokio::test]
    async fn gpt_api_key_uses_x_api_key_otherwise() {
        let account = api_key_account("k", AccountFamily::Gpt, "plain-token");
        let (set, _) = headers_of(&account.current_auth().unwrap());
        assert_eq!(find(&set, "x-api-key"), Some("plain-token"));
        assert_eq!(find(&set, "authorization"), None);
    }

    #[tokio::test]
    async fn claude_api_key_always_uses_x_api_key() {
        // Even with an `sk-` prefix, Anthropic wants `x-api-key`.
        let account = api_key_account("k", AccountFamily::Claude, "sk-ant-api03-x");
        let (set, _) = headers_of(&account.current_auth().unwrap());
        assert_eq!(find(&set, "x-api-key"), Some("sk-ant-api03-x"));
        assert_eq!(find(&set, "authorization"), None);
    }

    #[tokio::test]
    async fn gpt_plan_adds_codex_identity() {
        let account = plan_account("gpt", AccountFamily::Gpt, "at-1");
        account.ensure_fresh().await.unwrap();
        let (set, append) = headers_of(&account.current_auth().unwrap());
        assert_eq!(find(&set, "authorization"), Some("Bearer at-1"));
        assert_eq!(find(&set, "originator"), Some("codex-tui"));
        assert!(find(&set, "user-agent").unwrap().starts_with("codex-tui/"));
        assert!(append.is_empty(), "codex does not need merged betas");
    }

    #[tokio::test]
    async fn claude_plan_adds_claude_identity_and_merged_oauth_beta() {
        let account = plan_account("claude", AccountFamily::Claude, "sk-ant-oat01-x");
        account.ensure_fresh().await.unwrap();
        let (set, append) = headers_of(&account.current_auth().unwrap());

        assert_eq!(find(&set, "authorization"), Some("Bearer sk-ant-oat01-x"));
        assert_eq!(find(&set, "x-app"), Some("cli"));
        assert_eq!(
            find(&set, "user-agent"),
            Some("claude-cli/2.1.258 (external, cli)")
        );
        assert_eq!(find(&set, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(find(&set, "x-api-key"), None);
        assert_eq!(
            append,
            vec![(
                "anthropic-beta".to_string(),
                crate::identity::CLAUDE_OAUTH_BETA.to_string()
            )]
        );
    }

    #[test]
    fn api_key_mode_shares_the_legacy_placement_rule() {
        // The `sk-` rule lives in `UpstreamAuth::header_pairs`; the api-key path
        // must not grow a second copy that can drift.
        let account = api_key_account("k", AccountFamily::Gpt, "sk-shared");
        let auth = account.current_auth().unwrap();
        assert_eq!(
            auth.header_pairs(),
            UpstreamAuth::Static("sk-shared".into()).header_pairs()
        );
    }

    // ── registry behaviour ──

    #[tokio::test]
    async fn registry_skips_accounts_without_credentials() {
        let registry = PlanxRegistry::from_config(
            &[AccountConfig {
                name: "broken".into(),
                ..Default::default()
            }],
            None,
            ProbeUrls::default(),
        )
        .await
        .unwrap();
        assert!(
            registry.is_empty(),
            "credential-less account must be skipped"
        );
        assert!(!registry.has_account("broken"));
    }

    #[tokio::test]
    async fn an_empty_registry_is_valid_so_the_first_account_can_be_added_later() {
        let registry = PlanxRegistry::from_config(&[], None, ProbeUrls::default())
            .await
            .unwrap();
        assert!(registry.is_empty());
        let report = registry
            .reload(&[AccountConfig {
                name: "claude-key".into(),
                family: AccountFamily::Claude,
                mode: AccountMode::ApiKey,
                api_key: Some("sk-ant-api03-x".into()),
                ..Default::default()
            }])
            .await;
        assert_eq!(report.added, 1);
        assert!(registry.resolve("claude-key").is_some());
    }

    #[tokio::test]
    async fn registry_builds_an_api_key_account_without_network() {
        let registry = PlanxRegistry::from_config(
            &[AccountConfig {
                name: "claude-key".into(),
                family: AccountFamily::Claude,
                mode: AccountMode::ApiKey,
                api_key: Some("sk-ant-api03-x".into()),
                ..Default::default()
            }],
            None,
            ProbeUrls::default(),
        )
        .await
        .unwrap();
        assert_eq!(registry.len(), 1);
        let auth = registry
            .resolve("claude-key")
            .expect("published immediately");
        let (set, _) = headers_of(&auth);
        assert_eq!(find(&set, "x-api-key"), Some("sk-ant-api03-x"));
    }

    #[tokio::test]
    async fn account_names_are_trimmed_on_registration() {
        let registry = PlanxRegistry::from_config(
            &[AccountConfig {
                name: "  padded  ".into(),
                family: AccountFamily::Gpt,
                mode: AccountMode::ApiKey,
                api_key: Some("sk-1".into()),
                ..Default::default()
            }],
            None,
            ProbeUrls::default(),
        )
        .await
        .unwrap();
        assert_eq!(registry.names(), vec!["padded"]);
        assert!(registry.resolve("padded").is_some());
    }

    #[tokio::test]
    async fn sync_resolve_reads_the_published_snapshot() {
        let account = Arc::new(plan_account("work", AccountFamily::Gpt, "at-2"));
        account.ensure_fresh().await.unwrap();
        let registry = registry_with(vec![("work".to_string(), account)]);

        let auth = registry.resolve("work").expect("published auth");
        let (set, _) = headers_of(&auth);
        assert_eq!(find(&set, "authorization"), Some("Bearer at-2"));
        assert!(registry.resolve("missing").is_none());
        assert_eq!(registry.names(), vec!["work"]);
    }

    #[tokio::test]
    async fn resolve_before_publish_is_none_not_panic() {
        let account = plan_account("work", AccountFamily::Gpt, "at-3");
        assert!(account.current_auth().is_none(), "never published yet");
        let registry = registry_with(vec![("work".to_string(), Arc::new(account))]);
        assert!(registry.resolve("work").is_none());
    }

    #[tokio::test]
    async fn api_key_accounts_are_not_refreshable() {
        let account = api_key_account("k", AccountFamily::Gpt, "sk-1");
        assert!(!account.is_refreshable());
        let error = account.refresh_now().await.unwrap_err().to_string();
        assert!(error.contains("api_key"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_refresh_token_names_the_account() {
        let mut creds = Credentials {
            access_token: Some("at".into()),
            ..Default::default()
        };
        creds.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
        let account = PlanAccount {
            config: AccountConfig {
                name: "work".into(),
                mode: AccountMode::Plan,
                ..Default::default()
            },
            name: "work".into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            identity: Identity::from_profile_name(None, AccountFamily::Gpt),
            account_seed: "work".into(),
            http: test_client(),
            store: Some(TokenStore::new(creds, test_client(), AccountFamily::Gpt)),
            static_api_key: None,
            impersonate: Impersonation::Off,
            current: RwLock::new(None),
            quota: RwLock::new(None),
            stale_cli_version: None,
        };
        // `refresh_now` is the path the relay's 401 retry takes.
        let error = account.refresh_now().await.unwrap_err().to_string();
        assert!(error.contains("work"), "the account must be named: {error}");
        assert!(error.contains("refresh_token"), "{error}");
    }

    // ── live reload ──

    fn gpt_plan_config(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.into(),
            family: AccountFamily::Gpt,
            mode: AccountMode::Plan,
            access_token: Some(format!("at-{name}")),
            refresh_token: Some("rt".into()),
            ..Default::default()
        }
    }

    fn empty_registry() -> PlanxRegistry {
        registry_with(Vec::new())
    }

    #[tokio::test]
    async fn reload_adds_and_removes_accounts() {
        let registry = empty_registry();
        assert!(registry.is_empty());

        let report = registry.reload(&[gpt_plan_config("a")]).await;
        assert_eq!(report.added, 1);
        assert_eq!(registry.len(), 1);
        assert!(registry.has_account("a"));

        let report = registry.reload(&[]).await;
        assert_eq!(report.removed, 1);
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn reload_keeps_unchanged_accounts_identity() {
        let registry = empty_registry();
        registry.reload(&[gpt_plan_config("a")]).await;
        let first = registry.account("a").unwrap();

        let report = registry.reload(&[gpt_plan_config("a")]).await;
        assert_eq!(report.kept, 1);
        assert!(report.is_noop(), "nothing actually changed");
        let second = registry.account("a").unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged account must keep its cached credential and quota"
        );
    }

    #[tokio::test]
    async fn reload_rebuilds_a_changed_account() {
        let registry = empty_registry();
        registry.reload(&[gpt_plan_config("a")]).await;
        let first = registry.account("a").unwrap();

        let mut changed = gpt_plan_config("a");
        changed.access_token = Some("at-rotated".to_string());
        let report = registry.reload(&[changed]).await;

        assert_eq!(report.updated, 1);
        let second = registry.account("a").unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "changed config must rebuild");
        let auth = registry.resolve("a").unwrap();
        match auth {
            UpstreamAuth::Headers { set, .. } => assert!(set
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer at-rotated")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_reports_a_mixed_change_set() {
        let registry = empty_registry();
        registry
            .reload(&[gpt_plan_config("keep"), gpt_plan_config("drop")])
            .await;

        let report = registry
            .reload(&[gpt_plan_config("keep"), gpt_plan_config("new")])
            .await;
        assert_eq!(report.kept, 1);
        assert_eq!(report.added, 1);
        assert_eq!(report.removed, 1);
        assert!(!report.is_noop());
        assert!(!report.to_string().is_empty());
    }

    #[tokio::test]
    async fn a_broken_reload_keeps_the_previous_credential() {
        // An operator typo must not take a working account offline.
        let registry = empty_registry();
        registry.reload(&[gpt_plan_config("a")]).await;
        let before = registry.account("a").unwrap();

        let mut broken = gpt_plan_config("a");
        broken.access_token = None;
        broken.refresh_token = None;
        registry.reload(&[broken]).await;

        let after = registry.account("a").expect("previous kept");
        assert!(Arc::ptr_eq(&before, &after));
        assert!(registry.resolve("a").is_some());
    }

    #[tokio::test]
    async fn reload_ignores_empty_names() {
        let registry = empty_registry();
        let report = registry.reload(&[gpt_plan_config("  ")]).await;
        assert!(report.is_noop());
        assert!(registry.is_empty());
    }

    // ── quota probing ──

    #[tokio::test]
    async fn an_api_key_account_probes_as_data_without_a_request() {
        // Pointing at an unreachable port: if a request were attempted the error
        // would be a transport failure, not the api_key explanation.
        let account = api_key_account("k", AccountFamily::Gpt, "sk-1");
        let quota = account.probe_quota(&offshore_urls()).await;
        let error = quota.error.expect("api_key accounts report as data");
        assert!(error.contains("api_key"), "{error}");
        assert!(quota.windows.is_empty());
    }

    #[tokio::test]
    async fn probe_one_reports_an_unknown_account_as_none() {
        let registry = empty_registry();
        assert!(registry.probe_one("ghost").await.is_none());
    }

    #[tokio::test]
    async fn probe_all_covers_every_account() {
        let registry = empty_registry();
        registry
            .reload(&[gpt_plan_config("a"), gpt_plan_config("b")])
            .await;
        let results = registry.probe_all().await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[1].0, "b");
    }

    #[tokio::test]
    async fn names_and_accounts_are_sorted() {
        let registry = empty_registry();
        registry
            .reload(&[gpt_plan_config("zeta"), gpt_plan_config("alpha")])
            .await;
        assert_eq!(registry.names(), vec!["alpha", "zeta"]);
        let accounts = registry.accounts();
        let names: Vec<&str> = accounts.iter().map(|a| a.name()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
    }
}
