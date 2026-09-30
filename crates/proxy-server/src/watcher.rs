//! Config hot-reload watcher.
//!
//! Two triggers converge on the same reload path:
//!  1. an API edit (the dashboard) emits `UpstreamChanged` on the event bus;
//!  2. a hand-edit of `config.toml` changes the file mtime.
//!
//! Without (2) a manual edit would silently require a restart — that was a real
//! gap before this module existed: nothing in the codebase called
//! `ConfigStore::reload()`.
//!
//! The polling half is deliberately a plain struct with an explicit `poll_once`
//! so it can be tested without a running server.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use proxy_common::{ConfigStore, EventBus, WsMessage};
use proxy_planx::{PlanxRegistry, ReloadReport};
use tokio::sync::broadcast::error::RecvError;

/// How often the config file mtime is checked for hand-edits.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Outcome of one poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// The file is unchanged; nothing happened.
    Unchanged,
    /// The file changed and the config was reloaded successfully.
    Reloaded {
        /// What happened to the planx accounts during the same reload.
        accounts: ReloadReport,
    },
    /// The file changed but the new contents were rejected; the previous config
    /// is still in effect.
    Rejected(String),
}

impl PollOutcome {
    /// True when a reload actually took effect.
    #[cfg(test)]
    pub fn changed_something(&self) -> bool {
        matches!(self, PollOutcome::Reloaded { .. })
    }
}

/// Watches `config.toml` and keeps the planx registry in sync.
pub struct ConfigWatcher {
    config: ConfigStore,
    /// Always present: an account added while the server runs has to land
    /// somewhere.
    planx: Arc<PlanxRegistry>,
    path: PathBuf,
    /// Last observed `(mtime, len)`. The length guards against a write that
    /// lands with a byte-identical mtime (coarse filesystem timestamps, `mv`,
    /// `cp -p`).
    last_seen: Option<(SystemTime, u64)>,
}

impl ConfigWatcher {
    /// Build a watcher and record the current file state as the baseline.
    pub async fn new(config: ConfigStore, planx: Arc<PlanxRegistry>, path: PathBuf) -> Self {
        let last_seen = file_stamp(&path).await;
        Self {
            config,
            planx,
            path,
            last_seen,
        }
    }

    /// Reload accounts from the in-memory config (the API-edit trigger).
    pub async fn apply_accounts(&self) -> ReloadReport {
        let snapshot = self.config.get().await;
        let report = self.planx.reload(&snapshot.proxy.accounts).await;
        if !report.is_noop() {
            tracing::info!("[planx] accounts reloaded: {report}");
        }
        report
    }

    /// Check the file for changes and reload when it moved.
    ///
    /// A malformed hand-edit is reported and ignored: `ConfigStore::reload`
    /// validates before swapping, so the running config survives.
    pub async fn poll_once(&mut self) -> PollOutcome {
        let stamp = file_stamp(&self.path).await;
        if stamp == self.last_seen {
            return PollOutcome::Unchanged;
        }
        self.last_seen = stamp;

        match self.config.reload().await {
            Ok(_) => {
                tracing::info!("[config] reloaded from disk ({})", self.path.display());
                let accounts = self.apply_accounts().await;
                PollOutcome::Reloaded { accounts }
            }
            Err(error) => {
                tracing::error!("[config] disk reload rejected, keeping previous: {error}");
                PollOutcome::Rejected(error.to_string())
            }
        }
    }

    /// Run until the runtime shuts down.
    pub async fn run(mut self, events: EventBus) {
        let mut receiver = events.subscribe();
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // The first tick completes immediately; skip it so startup does not
        // count as a change.
        interval.tick().await;

        loop {
            tokio::select! {
                message = receiver.recv() => match message {
                    Ok(WsMessage::UpstreamChanged { .. }) => {
                        self.apply_accounts().await;
                    }
                    // A lagged receiver only means intermediate notifications
                    // were dropped; the next one still triggers a reload.
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    // The bus is gone, so `recv()` would return `Closed`
                    // immediately forever and spin this loop.
                    Err(RecvError::Closed) => return,
                },
                _ = interval.tick() => {
                    self.poll_once().await;
                }
            }
        }
    }
}

