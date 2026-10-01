//! Upstream authentication seam.
//!
//! This module defines **only types and a trait** — no HTTP, no provider logic.
//! `proxy-relay` consumes [`UpstreamAuth`]; `proxy-planx` produces it.
//!
//! # Why the indirection
//!
//! Authentication used to be a single `Option<String>` token resolved in
//! `relay.rs` and injected by `upstream::build_upstream_headers`. A GPT-plan
//! account needs more than a static string: a rotating OAuth access token plus
//! a set of Codex client identity headers. Rather than teaching `proxy-relay`
//! about OAuth, we widen the *value* it carries and let a pluggable mechanism
//! fill it in — mirroring how `SessionIngest` is injected as
//! `Option<Arc<dyn SessionIngest>>` (`proxy-relay/src/relay.rs`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Authentication material for one upstream request.
///
/// Deliberately mechanism-agnostic: the relay only needs to know *which headers
/// to write*, never whether the credential came from an API key or a
/// subscription, nor which provider family it belongs to. Header placement is
/// family-specific (Claude keys go in `x-api-key`, GPT tokens follow the
/// `sk-` rule) and is therefore decided by the producing mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamAuth {
    /// Legacy static token, exactly as before this seam existed:
    /// `sk-` prefix → `Authorization: Bearer`, otherwise `x-api-key`.
    Static(String),
    /// Credential and identity headers produced by an account mechanism
    /// (see `proxy-planx`), for either family and either mode.
    ///
    /// Headers are `(name, value)` pairs rather than `HeaderMap` so that this
    /// crate keeps its dependency set free of `http`.
    Headers {
        /// Headers to write, overwriting any client-supplied value.
        set: Vec<(String, String)>,
        /// Headers to merge into an existing comma-separated value.
        ///
        /// Required for `anthropic-beta`: Claude subscription credentials must
        /// advertise `oauth-2025-04-20`, but a Claude Code client may already
        /// send other betas that must be preserved rather than replaced.
        append: Vec<(String, String)>,
    },
}

impl UpstreamAuth {
    /// Redacted one-line description for logs (never contains a full token).
    pub fn describe(&self) -> String {
        match self {
            UpstreamAuth::Static(_) => "static token".to_string(),
            UpstreamAuth::Headers { set, .. } => {
                let names: Vec<&str> = set.iter().map(|(name, _)| name.as_str()).collect();
                format!("account headers [{}]", names.join(", "))
            }
        }
    }

    /// Flatten to `(name, value)` pairs for a consumer that builds its own
    /// request and has no client-supplied headers to preserve.
    ///
    /// This is the single home of the two composition rules:
    /// [`UpstreamAuth::Static`] applies the legacy `sk-` → `Bearer` / otherwise
    /// `x-api-key` rule, and `append` entries are merged comma-separated into a
    /// matching `set` entry (duplicates skipped).
    ///
    /// The relay cannot use this for `append`: there the value has to be merged
    /// into a header the *client* already sent. See
    /// `proxy_relay::upstream::apply_auth`.
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        match self {
            UpstreamAuth::Static(token) => static_token_pairs(token),
            UpstreamAuth::Headers { set, append } => {
                let mut pairs = set.clone();
                for (name, value) in append {
                    merge_pair(&mut pairs, name, value);
                }
                pairs
            }
        }
    }
}

/// The legacy static-token placement rule, in one place.
fn static_token_pairs(token: &str) -> Vec<(String, String)> {
    if token.starts_with("sk-") {
        vec![("authorization".to_string(), format!("Bearer {token}"))]
    } else {
        vec![("x-api-key".to_string(), token.to_string())]
    }
}

/// Merge `value` into an existing comma-separated pair, skipping duplicates.
fn merge_pair(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    match pairs
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        Some((_, existing)) => {
            if !existing.split(',').any(|part| part.trim() == value) {
                existing.push(',');
                existing.push_str(value);
            }
        }
        None => pairs.push((name.to_string(), value.to_string())),
    }
}

/// Where a **plan connection** talks to.
///
/// A plan is a peer of an upstream — both answer "which upstream do we talk to" —
/// but it is defined by a subscription account rather than by providers and tiers.
/// The relay asks for this through [`PlanAuthProvider`], so it still never learns
/// anything about OAuth: it only learns a base URL and a wire protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEndpoint {
    /// Base URL the protocol's path is appended to.
    pub base_url: String,
    /// Wire protocol used for inference against this endpoint.
    pub protocol: crate::protocol::WireProtocol,
    /// Catalog kind to read the plan's model list with.
    pub models_kind: crate::config::ModelsKind,
}

