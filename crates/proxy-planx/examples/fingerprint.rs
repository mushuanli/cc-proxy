//! Print the outbound TLS / HTTP2 fingerprint for each impersonation profile.
//!
//! Run against a fingerprint echo service:
//!
//! ```bash
//! cargo run -p proxy-planx --features impersonate --example fingerprint
//! cargo run -p proxy-planx --features impersonate --example fingerprint -- chrome
//! ```
//!
//! It uses the *same* client builder and the *same* per-request hook as the quota
//! probe and the token refresh (`transport::maintenance_client` +
//! `transport::apply_emulation`), so a result here is what the upstream actually
//! sees.
//!
//! It prints both halves of "fingerprint":
//!
//! 1. **the header layer** — the request as built locally (offline), so a browser
//!    ClientHello carrying non-browser headers is visible;
//! 2. **the wire layer** — JA3 / JA4 / HTTP2 (Akamai) as the server saw them.
//!
//! `off` is included on purpose: it is the control. If `off` and `chrome` print
//! the same JA4 / Akamai hash, impersonation is not reaching the wire.

use proxy_planx::transport::{apply_emulation, maintenance_client, Impersonation};

/// Echoes back the TLS ClientHello and HTTP/2 frames it received.
const ECHO_URL: &str = "https://tls.peet.ws/api/all";

/// Headers that give a browser profile away when they are missing or wrong.
const TELLTALE_HEADERS: &[&str] = &[
    "user-agent",
    "accept",
    "accept-language",
    "accept-encoding",
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "sec-fetch-dest",
    "sec-fetch-mode",
    "sec-fetch-site",
    "upgrade-insecure-requests",
    "priority",
];

fn field<'a>(value: &'a serde_json::Value, path: &[&str]) -> &'a str {
    let mut cursor = value;
    for key in path {
        cursor = &cursor[*key];
    }
    cursor.as_str().unwrap_or("-")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let wanted: Vec<Impersonation> = match std::env::args().nth(1) {
        Some(raw) => match Impersonation::parse(&raw) {
            Some(profile) => vec![profile],
            None => {
                eprintln!(
                    "unknown profile '{raw}'; expected one of: {}",
                    Impersonation::ALL
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
                std::process::exit(2);
            }
        },
        // `off` first: it is the baseline every other row is compared against.
        None => {
            let mut all = vec![Impersonation::Off];
            all.extend(
                Impersonation::ALL
                    .iter()
                    .copied()
                    .filter(|profile| !profile.is_off()),
            );
            all
        }
    };

    println!(
        "IMPERSONATION_COMPILED = {}\n",
        proxy_planx::IMPERSONATION_COMPILED
    );

    let client = maintenance_client(None)?;
    println!(
        "{:<10} {:<5} {:<34} {:<28} {:<34} user-agent",
        "profile", "http", "ja3^(GREASE)", "ja4", "akamai_hash"
    );

    for profile in wanted {
        // Exactly what `probe::send` does: an explicit `accept`, then the profile.
        let request = apply_emulation(
            client.get(ECHO_URL).header("accept", "application/json"),
            profile,
        );

        println!("\n── {} ── request headers as built", profile.as_str());
        if let Some(clone) = request.try_clone() {
            match clone.build() {
                Ok(built) => {
                    for name in TELLTALE_HEADERS {
                        let value = built
                            .headers()
                            .get(*name)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("—");
                        println!("  {name:<28} {value}");
                    }
                }
                Err(error) => println!("  (could not build the request: {error})"),
            }
        }

        let response = request.send().await?;
        let seen: serde_json::Value = response.json().await?;

        println!(
            "{:<10} {:<5} {:<34} {:<28} {:<34} {}",
            profile.as_str(),
            field(&seen, &["http_version"]),
            field(&seen, &["tls", "ja3_hash"]),
            field(&seen, &["tls", "ja4"]),
            field(&seen, &["http2", "akamai_fingerprint_hash"]),
            field(&seen, &["user_agent"]),
        );
    }

    println!(
        "\nHow to read this:\n\
         * header layer — a profile must add the browser defaults (`sec-ch-ua`, `sec-fetch-*`,\n\
           `accept-language`, `upgrade-insecure-requests`). A browser TLS fingerprint with\n\
           Codex/`application/json` headers is a mixed fingerprint and easier to spot, not harder.\n\
         * wire layer — compare each row with a real browser's own /api/all output.\n\
           `off` must differ from every profile; if it does not, the profile is not reaching the wire.\n\
         * the ja3 column changes on every connection (browser profiles randomise GREASE),\n\
           so it is NOT a comparison target. ja4 and akamai_hash are stable — use those.\n\
         * `off` and a profile must also differ in the header list, or the profile only\n\
           changed the TLS layer."
    );
    Ok(())
}
