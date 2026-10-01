//! How to read an upstream's **model catalog** (its list of available models).
//!
//! Every family publishes one, and no two agree on path, auth or payload shape:
//!
//! | kind | path | payload |
//! |---|---|---|
//! | OpenAI-compatible | `/v1/models` | `data[].id`, Bearer |
//! | Anthropic | `/v1/models` | `data[].id` + `anthropic-version`, paginated |
//! | Codex backend | `/models` | `models[].slug`, gated on `client_version` |
//! | Gemini | `/v1beta/models` | `models[].name`, paginated, names normalized |
//! | manual | — | nothing is fetched; the configured models are the catalog |
//!
//! This names only the *reading* difference. It is deliberately independent of
//! the wire protocol used for inference: a gateway may serve OpenAI-shaped
//! catalogs while relaying Anthropic requests, and Gemini is catalog-only here
//! (cc-proxy cannot relay to it).

/// How to read one upstream's model catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsKind {
    /// `GET {base}/v1/models` → `data[].id`. The common case.
    #[default]
    OpenAi,
    /// `GET {base}/v1/models` → `data[].id`, `anthropic-version` header, paged.
    Anthropic,
    /// `GET {base}/models?client_version=…` → `models[].slug`.
    Codex,
    /// `GET {base}/v1beta/models` → `models[].name` (`models/x` → `x`).
    Gemini,
    /// The gateway decides, or models are declared by hand: nothing is fetched.
    Manual,
}

impl ModelsKind {
    pub const ALL: &'static [ModelsKind] = &[
        ModelsKind::OpenAi,
        ModelsKind::Anthropic,
        ModelsKind::Codex,
        ModelsKind::Gemini,
        ModelsKind::Manual,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelsKind::OpenAi => "openai",
            ModelsKind::Anthropic => "anthropic",
            ModelsKind::Codex => "codex",
            ModelsKind::Gemini => "gemini",
            ModelsKind::Manual => "manual",
        }
    }

    /// Parse a configured name. Aliases are accepted because operators reach for
    /// the vendor's own word rather than ours.
    pub fn parse(raw: &str) -> Option<Self> {
        // Vendors write it both ways ("openai-compatible" / "openai_compatible").
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "openai" | "openai_compatible" | "oai" => Some(ModelsKind::OpenAi),
            "anthropic" | "claude" => Some(ModelsKind::Anthropic),
            "codex" | "chatgpt" | "responses" => Some(ModelsKind::Codex),
            "gemini" | "google" => Some(ModelsKind::Gemini),
            "manual" | "none" | "off" => Some(ModelsKind::Manual),
            _ => None,
        }
    }

    pub fn accepted_names() -> String {
        Self::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Path appended to the provider's base URL. Empty when nothing is fetched.
    pub fn default_path(self) -> &'static str {
        match self {
            ModelsKind::OpenAi | ModelsKind::Anthropic => "/v1/models",
            ModelsKind::Codex => "/models",
            ModelsKind::Gemini => "/v1beta/models",
            ModelsKind::Manual => "",
        }
    }

    /// Whether this kind talks to the upstream at all.
    pub fn fetches(self) -> bool {
        self != ModelsKind::Manual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_aliases_resolve() {
        for kind in ModelsKind::ALL {
            assert_eq!(ModelsKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(
            ModelsKind::parse("OpenAI-Compatible"),
            Some(ModelsKind::OpenAi)
        );
        assert_eq!(ModelsKind::parse("claude"), Some(ModelsKind::Anthropic));
        assert_eq!(ModelsKind::parse("chatgpt"), Some(ModelsKind::Codex));
        assert_eq!(ModelsKind::parse("google"), Some(ModelsKind::Gemini));
        assert_eq!(ModelsKind::parse("off"), Some(ModelsKind::Manual));
        assert_eq!(ModelsKind::parse("vertex"), None);
        assert!(ModelsKind::accepted_names().contains("manual"));
    }

    #[test]
    fn each_kind_knows_its_path() {
        assert_eq!(ModelsKind::OpenAi.default_path(), "/v1/models");
        assert_eq!(ModelsKind::Anthropic.default_path(), "/v1/models");
        assert_eq!(ModelsKind::Codex.default_path(), "/models");
        assert_eq!(ModelsKind::Gemini.default_path(), "/v1beta/models");
        assert_eq!(ModelsKind::Manual.default_path(), "");
        assert!(!ModelsKind::Manual.fetches());
        assert!(ModelsKind::OpenAi.fetches());
    }

    #[test]
    fn the_default_is_the_common_case() {
        assert_eq!(ModelsKind::default(), ModelsKind::OpenAi);
    }
}