/// Current `(mtime, length)` of `path`, or `None` when it is unreadable.
pub async fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Spawn the watcher as a background task.
pub fn spawn(config: ConfigStore, events: EventBus, planx: Arc<PlanxRegistry>, path: PathBuf) {
    tokio::spawn(async move {
        ConfigWatcher::new(config, planx, path)
            .await
            .run(events)
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_common::PlanAuthProvider;
    use std::io::Write;

    /// Minimal config that passes validation: one codex provider bound to one
    /// GPT api-key account.
    fn minimal_config(api_key: &str) -> String {
        format!("{}\n{}", header_config(), account_config(api_key))
    }

    /// The same file with no account and no account reference.
    fn config_without_accounts() -> String {
        format!("{}\n{}", header_config(), provider_config(None))
    }

    fn header_config() -> &'static str {
        r#"
[logging]
level = "info"

[server]
listen_address = "127.0.0.1"
http_port = 39901
proxy_port = 39902

[proxy]
active_upstream = "local"
active_codex_upstream = "local"
active_proxy_upstream = "local"

[[model_pricing]]
id = "gpt-5"
price = [3.0, 15.0]
[model_pricing.providers]
local = ["gpt-5"]
"#
    }

    fn account_config(api_key: &str) -> String {
        format!(
            r#"
[[proxy.accounts]]
name    = "acct"
family  = "gpt"
mode    = "api_key"
api_key = "{api_key}"
"#
        ) + &provider_config(Some("acct"))
    }

    fn provider_config(account: Option<&str>) -> String {
        let account = match account {
            Some(name) => format!("account   = \"{name}\"\n"),
            None => String::new(),
        };
        format!(
            r#"
[[proxy.providers]]
name      = "local"
url       = "https://chatgpt.com/backend-api/codex"
protocols = ["codex"]
{account}
[[proxy.upstreams]]
name    = "local"
default = {{ provider = "local", model = "gpt-5" }}
"#
        )
    }

    /// A temp config file inside the workspace (ephemeral dirs are not reliable).
    struct TempConfig {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempConfig {
        async fn new(name: &str, contents: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cc-proxy-watcher-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            tokio::fs::create_dir_all(&dir).await.unwrap();
            let path = dir.join("config.toml");
            tokio::fs::write(&path, contents).await.unwrap();
            Self { dir, path }
        }

        /// Rewrite the file, guaranteeing a distinct mtime.
        async fn write(&self, contents: &str) {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            let mut file = std::fs::File::create(&self.path).unwrap();
            file.write_all(contents.as_bytes()).unwrap();
            file.sync_all().unwrap();
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn store_for(temp: &TempConfig) -> ConfigStore {
        ConfigStore::open(&temp.path).await.unwrap()
    }

    /// An empty registry, which is what a server with no accounts starts with.
    async fn empty_registry() -> Arc<PlanxRegistry> {
        Arc::new(
            PlanxRegistry::from_config(&[], None, proxy_planx::ProbeUrls::default())
                .await
                .unwrap(),
        )
    }

    async fn registry_for(store: &ConfigStore) -> Arc<PlanxRegistry> {
        Arc::new(
            PlanxRegistry::from_config(
                &store.get().await.proxy.accounts,
                None,
                proxy_planx::ProbeUrls::default(),
            )
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn an_untouched_file_is_not_reloaded() {
        let temp = TempConfig::new("idle", &minimal_config("sk-1")).await;
        let mut watcher = ConfigWatcher::new(
            store_for(&temp).await,
            empty_registry().await,
            temp.path.clone(),
        )
        .await;

        assert_eq!(watcher.poll_once().await, PollOutcome::Unchanged);
        assert_eq!(watcher.poll_once().await, PollOutcome::Unchanged);
    }

    #[tokio::test]
    async fn a_hand_edit_is_picked_up() {
        let temp = TempConfig::new("edit", &minimal_config("sk-1")).await;
        let store = store_for(&temp).await;
        let mut watcher =
            ConfigWatcher::new(store.clone(), empty_registry().await, temp.path.clone()).await;

        temp.write(&minimal_config("sk-2")).await;

        let outcome = watcher.poll_once().await;
        assert!(
            outcome.changed_something(),
            "expected a reload, got {outcome:?}"
        );
        let snapshot = store.get().await;
        assert_eq!(snapshot.proxy.accounts[0].api_key.as_deref(), Some("sk-2"));
    }

    #[tokio::test]
    async fn a_rejected_edit_keeps_the_previous_config() {
        let temp = TempConfig::new("bad", &minimal_config("sk-keep")).await;
        let store = store_for(&temp).await;
        let mut watcher =
            ConfigWatcher::new(store.clone(), empty_registry().await, temp.path.clone()).await;

        // Claude + plan + auth_json is rejected by validation.
        let broken = format!(
            "{}\n[[proxy.accounts]]\nname = \"bad\"\nfamily = \"claude\"\nmode = \"plan\"\nauth_json = \"x\"\n",
            minimal_config("sk-keep")
        );
        temp.write(&broken).await;

        match watcher.poll_once().await {
            PollOutcome::Rejected(reason) => {
                assert!(reason.contains("claude"), "unexpected reason: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        let snapshot = store.get().await;
        assert_eq!(
            snapshot.proxy.accounts.len(),
            1,
            "the rejected edit must not take effect"
        );
        assert_eq!(
            snapshot.proxy.accounts[0].api_key.as_deref(),
            Some("sk-keep")
        );
    }

    #[tokio::test]
    async fn unparseable_toml_is_rejected_not_fatal() {
        let temp = TempConfig::new("garbage", &minimal_config("sk-1")).await;
        let store = store_for(&temp).await;
        let mut watcher =
            ConfigWatcher::new(store.clone(), empty_registry().await, temp.path.clone()).await;

        temp.write("this is not toml {{{").await;
        assert!(matches!(
            watcher.poll_once().await,
            PollOutcome::Rejected(_)
        ));
        assert_eq!(store.get().await.proxy.accounts.len(), 1);
    }

    #[tokio::test]
    async fn accounts_are_rebuilt_from_the_new_file() {
        // The full path: disk edit → config reload → planx registry rebuild.
        let temp = TempConfig::new("accounts", &minimal_config("sk-before")).await;
        let store = store_for(&temp).await;

        let registry = registry_for(&store).await;
        assert_eq!(registry.len(), 1);

        let mut watcher =
            ConfigWatcher::new(store.clone(), registry.clone(), temp.path.clone()).await;
        temp.write(&minimal_config("sk-after")).await;

        let outcome = watcher.poll_once().await;
        match outcome {
            PollOutcome::Reloaded { accounts } => {
                assert_eq!(accounts.updated, 1, "the changed account must be rebuilt");
            }
            other => panic!("expected Reloaded, got {other:?}"),
        }
        // The live credential now carries the new key.
        match registry.resolve("acct").expect("published") {
            proxy_common::UpstreamAuth::Headers { set, .. } => assert!(set
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer sk-after")),
            other => panic!("unexpected auth {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unchanged_account_survives_a_reload() {
        let temp = TempConfig::new("keep", &minimal_config("sk-same")).await;
        let store = store_for(&temp).await;
        let registry = registry_for(&store).await;
        let before = registry.account("acct").unwrap();

        let mut watcher =
            ConfigWatcher::new(store.clone(), registry.clone(), temp.path.clone()).await;
        // Touch the file without changing the account.
        temp.write(&format!("{}\n# touched\n", minimal_config("sk-same")))
            .await;

        match watcher.poll_once().await {
            PollOutcome::Reloaded { accounts } => {
                assert_eq!(accounts.kept, 1);
                assert!(accounts.is_noop(), "a comment-only edit must be a no-op");
            }
            other => panic!("expected Reloaded, got {other:?}"),
        }
        let after = registry.account("acct").unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "unchanged account must keep its credential state"
        );
    }

    #[tokio::test]
    async fn the_first_account_can_arrive_while_the_server_runs() {
        // Before the fix the registry was only built when the startup config
        // already had an account, so this reload had nowhere to land.
        let temp = TempConfig::new("first", &config_without_accounts()).await;
        let store = store_for(&temp).await;
        let registry = empty_registry().await;
        assert!(registry.is_empty());

        let mut watcher =
            ConfigWatcher::new(store.clone(), registry.clone(), temp.path.clone()).await;

        temp.write(&minimal_config("sk-fresh")).await;
        assert!(watcher.poll_once().await.changed_something());
        assert_eq!(registry.len(), 1, "the first account came online");
        assert!(registry.resolve("acct").is_some());
    }

    #[tokio::test]
    async fn a_same_mtime_edit_with_a_different_size_is_detected() {
        let temp = TempConfig::new("stamp", &minimal_config("sk-1")).await;
        let store = store_for(&temp).await;
        let mut watcher =
            ConfigWatcher::new(store.clone(), empty_registry().await, temp.path.clone()).await;

        // Rewrite with the same mtime but a longer body: only the length changes.
        let stamp = file_stamp(&temp.path).await.unwrap();
        let longer = format!("{}\n# a longer comment\n", minimal_config("sk-1"));
        tokio::fs::write(&temp.path, &longer).await.unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(&temp.path)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(stamp.0))
            .unwrap();
        drop(file);

        assert!(
            watcher.poll_once().await.changed_something(),
            "a size change must not be missed"
        );
    }
}
