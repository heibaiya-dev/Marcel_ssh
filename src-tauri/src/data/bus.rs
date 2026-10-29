//! 数据变更广播。
//!
//! 任何一份已登记的数据被成功写盘后，写入方（`JsonStore`、`KnownHostsStore`）
//! 在**放掉写锁之后**往这里 `publish` 一条 [`DataChange`]。现阶段只有测试订阅；
//! 以后的同步引擎订阅它来得知「哪份数据变了、该推了」。
//!
//! 刻意做成「只说哪份数据变了，不带内容」：订阅方自己去读最新值，
//! 广播既不会拷贝大块数据，也不会让慢订阅者卡住写入方。

use tokio::sync::broadcast;

use super::DataSourceId;

/// 通道容量。慢订阅者落后太多时 `recv` 会返回 `Lagged`，订阅方应当把它当成
/// 「我可能漏了若干次变更，整体重新对一遍」处理——这对只带「哪份变了」的事件是无损的。
const BUS_CAPACITY: usize = 64;

/// 一次数据变更：哪份数据被写盘了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataChange {
    pub source: DataSourceId,
}

/// 数据变更总线（`Clone` 很便宜：只是一个发送端句柄）。
#[derive(Clone)]
pub struct DataChangeBus(broadcast::Sender<DataChange>);

impl DataChangeBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUS_CAPACITY);
        Self(tx)
    }

    /// 广播「`source` 这份数据刚被写盘」。没有订阅者时静默成功。
    pub fn publish(&self, source: DataSourceId) {
        // `send` 在没有任何接收者时返回 Err，那正是「现阶段没人订阅」的常态，不是错误。
        let _ = self.0.send(DataChange { source });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DataChange> {
        self.0.subscribe()
    }
}

impl Default for DataChangeBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_without_subscribers_is_silent() {
        let bus = DataChangeBus::new();
        bus.publish(DataSourceId::Settings);
    }

    #[tokio::test]
    async fn subscribers_receive_the_source_in_order() {
        let bus = DataChangeBus::new();
        let mut rx = bus.subscribe();
        bus.publish(DataSourceId::Settings);
        bus.publish(DataSourceId::KnownHosts);
        assert_eq!(rx.recv().await.unwrap().source, DataSourceId::Settings);
        assert_eq!(rx.recv().await.unwrap().source, DataSourceId::KnownHosts);
    }

    #[tokio::test]
    async fn clones_share_one_channel() {
        let bus = DataChangeBus::new();
        let mut rx = bus.subscribe();
        bus.clone().publish(DataSourceId::Skills);
        assert_eq!(rx.recv().await.unwrap().source, DataSourceId::Skills);
    }
}
