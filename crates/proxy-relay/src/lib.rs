//! proxy-relay — 代理通信中继库。
//!
//! 职责极简：收发流量 → 写入 store → 发送事件。
//! 不对外暴露查询 API，不计算费用（store 内部计费），不做 UI 决策（前端自行判断）。
//!
//! ```text
//! 上游 API ←── HTTP ──→ proxy-relay ──→ store.task_write(sid, NewTask { billing, usage })
//!                               │
//!                               └──→ events.publish(WsMessage) ──→ WS → 浏览器
//! ```

pub(crate) mod capture;
pub(crate) mod relay;
pub(crate) mod sse;
pub(crate) mod upstream;

pub use capture::CaptureControl;
pub(crate) use capture::ExchangeInfo;
pub use relay::RelayHandler;
