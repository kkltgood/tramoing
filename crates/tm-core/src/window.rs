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

/// 一个槽位存一秒的原始字节，保留 u64 精度以覆盖高速网卡和延迟采样。
type Slot = (u64, u64);

/// 最多记住 1 小时
pub const MAX_SLOTS: usize = 3600;

#[derive(Debug)]
pub struct WindowTracker {
    max_slots: usize,
    per_app: HashMap<AppKey, VecDeque>,
    total: VecDeque,
    clock: Option<std::time::Instant>,
    started: Option<std::time::Instant>,
    elapsed: f64,
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
            clock: None,
            started: None,
            elapsed: 0.0,
        }
    }

    /// 推入这一秒的增量。必须是「每个应用每秒恰好调用一次」，缺的应用会自动补 0。
    pub fn push(&mut self, deltas: &[(AppKey, u64, u64)]) {
        let mut seen: HashSet<&AppKey> = HashSet::with_capacity(deltas.len());
        let mut total_rx = 0u64;
        let mut total_tx = 0u64;

        for (app, rx, tx) in deltas {
            let history = self.total.len().min(self.max_slots.saturating_sub(1));
            let d = self
                .per_app
                .entry(app.clone())
                .or_insert_with(|| std::iter::repeat_n((0, 0), history).collect());
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

    /// 按真实单调时钟推进；超时/休眠期间补空桶，同一秒的数据合并。
    /// 输入为观测到的增量，无法从累计采样反推丢失期间的逐包时间。
    pub fn push_at(&mut self, now: std::time::Instant, deltas: &[(AppKey, u64, u64)]) {
        let started = *self.started.get_or_insert(now);
        self.elapsed = now.saturating_duration_since(started).as_secs_f64();
        let Some(clock) = self.clock else {
            self.clock = Some(now);
            self.push(deltas);
            return;
        };
        let elapsed = now.saturating_duration_since(clock).as_secs();
        if elapsed > 0 {
            for _ in 0..elapsed.min(self.max_slots as u64) {
                self.push(&[]);
            }
            self.clock = Some(clock + std::time::Duration::from_secs(elapsed));
        }
        for (app, rx, tx) in deltas {
            let d = self
                .per_app
                .entry(app.clone())
                .or_insert_with(|| std::iter::repeat_n((0, 0), self.total.len()).collect());
            if let Some(slot) = d.back_mut() {
                slot.0 = slot.0.saturating_add(*rx);
                slot.1 = slot.1.saturating_add(*tx);
            }
            if let Some(slot) = self.total.back_mut() {
                slot.0 = slot.0.saturating_add(*rx);
                slot.1 = slot.1.saturating_add(*tx);
            }
        }
    }

    pub fn bytes(&self, secs: usize) -> Vec<(AppKey, u64, u64)> {
        let n = secs.clamp(1, self.max_slots);
        self.per_app
            .iter()
            .filter_map(|(app, d)| {
                let (rx, tx) = sum_last(d, n);
                (rx > 0 || tx > 0).then(|| (app.clone(), rx, tx))
            })
            .collect()
    }

    /// 最近 `secs` 秒的平均速率（字节/秒），按总量降序。
    ///
    /// 分母取「实际攒到的秒数」和 `secs` 里小的那个：刚启动 5 秒时看「1小时」，
    /// 分母若按 3600 算，数字会小到像没在工作。攒满 `secs` 秒之后语义才回到
    /// 「最近 N 秒的平均」。
    pub fn rates(&self, secs: usize) -> Vec<(AppKey, f64, f64)> {
        let n = secs.clamp(1, self.max_slots);
        let denom = self.denominator(n);

        let mut out: Vec<(AppKey, f64, f64)> = self
            .per_app
            .iter()
            .filter_map(|(app, d)| {
                let (rx, tx) = sum_last(d, n);
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
        let denom = self.denominator(n);
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

    fn denominator(&self, n: usize) -> f64 {
        if self.started.is_some() {
            self.elapsed.min(n as f64).max(1.0)
        } else {
            n.min(self.total.len()).max(1) as f64
        }
    }
}

fn push_slot(d: &mut VecDeque, rx: u64, tx: u64, max_slots: usize) {
    d.push_back((rx, tx));
    while d.len() > max_slots {
        d.pop_front();
    }
}

fn sum_last(d: &VecDeque, n: usize) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for (r, t) in d.iter().rev().take(n) {
        rx = rx.saturating_add(*r);
        tx = tx.saturating_add(*t);
    }
    (rx, tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_app_uses_shared_window_and_retains_large_deltas() {
        let mut w = WindowTracker::default();
        for _ in 0..59 {
            w.push(&[]);
        }
        w.push(&[(app("late"), 6000, 0)]);
        assert_eq!(w.rates(60)[0].1, 100.0);
        assert_eq!(w.total_rate(60).0, 100.0);
        w.push(&[(app("large"), 1u64 << 34, 0)]);
        assert!(w.bytes(1).contains(&(app("large"), 1u64 << 34, 0)));
    }

    #[test]
    fn elapsed_time_expires_traffic_even_without_samples() {
        let mut w = WindowTracker::new(60);
        let t = std::time::Instant::now();
        w.push_at(t, &[(app("a"), 1000, 0)]);
        w.push_at(t + std::time::Duration::from_secs(61), &[]);
        assert!(w.bytes(60).is_empty());
        assert_eq!(w.total_rate(60), (0.0, 0.0));
    }

    #[test]
    fn timestamp_baseline_does_not_add_a_phantom_second() {
        let mut w = WindowTracker::default();
        let t = std::time::Instant::now();
        w.push_at(t, &[]);
        w.push_at(
            t + std::time::Duration::from_secs(1),
            &[(app("a"), 1000, 0)],
        );
        assert_eq!(w.rates(60)[0].1, 1000.0);
        assert_eq!(w.bytes(600)[0].1, 1000);
    }

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
