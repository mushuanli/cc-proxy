use crate::models::WsMessage;
use tokio::sync::broadcast;

/// 跨模块异步事件总线。
///
/// 模块间（proxy → ws, web → ws, storage → ws）通过 EventBus 解耦通信，
/// 而非直接调用。发布是非阻塞的，订阅者通过 broadcast channel 接收消息。
pub struct EventBus {
    tx: broadcast::Sender<WsMessage>,
}

impl EventBus {
    /// 创建新的 EventBus，指定 channel 容量。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布事件（非阻塞）。
    ///
    /// 如果所有 receiver 都已 lagged（落后太多），消息会被静默丢弃。
    /// 调用方无需处理发送失败——lagged 的 receiver 会收到
    /// `RecvError::Lagged`，从而触发自身的重新同步逻辑。
    pub fn publish(&self, msg: WsMessage) {
        let _ = self.tx.send(msg);
    }

    /// 订阅事件流，返回一个 Receiver。
    ///
    /// 调用方应循环调用 `receiver.recv().await` 来处理消息。
    /// 当收到 `RecvError::Lagged(n)` 时，表明有 `n` 条消息被丢弃，
    /// 调用方应触发完整重同步（例如通过 REST API 重新拉取数据）。
    pub fn subscribe(&self) -> broadcast::Receiver<WsMessage> {
        self.tx.subscribe()
    }
}

impl Clone for EventBus {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::WsMessage;

    #[test]
    fn publish_and_receive() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(WsMessage::Cleared);

        let msg = rx.try_recv().unwrap();
        matches!(msg, WsMessage::Cleared);
    }

    #[test]
    fn multiple_subscribers() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(WsMessage::Cleared);

        matches!(rx1.try_recv().unwrap(), WsMessage::Cleared);
        matches!(rx2.try_recv().unwrap(), WsMessage::Cleared);
    }

    #[test]
    fn clone_bus_shares_channel() {
        let bus1 = EventBus::new(16);
        let bus2 = bus1.clone();
        let mut rx = bus1.subscribe();

        bus2.publish(WsMessage::Cleared);

        matches!(rx.try_recv().unwrap(), WsMessage::Cleared);
    }
}
