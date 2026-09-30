//! Cross-protocol translation seam.
//!
//! cc-proxy already speaks two wire protocols: Anthropic Messages (`Anthropic`)
//! and OpenAI Responses (`Codex`). Historically a request had to go to an
//! upstream speaking the *same* protocol it arrived in. This seam lifts that
//! restriction without teaching `proxy-relay` anything about either format:
//! the relay asks an injected adapter to rewrite the body on the way out and the
//! response on the way back.
//!
//! Same policy/mechanism split as the account seam in [`crate::auth`]:
//!
//! ```text
//! proxy-common   trait ProtocolAdapter / ResponseTranslator   (policy + shape)
//! proxy-bridge   concrete Anthropic ↔ Codex translation       (mechanism)
//! proxy-relay    calls it when client protocol != upstream    (consumer)
//! proxy-server   builds and injects it                        (composition)
//! ```
//!
//! With no adapter injected the relay behaves exactly as before, so this is
//! additive.

use std::sync::Arc;

/// A wire protocol cc-proxy can receive or send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireProtocol {
    /// Anthropic Messages API (`/v1/messages`).
    Anthropic,
    /// OpenAI Responses API (`/responses`).
    Codex,
}

impl WireProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            WireProtocol::Anthropic => "anthropic",
            WireProtocol::Codex => "codex",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" | "messages" => Some(WireProtocol::Anthropic),
            "codex" | "openai" | "responses" => Some(WireProtocol::Codex),
            _ => None,
        }
    }

    /// Canonical names, for validation messages and UI lists.
    pub fn accepted_names() -> String {
        [WireProtocol::Anthropic, WireProtocol::Codex]
            .iter()
            .map(|protocol| protocol.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

/// A request body rewritten for a different upstream protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedRequest {
    pub body: Vec<u8>,
    /// Streaming flag as the *upstream* expects it (it is a body field, not a header).
    pub stream: bool,
    /// Model name to bill/record against, when the translation knows it.
    pub model: Option<String>,
}

/// Stateful streaming response translator.
///
/// One instance per response: a stream is a state machine (the client protocol
/// requires exactly one `message_start`, monotonic content-block indices, and a
/// terminal `message_stop`), so translation cannot be a pure function.
pub trait ResponseTranslator: Send {
    /// Feed one upstream SSE frame (the raw `data:` payload) and get back zero or
    /// more client-facing SSE frames, already serialised including the
    /// `event:`/`data:` prefixes.
    fn push(&mut self, event: &[u8]) -> Vec<Vec<u8>>;

    /// Signal end of stream; emits anything still owed to the client.
    fn finish(&mut self) -> Vec<Vec<u8>>;

    /// Signal that the upstream stream ended **abnormally** — a transport error,
    /// a client disconnect, or a body that could not be translated.
    ///
    /// Must not be reported as a successful completion: a client has to be able
    /// to tell a truncated answer from a complete one. The default preserves the
    /// old behaviour for translators that have nothing better to say.
    fn abort(&mut self, _reason: &str) -> Vec<Vec<u8>> {
        self.finish()
    }

    /// Non-streaming translation of a complete upstream body.
    ///
    /// Default: unsupported, in which case the relay leaves the response alone.
    fn translate_complete(&mut self, _body: &[u8]) -> Option<Vec<u8>> {
        None
    }

    /// Render a complete upstream body as the client protocol's SSE sequence.
    ///
    /// Needed because an upstream may ignore `stream: true` and answer with a
    /// single JSON body. Without this the client would receive a valid but empty
    /// event stream. Default: unsupported, leaving behaviour unchanged.
    fn stream_from_complete(&mut self, _body: &[u8]) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// Translates between wire protocols.
pub trait ProtocolAdapter: Send + Sync {
    /// Whether this adapter can translate `from` (client) to `to` (upstream).
    fn supports(&self, from: WireProtocol, to: WireProtocol) -> bool;

    /// Rewrite a request body for the upstream protocol.
    ///
    /// Returning `Err` aborts the request with a 502 rather than forwarding a
    /// malformed body upstream.
    fn translate_request(
        &self,
        from: WireProtocol,
        to: WireProtocol,
        body: &[u8],
    ) -> Result<TranslatedRequest, String>;

    /// Build a streaming translator for the response direction.
    fn response_translator(
        &self,
        from: WireProtocol,
        to: WireProtocol,
    ) -> Option<Box<dyn ResponseTranslator>>;
}

/// Injected adapter, absent by default.
pub type ProtocolAdapterHandle = Option<Arc<dyn ProtocolAdapter>>;

/// Convenience for callers holding a [`ProtocolAdapterHandle`].
pub trait ProtocolAdapterHandleExt {
    /// The adapter, when one is present and covers this direction.
    fn adapter_for(
        &self,
        from: WireProtocol,
        to: WireProtocol,
    ) -> Option<&Arc<dyn ProtocolAdapter>>;
}

impl ProtocolAdapterHandleExt for ProtocolAdapterHandle {
    fn adapter_for(
        &self,
        from: WireProtocol,
        to: WireProtocol,
    ) -> Option<&Arc<dyn ProtocolAdapter>> {
        self.as_ref().filter(|adapter| adapter.supports(from, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    impl ProtocolAdapter for Stub {
        fn supports(&self, from: WireProtocol, to: WireProtocol) -> bool {
            from == WireProtocol::Anthropic && to == WireProtocol::Codex
        }

        fn translate_request(
            &self,
            _from: WireProtocol,
            _to: WireProtocol,
            body: &[u8],
        ) -> Result<TranslatedRequest, String> {
            Ok(TranslatedRequest {
                body: body.to_vec(),
                stream: false,
                model: None,
            })
        }

        fn response_translator(
            &self,
            _from: WireProtocol,
            _to: WireProtocol,
        ) -> Option<Box<dyn ResponseTranslator>> {
            None
        }
    }

    #[test]
    fn wire_protocol_round_trips() {
        for protocol in [WireProtocol::Anthropic, WireProtocol::Codex] {
            assert_eq!(WireProtocol::parse(protocol.as_str()), Some(protocol));
        }
        assert_eq!(
            WireProtocol::parse("messages"),
            Some(WireProtocol::Anthropic)
        );
        assert_eq!(WireProtocol::parse("responses"), Some(WireProtocol::Codex));
        assert_eq!(WireProtocol::parse("grpc"), None);
    }

    #[test]
    fn absent_handle_yields_no_adapter() {
        let handle: ProtocolAdapterHandle = None;
        assert!(handle
            .adapter_for(WireProtocol::Anthropic, WireProtocol::Codex)
            .is_none());
    }

    #[test]
    fn handle_filters_by_direction() {
        let handle: ProtocolAdapterHandle = Some(Arc::new(Stub));
        assert!(handle
            .adapter_for(WireProtocol::Anthropic, WireProtocol::Codex)
            .is_some());
        // The reverse direction is not covered by this adapter.
        assert!(handle
            .adapter_for(WireProtocol::Codex, WireProtocol::Anthropic)
            .is_none());
        // Same-protocol needs no translation at all.
        assert!(handle
            .adapter_for(WireProtocol::Anthropic, WireProtocol::Anthropic)
            .is_none());
    }
}
