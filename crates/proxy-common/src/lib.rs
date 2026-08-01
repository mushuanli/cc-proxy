pub mod config;
pub(crate) mod core;
pub mod models;
pub mod response;

// Re-export config (only public items from config/mod.rs)
pub use config::*;

// Re-export core
pub use core::event::EventBus;

// Re-export shared domain types
pub use models::{
    BillingSnapshot, ClientType, CostData, DailyCost, ModelCost, NormalizedResponse, PriceRates,
    ProviderCost, ProviderInfo, ProxiedRequest, SessionCost, SessionId, SseEvent, TaskId,
    TaskStatus, TaskUsage, TierRuleInfo, ToolCallRecord, UpstreamInfo, WsMessage,
};
pub use response::{normalize_response, sanitize_text};
