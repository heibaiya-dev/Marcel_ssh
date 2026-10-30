//! 混合逻辑时钟（HLC）：给每个 op 一个全局可比较、且因果有序的时间戳。
//!
//! 只用墙钟的话，两台设备时钟差几分钟，后改的反而被先改的盖掉；只用 Lamport 计数
//! 的话，时间戳和真实时间脱节，用户看「同步记录」时对不上。HLC 取两者之长：
//! `ms` 跟着墙钟走（所以大致等于真实时间），`counter` 在同一毫秒内（或本机时钟倒退时）
//! 补足单调性，`device` 兜底让全序成立（两台设备不可能产生完全相同的 HLC）。
//!
//! 比较顺序 `ms → counter → device`，由字段声明顺序 + `derive(Ord)` 保证，**别调字段顺序**。

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// 远端时钟比本机墙钟快过这个值就告警（仍然接受，否则会破坏收敛）。
const SKEW_WARN_MS: u64 = 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    pub ms: u64,
    pub counter: u32,
    pub device: String,
}

impl Hlc {
    /// 「加入同步」时本机已有数据用的时间戳（见规格 4.6）：任何真实写入的 `ms` 都大于 0，
    /// 所以云端在每一个冲突上都赢，本机独有的条目与字段则原样保留。
    pub fn zero_for(device: impl Into<String>) -> Hlc {
        Hlc {
            ms: 0,
            counter: 0,
            device: device.into(),
        }
    }
}

/// 本机时钟。`last` 是本机已发出 / 已观察到的最大时间戳，持久化在 `state.json`，
/// 重启后用 [`HlcClock::from_last`] 恢复——否则墙钟被往回调的设备会发出比旧 op 还小的时间戳。
#[derive(Clone, Debug)]
pub struct HlcClock {
    last: Hlc,
}

impl HlcClock {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            last: Hlc::zero_for(device),
        }
    }

    /// 从持久化的 `last` 恢复。`last.device` 就是本机设备 id。
    pub fn from_last(last: Hlc) -> Self {
        Self { last }
    }

    pub fn last(&self) -> &Hlc {
        &self.last
    }

    pub fn device(&self) -> &str {
        &self.last.device
    }

    /// 发一个新时间戳（严格大于此前发出 / 观察到的任何一个）。`wall_ms` 由调用方传入，
    /// 这样测试可以精确控制时间（用 [`wall_clock_ms`] 取真实墙钟）。
    pub fn now(&mut self, wall_ms: u64) -> Hlc {
        let ms = wall_ms.max(self.last.ms);
        let (ms, counter) = if ms == self.last.ms {
            match self.last.counter.checked_add(1) {
                Some(c) => (ms, c),
                // counter 溢出在现实里不会发生（同一毫秒发 40 亿次），但要保证严格单调
                None => (ms + 1, 0),
            }
        } else {
            (ms, 0)
        };
        self.last = Hlc {
            ms,
            counter,
            device: self.last.device.clone(),
        };
        self.last.clone()
    }

    /// 看到远端的时间戳：把 `last` 推进到 max(last, remote)（只看 ms/counter，device 仍是本机）。
    /// 之后的 [`now`](Self::now) 必然大于它——这保证「我在看到你的删除之后做的修改」排在删除之后。
    ///
    /// 远端比本机墙钟快 1 小时以上时告警但照常接受：拒绝会让两台设备的时钟永远对不齐。
    pub fn observe(&mut self, remote: &Hlc, wall_ms: u64) {
        if (remote.ms, remote.counter) <= (self.last.ms, self.last.counter) {
            return;
        }
        if remote.ms > wall_ms.saturating_add(SKEW_WARN_MS) {
            log::warn!(
                "同步：远端设备的时钟比本机快 {} 分钟，请检查两台设备的系统时间",
                (remote.ms - wall_ms) / 60_000
            );
        }
        self.last = Hlc {
            ms: remote.ms,
            counter: remote.counter,
            device: self.last.device.clone(),
        };
    }
}

