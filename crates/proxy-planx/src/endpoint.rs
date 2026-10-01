//! Where a plan connection points, per family × mode.
//!
//! A plan is the relay's peer to an upstream — both answer "which upstream do we
//! talk to" — but it is defined by a *subscription account* instead of by
//! providers and tiers, so the endpoint follows from the account's family and
//! mode rather than from configuration.

use proxy_common::config::{AccountFamily, AccountMode};
use proxy_common::protocol::WireProtocol;
use proxy_common::{ModelsKind, PlanEndpoint};

/// The ChatGPT backend the Codex CLI itself talks to.
const CHATGPT_CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// OpenAI's platform API, for an `api_key` account.
const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
/// Anthropic, shared by both modes.
const ANTHROPIC_BASE: &str = "https://api.anthropic.com";

/// Endpoint for an account.
///
/// The *mode* matters, not just the family: a ChatGPT subscription talks to
/// `chatgpt.com/backend-api/codex` (Codex wire), while a platform key talks to
/// `api.openai.com/v1`. Both are addressed as `{base}/{protocol path}`.
pub fn endpoint_for(family: AccountFamily, mode: AccountMode) -> PlanEndpoint {
    match (family, mode) {
        (AccountFamily::Gpt, AccountMode::Plan) => PlanEndpoint {
            base_url: CHATGPT_CODEX_BASE.to_string(),
            protocol: WireProtocol::Codex,
            models_kind: ModelsKind::Codex,
        },
        (AccountFamily::Gpt, AccountMode::ApiKey) => PlanEndpoint {
            base_url: OPENAI_API_BASE.to_string(),
            protocol: WireProtocol::Codex,
            models_kind: ModelsKind::OpenAi,
        },
        (AccountFamily::Claude, _) => PlanEndpoint {
            base_url: ANTHROPIC_BASE.to_string(),
            protocol: WireProtocol::Anthropic,
            models_kind: ModelsKind::Anthropic,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_decides_the_gpt_endpoint() {
        let plan = endpoint_for(AccountFamily::Gpt, AccountMode::Plan);
        assert_eq!(plan.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(plan.protocol, WireProtocol::Codex);
        assert_eq!(plan.models_kind, ModelsKind::Codex);

        let key = endpoint_for(AccountFamily::Gpt, AccountMode::ApiKey);
        assert_eq!(key.base_url, "https://api.openai.com/v1");
        assert_eq!(key.protocol, WireProtocol::Codex);
        assert_eq!(key.models_kind, ModelsKind::OpenAi);
    }

    #[test]
    fn claude_shares_one_endpoint_across_modes() {
        for mode in [AccountMode::Plan, AccountMode::ApiKey] {
            let endpoint = endpoint_for(AccountFamily::Claude, mode);
            assert_eq!(endpoint.base_url, "https://api.anthropic.com");
            assert_eq!(endpoint.protocol, WireProtocol::Anthropic);
            assert_eq!(endpoint.models_kind, ModelsKind::Anthropic);
        }
    }

    #[test]
    fn plan_bases_join_the_protocol_path_without_a_double_slash() {
        // The relay appends `ApiProtocol::default_path()`, e.g. "/responses".
        for (family, mode) in [
            (AccountFamily::Gpt, AccountMode::Plan),
            (AccountFamily::Gpt, AccountMode::ApiKey),
            (AccountFamily::Claude, AccountMode::Plan),
        ] {
            let base = endpoint_for(family, mode).base_url;
            assert!(!base.ends_with('/'), "{base}");
            assert!(base.starts_with("https://"), "{base}");
        }
    }
}