/// One entry of a plan's model catalog, as the relay needs it.
///
/// Deliberately narrow: the client-facing `/v1/models` only needs an id and an
/// optional display name. The richer admin view (reasoning levels, context
/// windows) stays in `proxy-planx`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The upstream hides it from its own picker (e.g. `codex-auto-review`), so it
    /// is a poor default when substituting an unroutable model name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// Boxed future alias so the trait stays object-safe without `async-trait`.
pub type PlanAuthFuture<'a> = Pin<Box<dyn Future<Output = Option<UpstreamAuth>> + Send + 'a>>;

/// Mechanism interface for subscription-account authentication.
///
/// Implemented by `proxy-planx` and injected by `proxy-server`. Kept
/// **synchronous on the hot path** (`resolve` must not block on the network);
/// the one asynchronous escape hatch is [`PlanAuthProvider::force_refresh`],
/// used by the relay after an upstream 401 to retry in-request.
pub trait PlanAuthProvider: Send + Sync {
    /// Whether an account with this name is configured.
    fn has_account(&self, name: &str) -> bool;

    /// Currently usable auth for this account, or `None` when unknown.
    ///
    /// Must be cheap and must never perform network I/O: implementations keep a
    /// pre-refreshed token in memory, maintained by a background task.
    fn resolve(&self, name: &str) -> Option<UpstreamAuth>;

    /// Force a refresh after an upstream 401 and return the new auth.
    ///
    /// Returns `None` when the account is unknown or the refresh failed.
    fn force_refresh<'a>(&'a self, name: &'a str) -> PlanAuthFuture<'a>;

    /// Where this plan connection points, or `None` when the account is unknown.
    ///
    /// Cheap and synchronous, like [`PlanAuthProvider::resolve`].
    fn endpoint(&self, name: &str) -> Option<PlanEndpoint>;

    /// The plan's own model catalog, for the client-facing `GET /v1/models`.
    ///
    /// Implementations cache; `None` means "could not tell", in which case the
    /// caller falls back to the configured model list rather than failing the
    /// client's request.
    fn models<'a>(&'a self, name: &'a str) -> PlanModelsFuture<'a>;
}

/// Boxed future for [`PlanAuthProvider::models`].
pub type PlanModelsFuture<'a> =
    Pin<Box<dyn Future<Output = Option<Vec<CatalogModel>>> + Send + 'a>>;

/// Injected handle. `None` = no subscription accounts configured.
pub type PlanAuthHandle = Option<Arc<dyn PlanAuthProvider>>;

/// Convenience helpers so call sites read naturally on an `Option`.
pub trait PlanAuthHandleExt {
    /// Resolve auth, or `None` when unconfigured / unknown account.
    fn resolve_account(&self, name: &str) -> Option<UpstreamAuth>;

    /// Force a refresh; resolves to `None` when unconfigured.
    fn refresh_account<'a>(&'a self, name: &'a str) -> PlanAuthFuture<'a>;

    /// Where a plan connection points; `None` when unconfigured or unknown.
    fn plan_endpoint(&self, name: &str) -> Option<PlanEndpoint>;

    /// A plan's own catalog; resolves to `None` when unconfigured or unknown.
    fn plan_models<'a>(&'a self, name: &'a str) -> PlanModelsFuture<'a>;
}

impl PlanAuthHandleExt for PlanAuthHandle {
    fn resolve_account(&self, name: &str) -> Option<UpstreamAuth> {
        self.as_ref().and_then(|provider| provider.resolve(name))
    }

    fn refresh_account<'a>(&'a self, name: &'a str) -> PlanAuthFuture<'a> {
        match self {
            Some(provider) => provider.force_refresh(name),
            None => Box::pin(async { None }),
        }
    }

    fn plan_endpoint(&self, name: &str) -> Option<PlanEndpoint> {
        self.as_ref().and_then(|provider| provider.endpoint(name))
    }

