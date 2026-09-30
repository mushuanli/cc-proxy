pub mod auth;
pub mod config;
pub(crate) mod core;
pub mod messages;
pub mod models;
pub mod protocol;
pub mod response;

// Re-export config (only public items from config/mod.rs)
pub use config::*;

// Re-export the upstream-auth seam
pub use auth::{PlanAuthFuture, PlanAuthHandle, PlanAuthHandleExt, PlanAuthProvider, UpstreamAuth};

// Re-export the cross-protocol translation seam
pub use protocol::{
    ProtocolAdapter, ProtocolAdapterHandle, ProtocolAdapterHandleExt, ResponseTranslator,
    TranslatedRequest, WireProtocol,
};

// Re-export core
pub use core::event::EventBus;

// Re-export shared domain types
pub use messages::{extract_user_text, is_real_user_prompt, is_tool_result};
pub use models::{
    BillingSnapshot, ClientType, CostData, DailyCost, ModelCost, NormalizedResponse, PriceRates,
    ProviderCost, ProviderInfo, ProxiedRequest, SessionCost, SessionId, SseEvent, TaskId,
    TaskStatus, TaskUsage, TierRuleInfo, ToolCallRecord, UpstreamInfo, WsMessage,
};
pub use response::{normalize_response, sanitize_text};
