//! 时间窗口聚合：把「每秒增量」攒成「最近 N 秒的平均速率」。
//!
//! 界面上的 `实时 / 1分钟 / 5分钟 / 1小时` 就是这条链路的产物。
//! 实时数字适合看瞬时，但排序会疯狂跳动；切到 1 分钟就稳得多 ——
//! 这才是「谁在吃带宽」真正好用的视角。
//!
//! # 对齐规则
//!
//! 每个应用都保留同样长度的槽位，**这一秒没有流量的应用补 0**。
//! 不补的话不同应用的时间轴会错位，`rates(60)` 算出来的就不是同一分钟。

use std::collections::{HashMap, HashSet};

use crate::types::AppKey;

/// 一个槽位存一秒的增量。u32 够用（单进程 4GB/s 不现实），省一半内存。
type Slot = (u32, u32);

/// 最多记住 1 小时
pub const MAX_SLOTS: usize = 3600;

#[derive(Debug)]
pub struct WindowTracker {
    max_slots: usize,
    per_app: HashMap<AppKey, VecDeque>,
    total: VecDeque,
}

type VecDeque = std::collections::VecDeque<Slot>;

impl Default for WindowTracker {
    fn default() -> Self {
        Self::new(MAX_SLOTS)
    }
}

impl WindowTracker {
    pub fn new(max_slots: usize) -> Self {
        let max_slots = max_slots.max(1);
        Self {
            max_slots,
            per_app: HashMap::new(),
            total: std::collections::VecDeque::with_capacity(max_slots),
        }
    }

    /// 推入这一秒的增量。必须是「每个应用每秒恰好调用一次」，缺的应用会自动补 0。
    pub fn push(&mut self, deltas: &[(AppKey, u64, u64)]) {
        let mut seen: HashSet<&AppKey> = HashSet::with_capacity(deltas.len());
        let mut total_rx = 0u64;
        let mut total_tx = 0u64;

        for (app, rx, tx) in deltas {
            let d = self.per_app.entry(app.clone()).or_default();
            push_slot(d, *rx, *tx, self.max_slots);
            seen.insert(app);
            total_rx = total_rx.saturating_add(*rx);
            total_tx = total_tx.saturating_add(*tx);
        }

        // 本秒没有流量的应用补 0，保证所有应用时间轴对齐
        for (app, d) in self.per_app.iter_mut() {
            if !seen.contains(app) {
                push_slot(d, 0, 0, self.max_slots);
            }
        }

        push_slot(&mut self.total, total_rx, total_tx, self.max_slots);
    }