    fn plan_models<'a>(&'a self, name: &'a str) -> PlanModelsFuture<'a> {
        match self {
            Some(provider) => provider.models(name),
            None => Box::pin(async { None }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider;

    impl PlanAuthProvider for FakeProvider {
        fn has_account(&self, name: &str) -> bool {
            name == "work"
        }

        fn resolve(&self, name: &str) -> Option<UpstreamAuth> {
            self.has_account(name).then(|| UpstreamAuth::Headers {
                set: vec![
                    ("authorization".into(), "Bearer at-1".into()),
                    ("originator".into(), "codex-tui".into()),
                ],
                append: vec![],
            })
        }

        fn force_refresh<'a>(&'a self, name: &'a str) -> PlanAuthFuture<'a> {
            Box::pin(async move {
                self.has_account(name).then(|| UpstreamAuth::Headers {
                    set: vec![("authorization".into(), "Bearer at-2".into())],
                    append: vec![],
                })
            })
        }

        fn endpoint(&self, name: &str) -> Option<PlanEndpoint> {
            self.has_account(name).then(|| PlanEndpoint {
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                protocol: crate::protocol::WireProtocol::Codex,
                models_kind: crate::config::ModelsKind::Codex,
            })
        }

        fn models<'a>(&'a self, name: &'a str) -> PlanModelsFuture<'a> {
            Box::pin(async move {
                self.has_account(name).then(|| {
                    vec![CatalogModel {
                        id: "gpt-6.1-sol".into(),
                        display_name: Some("GPT-6.1-Sol".into()),
                        hidden: false,
                    }]
                })
            })
        }
    }

    #[test]
    fn static_variant_is_described_without_the_token() {
        let auth = UpstreamAuth::Static("sk-super-secret".into());
        assert_eq!(auth.describe(), "static token");
        assert!(!auth.describe().contains("secret"));
    }

    #[test]
    fn static_pairs_follow_the_legacy_placement_rule() {
        assert_eq!(
            UpstreamAuth::Static("sk-abc".into()).header_pairs(),
            vec![("authorization".to_string(), "Bearer sk-abc".to_string())]
        );
        assert_eq!(
            UpstreamAuth::Static("plain".into()).header_pairs(),
            vec![("x-api-key".to_string(), "plain".to_string())]
        );
    }

    #[test]
    fn append_pairs_merge_into_an_existing_value_once() {
        let auth = UpstreamAuth::Headers {
            set: vec![("anthropic-beta".into(), "effort-2025-11-24".into())],
            append: vec![
                ("anthropic-beta".into(), "oauth-2025-04-20".into()),
                ("anthropic-beta".into(), "oauth-2025-04-20".into()),
            ],
        };
        assert_eq!(
            auth.header_pairs(),
            vec![(
                "anthropic-beta".to_string(),
                "effort-2025-11-24,oauth-2025-04-20".to_string()
            )]
        );
    }

    #[test]
    fn append_pairs_create_a_missing_header() {
        let auth = UpstreamAuth::Headers {
            set: vec![("authorization".into(), "Bearer at".into())],
            append: vec![("anthropic-beta".into(), "oauth-2025-04-20".into())],
        };
        assert!(auth
            .header_pairs()
            .iter()
            .any(|(name, value)| name == "anthropic-beta" && value == "oauth-2025-04-20"));
    }

    #[test]
    fn headers_variant_lists_names_only() {
        let auth = UpstreamAuth::Headers {
            set: vec![
                ("authorization".into(), "Bearer super-secret".into()),
                ("originator".into(), "codex-tui".into()),
            ],
            append: vec![("anthropic-beta".into(), "oauth-2025-04-20".into())],
        };
        let described = auth.describe();
        assert!(described.contains("authorization"));
        assert!(described.contains("originator"));
        assert!(!described.contains("secret"), "{described}");
    }

    #[test]
    fn unconfigured_handle_resolves_to_none() {
        let handle: PlanAuthHandle = None;
        assert!(handle.resolve_account("work").is_none());
        assert!(handle.as_ref().is_none());
    }

    #[tokio::test]
    async fn handle_delegates_to_provider() {
        let handle: PlanAuthHandle = Some(Arc::new(FakeProvider));
        let resolved = handle.resolve_account("work").expect("resolved");
        match resolved {
            UpstreamAuth::Headers { set, .. } => {
                assert!(set
                    .iter()
                    .any(|(name, value)| name == "authorization" && value == "Bearer at-1"));
            }
            other => panic!("expected account headers, got {other:?}"),
        }
        assert!(handle.resolve_account("missing").is_none());

        let refreshed = handle.refresh_account("work").await.expect("refreshed");
        match refreshed {
            UpstreamAuth::Headers { set, .. } => {
                assert!(set
                    .iter()
                    .any(|(name, value)| name == "authorization" && value == "Bearer at-2"));
            }
            other => panic!("expected account headers, got {other:?}"),
        }
        assert!(handle.refresh_account("missing").await.is_none());
    }

    #[tokio::test]
    async fn unconfigured_handle_refresh_is_none() {
        let handle: PlanAuthHandle = None;
        assert!(handle.refresh_account("work").await.is_none());
    }
}
