//! Print the identity headers proxy-planx would send, so they can be diffed
//! against a capture of the real client:
//!
//! ```bash
//! cargo run -p proxy-planx --example identity_headers            # gpt, default profile
//! cargo run -p proxy-planx --example identity_headers -- claude
//! cargo run -p proxy-planx --example identity_headers -- gpt codex_cli_rs
//! scripts/capture-cli-headers.sh                                 # other side of the diff
//! ```
//!
//! Only the *identity* headers are printed; the credential (`authorization` /
//! `x-api-key`) is added by the registry, not by the identity profile.
use proxy_common::auth::UpstreamAuth;
use proxy_common::config::AccountFamily;
use proxy_planx::identity::Identity;

fn main() {
    let mut args = std::env::args().skip(1);
    let family = match args.next().as_deref() {
        Some("claude") => AccountFamily::Claude,
        _ => AccountFamily::Gpt,
    };
    let profile = args.next();

    let identity = Identity::from_profile_name(profile.as_deref(), family);
    println!(
        "# family={} profile={}",
        family.as_str(),
        identity.profile.as_str()
    );
    println!(
        "# cli_version={} (beta_features={})",
        identity.client_version, identity.beta_features
    );

    if let UpstreamAuth::Headers { set, append } =
        identity.headers(family, "ACCOUNT-SEED", Some("WORKSPACE-UUID"))
    {
        for (name, value) in set {
            println!("{name}: {value}");
        }
        for (name, value) in append {
            println!("{name}: {value}   # append (merged with the client's value)");
        }
    }
}
