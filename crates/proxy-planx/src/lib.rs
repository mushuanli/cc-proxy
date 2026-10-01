//! # proxy-planx — upstream accounts as an upstream credential
//!
//! Mechanism layer for the planx feature. It answers exactly one question for
//! `proxy-relay`: *what headers should this upstream request carry?* Everything
//! else — routing, request bodies, response parsing, billing, recording — stays
//! in the existing crates.
//!
//! ## Two families × two modes
//!
//! | family | `mode = api_key` | `mode = plan` |
//! |---|---|---|
//! | `gpt` (ChatGPT / Codex) | static key → `Bearer`/`x-api-key` | Codex OAuth + Codex identity |
//! | `claude` (Anthropic) | static key → `x-api-key` | Claude OAuth + Claude Code identity + `oauth-2025-04-20` beta |
//!
//! The family decides *how the credential is presented* and which identity is
//! synthesised; the mode decides *where the credential comes from* (a static
//! string, or an OAuth refresh cycle).
//!
//! ```text
//! proxy-common   Policy + seam:  UpstreamAuth / PlanAuthProvider / AccountConfig
//! proxy-planx    Mechanism:      OAuth refresh, client identity, quota probes
//! proxy-relay    Consumer:       asks the seam for auth, keeps sending the request
//! proxy-server   Composition:    builds PlanxRegistry and injects it
//! ```
//!
//! Dependency direction is `server → planx → common ← relay`, so `proxy-relay`
//! never learns about OAuth and `proxy-common` stays free of `reqwest`/`http`.
//! Configuration vocabulary — including the identity and impersonation profile
//! names — is owned by `proxy-common` and validated there; this crate consumes
//! the parsed values and never interprets raw config strings.
//!
//! ## Not implemented here
//!
//! - **Protocol translation.** planx reuses the existing wire protocols; the
//!   Anthropic ⇄ Responses bridge lives in `proxy-bridge`.
//! - **Sending `/responses`.** `proxy-relay` keeps that transport, so proxied
//!   traffic has exactly one code path.

#![deny(unsafe_code)]

pub mod credential;
pub mod endpoint;
pub mod error;
pub mod identity;
pub mod jwt;
pub mod probe;
pub mod registry;
pub mod transport;

pub use credential::{Credentials, TokenStore};
pub use endpoint::endpoint_for;
pub use error::{PlanxError, Result};
pub use identity::{Identity, CLAUDE_API_VERSION, CLAUDE_OAUTH_BETA, DEFAULT_CLAUDE_CLI_VERSION};
pub use probe::{AccountQuota, ProbeUrls, QuotaWindow};
pub use registry::{spawn_refresher, PlanAccount, PlanxRegistry, ReloadReport};
pub use transport::IMPERSONATION_COMPILED;