/// 真实墙钟（Unix 毫秒）。系统时间早于 1970 年这种事不存在，真遇上就当 0。
pub fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(ms: u64, counter: u32, device: &str) -> Hlc {
        Hlc {
            ms,
            counter,
            device: device.into(),
        }
    }

    #[test]
    fn ordering_is_ms_then_counter_then_device() {
        assert!(h(2, 0, "a") > h(1, 9, "z"));
        assert!(h(1, 2, "a") > h(1, 1, "z"));
        assert!(h(1, 1, "b") > h(1, 1, "a"));
        assert_eq!(h(1, 1, "a"), h(1, 1, "a"));
    }

    #[test]
    fn zero_is_below_any_real_timestamp() {
        let z = Hlc::zero_for("dev");
        assert!(z < h(1, 0, ""));
        assert!(z < h(1, 0, "aaa"));
        // 同样是 0 的两台设备靠 device 分高下，仍是全序
        assert_ne!(Hlc::zero_for("a"), Hlc::zero_for("b"));
    }

    #[test]
    fn now_follows_wall_clock_and_resets_counter() {
        let mut c = HlcClock::new("A");
        let a = c.now(1000);
        assert_eq!((a.ms, a.counter), (1000, 0));
        let b = c.now(2000);
        assert_eq!((b.ms, b.counter), (2000, 0));
        assert!(b > a);
        assert_eq!(b.device, "A");
    }

    #[test]
    fn now_is_strictly_monotonic_within_same_ms() {
        let mut c = HlcClock::new("A");
        let a = c.now(1000);
        let b = c.now(1000);
        let d = c.now(1000);
        assert_eq!((b.ms, b.counter), (1000, 1));
        assert_eq!((d.ms, d.counter), (1000, 2));
        assert!(a < b && b < d);
    }

    #[test]
    fn now_stays_monotonic_when_wall_clock_goes_backwards() {
        let mut c = HlcClock::new("A");
        let a = c.now(5000);
        let b = c.now(1000); // 用户把系统时间往回调了
        assert!(b > a);
        assert_eq!(b.ms, 5000);
        assert_eq!(b.counter, 1);
    }

    #[test]
    fn observe_advances_and_next_now_exceeds_remote() {
        let mut c = HlcClock::new("A");
        c.now(1000);
        let remote = h(9000, 3, "B");
        c.observe(&remote, 1000);
        assert_eq!((c.last().ms, c.last().counter), (9000, 3));
        assert_eq!(c.last().device, "A"); // device 仍是本机
        let n = c.now(1500); // 本机墙钟远落后于远端
        assert!(n > remote, "{n:?} 必须大于 {remote:?}");
        assert_eq!((n.ms, n.counter), (9000, 4));
    }

    #[test]
    fn observe_ignores_older_remote() {
        let mut c = HlcClock::new("A");
        c.now(5000);
        let before = c.last().clone();
        c.observe(&h(1000, 0, "B"), 5000);
        assert_eq!(c.last(), &before);
        // 同 ms 但 counter 更小也不动
        c.observe(&h(5000, 0, "B"), 5000);
        assert_eq!(c.last(), &before);
    }

    #[test]
    fn observe_far_future_remote_is_still_accepted() {
        let mut c = HlcClock::new("A");
        let remote = h(10 * 3_600_000, 0, "B");
        c.observe(&remote, 1000); // 会 warn，但不能拒绝
        assert_eq!(c.last().ms, remote.ms);
    }

    #[test]
    fn counter_overflow_still_strictly_increases() {
        let mut c = HlcClock::from_last(h(7, u32::MAX, "A"));
        let n = c.now(0);
        assert!(n > h(7, u32::MAX, "A"));
    }

    #[test]
    fn from_last_restores_after_restart() {
        let mut c1 = HlcClock::new("A");
        let a = c1.now(8000);
        let mut c2 = HlcClock::from_last(c1.last().clone());
        let b = c2.now(100); // 重启后墙钟更小
        assert!(b > a);
    }

    #[test]
    fn hlc_json_round_trip() {
        let x = h(1_700_000_000_123, 7, "dev-1");
        let s = serde_json::to_string(&x).unwrap();
        assert_eq!(s, r#"{"ms":1700000000123,"counter":7,"device":"dev-1"}"#);
        assert_eq!(serde_json::from_str::<Hlc>(&s).unwrap(), x);
    }
}
