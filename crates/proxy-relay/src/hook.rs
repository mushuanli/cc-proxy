use proxy_common::EventBus;
use proxy_common::WsMessage;
use serde_json::Value;

/// Hook event receiver.
///
/// Receives hook events from proxy-hook-agent (POSTed by the CLI),
/// publishes them via EventBus for WebSocket clients.
#[derive(Clone)]
pub struct HookReceiver {
    events: EventBus,
}

impl HookReceiver {
    pub fn new(events: EventBus) -> Self {
        Self { events }
    }

    /// Receive a hook event payload and broadcast.
    pub fn receive(&self, payload: &Value) {
        // Extract hook event name from payload
        let hook_event_name = payload
            .get("hook_event_name")
            .or_else(|| payload.get("hookEventName"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let session_id = payload
            .get("session_id")
            .or_else(|| payload.get("sessionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let hook = proxy_common::HookEvent::new(
            hook_event_name.to_string(),
            session_id.to_string(),
            payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );

        self.events.publish(WsMessage::NewHook(hook));
    }

    /// Update hook result by payload body.
    pub fn update_by_payload(&self, payload: &Value) {
        let _ = payload; // Hook update is handled by the API layer
    }

    /// Clear all hook records (notifies frontend).
    pub fn clear_all(&self) {
        self.events.publish(WsMessage::Cleared);
    }
}