    /// 最近 `secs` 秒的平均速率（字节/秒），按总量降序。
    ///
    /// 分母取「实际攒到的秒数」和 `secs` 里小的那个：刚启动 5 秒时看「1小时」，
    /// 分母若按 3600 算，数字会小到像没在工作。攒满 `secs` 秒之后语义才回到
    /// 「最近 N 秒的平均」。
    pub fn rates(&self, secs: usize) -> Vec<(AppKey, f64, f64)> {
        let n = secs.clamp(1, self.max_slots);

        let mut out: Vec<(AppKey, f64, f64)> = self
            .per_app
            .iter()
            .filter_map(|(app, d)| {
                let (rx, tx) = sum_last(d, n);
                let denom = n.min(d.len()).max(1) as f64;
                let (rx, tx) = (rx as f64 / denom, tx as f64 / denom);
                // 平均下来不到 1 B/s 的，界面上就是「0 B/s」，不占列表位置
                if rx < 1.0 && tx < 1.0 {
                    return None;
                }
                Some((app.clone(), rx, tx))
            })
            .collect();

        out.sort_by(|a, b| {
            (b.1 + b.2)
                .partial_cmp(&(a.1 + a.2))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// 最近 `secs` 秒的总平均速率。分母规则同 [`Self::rates`]。
    pub fn total_rate(&self, secs: usize) -> (f64, f64) {
        let n = secs.clamp(1, self.max_slots);
        let (rx, tx) = sum_last(&self.total, n);
        let denom = n.min(self.total.len()).max(1) as f64;
        (rx as f64 / denom, tx as f64 / denom)
    }

    /// 最近 `secs` 秒每秒的总下载量，给折线图用。长度不足时前面少几个点。
    pub fn series(&self, secs: usize) -> Vec<f32> {
        let n = secs.clamp(1, self.max_slots);
        let skip = self.total.len().saturating_sub(n);
        self.total
            .iter()
            .skip(skip)
            .map(|(rx, _)| *rx as f32)
            .collect()
    }

    /// 清掉最近 `secs` 秒完全没动静的应用，防止长期运行后 map 只增不减。
    pub fn sweep_idle(&mut self, secs: usize) {
        let n = secs.clamp(1, self.max_slots);
        self.per_app.retain(|_, d| {
            let (rx, tx) = sum_last(d, n);
            rx > 0 || tx > 0
        });
    }

    pub fn app_count(&self) -> usize {
        self.per_app.len()
    }
}

fn push_slot(d: &mut VecDeque, rx: u64, tx: u64, max_slots: usize) {
    d.push_back((
        rx.min(u32::MAX as u64) as u32,
        tx.min(u32::MAX as u64) as u32,
    ));
    while d.len() > max_slots {
        d.pop_front();
    }
}

fn sum_last(d: &VecDeque, n: usize) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for (r, t) in d.iter().rev().take(n) {
        rx += *r as u64;
        tx += *t as u64;
    }
    (rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(s: &str) -> AppKey {
        AppKey::new(s.to_owned())
    }

    #[test]
    fn single_second_rate() {
        let mut w = WindowTracker::new(10);
        w.push(&[(app("chrome"), 1000, 100)]);
        let r = w.rates(1);
        assert_eq!(r.len(), 1);
        assert!((r[0].1 - 1000.0).abs() < 0.01);
        assert!((r[0].2 - 100.0).abs() < 0.01);
    }

    #[test]
    fn averages_over_window() {
        let mut w = WindowTracker::new(10);
        w.push(&[(app("chrome"), 1000, 0)]);
        w.push(&[(app("chrome"), 3000, 0)]);

        // 两秒共 4000 字节 → 平均 2000 B/s
        let r = w.rates(2);
        assert!((r[0].1 - 2000.0).abs() < 0.01);
    }

    #[test]
    fn absent_apps_get_zero_slots_to_keep_alignment() {
        let mut w = WindowTracker::new(10);
        w.push(&[(app("a"), 100, 0), (app("b"), 100, 0)]);
        // 只有 a 有流量
        w.push(&[(app("a"), 100, 0)]);

        // b 在最近 2 秒只有 100 字节 → 平均 50
        let r = w.rates(2);
        let b = r.iter().find(|(k, _, _)| k.as_str() == "b").unwrap();
        assert!((b.1 - 50.0).abs() < 0.01, "得到 {}", b.1);
    }

    #[test]
    fn window_smaller_than_history_uses_tail() {
        let mut w = WindowTracker::new(10);
        for _ in 0..5 {
            w.push(&[(app("a"), 1000, 0)]);
        }
        // 只看最近 2 秒
        let r = w.rates(2);
        assert!((r[0].1 - 1000.0).abs() < 0.01);
    }

    #[test]
    fn respects_max_slots() {
        let mut w = WindowTracker::new(3);
        for _ in 0..10 {
            w.push(&[(app("a"), 100, 0)]);
        }
        // 窗口被限制在 3 秒，最多 300 字节
        let r = w.rates(100);
        assert!((r[0].1 - 100.0).abs() < 0.01, "得到 {}", r[0].1);
    }

    #[test]
    fn series_length_matches_slots() {
        let mut w = WindowTracker::new(10);
        for i in 0..4 {
            w.push(&[(app("a"), i as u64 * 100, 0)]);
        }
        assert_eq!(w.series(10).len(), 4);
        assert_eq!(w.series(2).len(), 2);
    }

    #[test]
    fn sweep_removes_idle_apps() {
        let mut w = WindowTracker::new(10);
        w.push(&[(app("busy"), 500, 0), (app("idle"), 500, 0)]);
        w.push(&[(app("busy"), 500, 0)]);
        w.sweep_idle(1); // 只看最近 1 秒
        assert_eq!(w.app_count(), 1);
        assert_eq!(w.rates(2)[0].0.as_str(), "busy");
    }

    #[test]
    fn zero_traffic_apps_are_not_listed() {
        let mut w = WindowTracker::new(10);
        w.push(&[(app("a"), 0, 0)]);
        assert!(w.rates(1).is_empty());
    }

    #[test]
    fn short_history_divides_by_covered_seconds() {
        // 刚启动 3 秒就切到「1 分钟」：分母应是 3（已覆盖的秒数），不是 60。
        // 按 60 算的话，1000 B/s 会显示成 50 B/s，看着像采样坏了。
        let mut w = WindowTracker::new(3600);
        for _ in 0..3 {
            w.push(&[(app("a"), 1000, 0)]);
        }
        let r = w.rates(60);
        assert!((r[0].1 - 1000.0).abs() < 0.01, "得到 {}", r[0].1);
        let (rx, _) = w.total_rate(60);
        assert!((rx - 1000.0).abs() < 0.01, "得到 {rx}");
    }
}
