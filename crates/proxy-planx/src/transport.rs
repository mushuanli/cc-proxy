//! HTTP transport for planx's own outbound calls (OAuth refresh, quota probes).
//!
//! Note: planx does **not** send `/responses` — that stays in `proxy-relay`, so
//! there is exactly one transport for proxied traffic. This client is only for
//! the maintenance calls planx owns.
//!
//! Two engines are available:
//!
//! | feature | engine | TLS stack |
//! |---|---|---|
//! | default | `reqwest` | rustls |
//! | `impersonate` | `wreq` | BoringSSL + browser profiles |
//!
//! The `impersonate` feature is opt-in because BoringSSL needs `cmake` and a C++
//! toolchain, and because a browser profile also rewrites default headers.
//!
//! The *policy* (which profile an operator selected) lives in
//! `proxy_common::config::Impersonation`; only the mapping onto an engine profile
//! is mechanism, and it lives here.

#[cfg(not(feature = "impersonate"))]
pub use reqwest as engine;
#[cfg(feature = "impersonate")]
pub use wreq as engine;

pub use proxy_common::config::Impersonation;

/// Whether this build can impersonate a browser TLS/HTTP2 fingerprint.
pub const IMPERSONATION_COMPILED: bool = cfg!(feature = "impersonate");

/// Build the client planx uses for its own maintenance requests.
///
/// Connection pooling is disabled (`pool_max_idle_per_host(0)`), matching the
/// rest of planx: these calls are rare and a shared pool would be a lifetime
/// liability for little gain.
pub fn maintenance_client(proxy: Option<&str>) -> crate::error::Result<engine::Client> {
    // Feature set deliberately mirrors `proxy-relay`'s client (no `http2`
    // feature), so planx does not silently diverge in transport behaviour.
    let mut builder = engine::Client::builder()
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(std::time::Duration::from_millis(1))
        .connect_timeout(std::time::Duration::from_secs(20))
        .tcp_nodelay(true);

    if let Some(proxy) = proxy.map(str::trim).filter(|p| !p.is_empty()) {
        let proxy = engine::Proxy::all(proxy).map_err(|e| {
            crate::error::PlanxError::config(format!("invalid proxy '{proxy}': {e}"))
        })?;
        builder = builder.proxy(proxy);
    }

    builder
        .build()
        .map_err(|e| crate::error::PlanxError::config(format!("failed to build HTTP client: {e}")))
}

/// Apply an impersonation profile to a request builder.
///
/// No-op unless the `impersonate` feature is compiled in and the profile is not
/// `Off`.
pub fn apply_emulation(
    request: engine::RequestBuilder,
    profile: Impersonation,
) -> engine::RequestBuilder {
    #[cfg(feature = "impersonate")]
    {
        if let Some(emulation) = emulation_for(profile) {
            return request.emulation(emulation);
        }
    }
    let _ = profile;
    request
}

/// Map a profile to a `wreq-util` browser profile.
///
/// Type note: `wreq_util::Profile` is the profile enum; `wreq::Emulation` is the
/// runtime TLS/HTTP2 settings struct (and `wreq_util::Emulation` is a descriptor
/// whose `Chrome149` associated constant is itself a `Profile`).
#[cfg(feature = "impersonate")]
pub fn emulation_for(profile: Impersonation) -> Option<wreq_util::Profile> {
    use wreq_util::Profile;
    match profile {
        Impersonation::Off => None,
        Impersonation::Chrome => Some(Profile::Chrome149),
        Impersonation::Chrome142 => Some(Profile::Chrome142),
        Impersonation::Edge => Some(Profile::Edge148),
        Impersonation::Firefox => Some(Profile::Firefox151),
        Impersonation::Safari => Some(Profile::Safari26),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_flag_matches_feature() {
        assert_eq!(IMPERSONATION_COMPILED, cfg!(feature = "impersonate"));
    }

    #[test]
    fn maintenance_client_builds_without_proxy() {
        let client = maintenance_client(None);
        assert!(client.is_ok(), "client should build: {:?}", client.err());
    }

    #[test]
    fn maintenance_client_rejects_bad_proxy() {
        assert!(maintenance_client(Some("not a proxy")).is_err());
    }

    /// Every configured profile must map to something the engine understands (or
    /// to `Off`), so a validated name can never silently do nothing.
    #[cfg(feature = "impersonate")]
    #[test]
    fn every_profile_maps_to_an_engine_profile() {
        for profile in Impersonation::ALL {
            assert_eq!(profile.is_off(), emulation_for(*profile).is_none());
        }
    }
}
