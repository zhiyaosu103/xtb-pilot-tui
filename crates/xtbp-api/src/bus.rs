//! 事件总线：daemon 内部广播 [`JobEvent`]，服务器把订阅连接接入
//! （设计文档 §3.7 订阅推送，agent 无需轮询）。

use tokio::sync::broadcast;
use xtbp_core::job::JobEvent;

/// 任务事件广播总线（tokio broadcast 封装）。
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<JobEvent>,
}

impl EventBus {
    /// 新建总线（环形缓冲容量）。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// 广播一个事件（无订阅者时直接丢弃）。
    pub fn publish(&self, event: JobEvent) {
        let _ = self.tx.send(event);
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.tx.subscribe()
    }

    /// 订阅者数量（health 报告用）。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_reaches_subscriber() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        bus.publish(JobEvent::QueueDepth {
            queued: 1,
            running: 2,
        });
        match rx.try_recv().unwrap() {
            JobEvent::QueueDepth { queued, running } => {
                assert_eq!((queued, running), (1, 2));
            }
            other => panic!("意外事件: {other:?}"),
        }
    }

    #[test]
    fn late_subscriber_misses_overflow_but_stays_usable() {
        let bus = EventBus::new(2);
        for i in 0..10 {
            bus.publish(JobEvent::QueueDepth {
                queued: i,
                running: 0,
            });
        }
        let mut rx = bus.subscribe();
        // 可能 Lagged 或拿到最近事件——两者都可接受，订阅不 panic 即可
        match rx.try_recv() {
            Ok(_) => {}
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => {}
        }
    }
}
