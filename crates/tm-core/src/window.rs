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

use std::collections::{HashMap, VecDeque};

use crate::types::AppKey;

/// 一个槽位存一秒的原始字节，保留 u64 精度以覆盖高速网卡和延迟采样。
type Slot = (u64, u64);

/// 最多记住 1 小时
pub const MAX_SLOTS: usize = 3600;

/// 主面板的三个窗口和流量提醒窗口。其他长度仍支持按槽位求和。
const CACHED_WINDOWS: [usize; 4] = [60, 300, 600, MAX_SLOTS];

#[derive(Debug)]
struct History {
    slots: VecDeque<Slot>,
    // 必须保留超出 u64 的部分，否则饱和后移出旧桶会错误地扣掉仍在窗口内的字节。
    sums: [(u128, u128); CACHED_WINDOWS.len()],
}

impl History {
    fn zeros(len: usize) -> Self {
        Self {
            slots: std::iter::repeat_n((0, 0), len).collect(),
            sums: [(0, 0); CACHED_WINDOWS.len()],
        }
    }

    /// 一次推进多个空桶；先移出过期槽位，避免满环插入时触发不必要的扩容。
    fn advance(&mut self, count: usize, max_slots: usize) {
        if count >= max_slots {
            self.slots.clear();
            self.slots.resize(max_slots, (0, 0));
            self.sums.fill((0, 0));
            return;
        }

        let len = self.slots.len();
        for (sum, window) in self.sums.iter_mut().zip(CACHED_WINDOWS) {
            let n = window.min(max_slots);
            if count >= n {
                *sum = (0, 0);
            } else {
                let start = len.saturating_sub(n);
                let end = len.saturating_sub(n - count);
                for (rx, tx) in self.slots.range(start..end) {
                    sum.0 -= *rx as u128;
                    sum.1 -= *tx as u128;
                }
            }
        }
        let expired = count.saturating_sub(max_slots - len);
        self.slots.drain(..expired);
        self.slots.resize(self.slots.len() + count, (0, 0));
    }

    fn add_at(&mut self, offset: usize, rx: u64, tx: u64) {
        if let Some(index) = self.slots.len().checked_sub(offset + 1) {
            let slot = &mut self.slots[index];
            let updated = (slot.0.saturating_add(rx), slot.1.saturating_add(tx));
            for (sum, window) in self.sums.iter_mut().zip(CACHED_WINDOWS) {
                if offset < window {
                    sum.0 += (updated.0 - slot.0) as u128;
                    sum.1 += (updated.1 - slot.1) as u128;
                }
            }
            *slot = updated;
        }
    }

    fn sum_last(&self, n: usize, max_slots: usize) -> Slot {
        if let Some(index) = CACHED_WINDOWS
            .iter()
            .position(|window| (*window).min(max_slots) == n)
        {
            let (rx, tx) = self.sums[index];
            return (
                rx.min(u64::MAX as u128) as u64,
                tx.min(u64::MAX as u128) as u64,
            );
        }
        self.slots
            .iter()
            .rev()
            .take(n)
            .fold((0u64, 0u64), |(rx, tx), (r, t)| {
                (rx.saturating_add(*r), tx.saturating_add(*t))
            })
    }
}

#[derive(Debug)]
pub struct WindowTracker {
    max_slots: usize,
    per_app: HashMap<AppKey, History>,
    total: History,
    clock: Option<std::time::Instant>,
    started: Option<std::time::Instant>,
    elapsed: f64,
}

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
            total: History::zeros(0),
            clock: None,
            started: None,
            elapsed: 0.0,
        }
    }

    /// 推入这一秒的增量。同一应用的多条记录会合并，缺的应用自动补 0。
    pub fn push(&mut self, deltas: &[(AppKey, u64, u64)]) {
        self.advance(1);
        self.add_deltas_at(0, deltas);
    }

    fn advance(&mut self, count: usize) {
        if count >= self.max_slots {
            // 休眠超过完整窗口时，所有应用历史都已失效，无需逐秒逐应用补零。
            self.per_app.clear();
        } else {
            for history in self.per_app.values_mut() {
                history.advance(count, self.max_slots);
            }
        }
        self.total.advance(count, self.max_slots);
    }

    fn add_deltas_at(&mut self, offset: usize, deltas: &[(AppKey, u64, u64)]) {
        if offset >= self.total.slots.len() {
            return;
        }
        for (app, rx, tx) in deltas {
            if let Some(history) = self.per_app.get_mut(app) {
                history.add_at(offset, *rx, *tx);
            } else {
                let mut history = History::zeros(self.total.slots.len());
                history.add_at(offset, *rx, *tx);
                self.per_app.insert(app.clone(), history);
            }
            self.total.add_at(offset, *rx, *tx);
        }
    }

    /// 按真实单调时钟推进；超时/休眠期间补空桶，同一秒的数据合并。
    /// 积压帧回填原来的秒桶；已过保留期或早于首次观测的数据不进入窗口。
    /// 输入为观测到的增量，无法从累计采样反推丢失期间的逐包时间。
    pub fn push_at(&mut self, now: std::time::Instant, deltas: &[(AppKey, u64, u64)]) {
        let started = *self.started.get_or_insert(now);
        if now < started {
            return;
        }
        self.elapsed = self.elapsed.max(now.duration_since(started).as_secs_f64());
        let Some(clock) = self.clock else {
            self.clock = Some(now);
            self.push(deltas);
            return;
        };
        let elapsed = now.saturating_duration_since(clock).as_secs();
        if elapsed > 0 {
            self.advance(elapsed.min(self.max_slots as u64) as usize);
            self.clock = Some(clock + std::time::Duration::from_secs(elapsed));
        }
        // clock 指向当前秒桶的起点；不足一秒的迟到也属于前一个桶。
        let age = clock.saturating_duration_since(now);
        let offset = age
            .as_secs()
            .saturating_add(u64::from(age.subsec_nanos() != 0));
        self.add_deltas_at(offset.min(self.max_slots as u64) as usize, deltas);
    }

    pub fn bytes(&self, secs: usize) -> Vec<(AppKey, u64, u64)> {
        let n = secs.clamp(1, self.max_slots);
        self.per_app
            .iter()
            .filter_map(|(app, d)| {
                let (rx, tx) = d.sum_last(n, self.max_slots);
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
                let (rx, tx) = d.sum_last(n, self.max_slots);
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
        let (rx, tx) = self.total.sum_last(n, self.max_slots);
        let denom = self.denominator(n);
        (rx as f64 / denom, tx as f64 / denom)
    }

    /// 最近 `secs` 秒每秒的总下载量，给折线图用。长度不足时前面少几个点。
    pub fn series(&self, secs: usize) -> Vec<f32> {
        let n = secs.clamp(1, self.max_slots);
        let skip = self.total.slots.len().saturating_sub(n);
        self.total
            .slots
            .iter()
            .skip(skip)
            .map(|(rx, _)| *rx as f32)
            .collect()
    }

    /// 清掉最近 `secs` 秒完全没动静的应用，防止长期运行后 map 只增不减。
    pub fn sweep_idle(&mut self, secs: usize) {
        let n = secs.clamp(1, self.max_slots);
        self.per_app.retain(|_, d| {
            let (rx, tx) = d.sum_last(n, self.max_slots);
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
            n.min(self.total.slots.len()).max(1) as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    #[test]
    fn duplicate_records_and_subsecond_updates_share_one_bucket() {
        let mut w = WindowTracker::default();
        w.push(&[(app("a"), 5, 1), (app("a"), 7, 2), (app("b"), 20, 0)]);
        assert!(w.bytes(1).contains(&(app("a"), 12, 3)));
        assert_eq!(w.series(60), vec![32.0]);
        w.push(&[]);
        assert!(w.bytes(1).is_empty());
        assert!(w.bytes(60).contains(&(app("a"), 12, 3)));

        let mut w = WindowTracker::default();
        let start = Instant::now();
        w.push_at(start, &[(app("a"), 5, 1), (app("a"), 7, 2)]);
        w.push_at(start + Duration::from_millis(900), &[(app("a"), 8, 3)]);
        assert_eq!(w.bytes(60), vec![(app("a"), 20, 6)]);
        assert_eq!(w.series(60), vec![20.0]);
        w.push_at(start + Duration::from_secs(60), &[]);
        assert!(w.bytes(60).is_empty());
        assert_eq!(w.bytes(300), vec![(app("a"), 20, 6)]);
    }

    #[test]
    fn cached_windows_keep_saturated_bytes_until_their_own_bucket_expires() {
        for window in CACHED_WINDOWS {
            let mut w = WindowTracker::default();
            let start = Instant::now();
            w.push_at(start, &[(app("a"), u64::MAX, 10)]);
            w.push_at(start + Duration::from_secs(1), &[(app("a"), u64::MAX, 20)]);
            // 同一桶饱和后新增的字节不能进入滚动累计，否则移出该桶时会残留。
            w.push_at(start + Duration::from_millis(1500), &[(app("a"), 99, 1)]);
            w.push_at(start + Duration::from_secs(2), &[(app("a"), 17, 30)]);
            w.push_at(start + Duration::from_secs(window as u64), &[]);
            assert_eq!(w.bytes(window), vec![(app("a"), u64::MAX, 51)]);
            w.push_at(start + Duration::from_secs(window as u64 + 1), &[]);
            assert_eq!(w.bytes(window), vec![(app("a"), 17, 30)]);
            assert_eq!(
                w.total_rate(window),
                (17.0 / window as f64, 30.0 / window as f64)
            );
            w.push_at(start + Duration::from_secs(window as u64 + 2), &[]);
            assert!(w.bytes(window).is_empty());
            assert_eq!(w.total_rate(window), (0.0, 0.0));
        }
    }

    #[test]
    fn windows_match_timestamped_reference_through_wraparound_and_gaps() {
        for max_slots in [1, 17, 60, 317, MAX_SLOTS, MAX_SLOTS + 10] {
            let mut w = WindowTracker::new(max_slots);
            let start = Instant::now();
            let mut millis = 0u64;
            let mut records = Vec::new();
            for tick in 0..4200u64 {
                if tick > 0 {
                    millis += match tick {
                        4000 => 86_400_000,
                        n if n % 211 == 0 => 61_000,
                        n => [0, 200, 800, 1000, 2200][n as usize % 5],
                    };
                }
                let second = millis / 1000;
                let deltas: Vec<_> = (0..3u64)
                    .filter(|index| tick % (index + 2) == 0)
                    .map(|index| (app(&format!("app-{index}")), tick + 1, index * 7))
                    .collect();
                for (app, rx, tx) in &deltas {
                    records.push((second, app.clone(), *rx, *tx));
                }
                w.push_at(start + Duration::from_millis(millis), &deltas);
                if tick % 101 != 0 && tick != 4000 && tick != 4199 {
                    continue;
                }

                for secs in [0, 1, 17, 59, 60, 61, 299, 300, 600, 3600, 5000] {
                    let n = secs.clamp(1, max_slots);
                    let from = second.saturating_sub(n as u64 - 1);
                    let mut expected = BTreeMap::<AppKey, Slot>::new();
                    let mut series = BTreeMap::<u64, u64>::new();
                    for (at, app, rx, tx) in &records {
                        if *at >= from {
                            let entry = expected.entry(app.clone()).or_default();
                            entry.0 += rx;
                            entry.1 += tx;
                            *series.entry(*at).or_default() += rx;
                        }
                    }
                    let expected: Vec<_> = expected
                        .into_iter()
                        .map(|(app, (rx, tx))| (app, rx, tx))
                        .collect();
                    let mut actual = w.bytes(secs);
                    actual.sort_by(|a, b| a.0.cmp(&b.0));
                    assert_eq!(
                        actual, expected,
                        "max={max_slots}, tick={tick}, secs={secs}"
                    );
                    let denom = (millis as f64 / 1000.0).min(n as f64).max(1.0);
                    let expected_rates: Vec<_> = expected
                        .iter()
                        .map(|(app, rx, tx)| (app.clone(), *rx as f64 / denom, *tx as f64 / denom))
                        .filter(|(_, rx, tx)| *rx >= 1.0 || *tx >= 1.0)
                        .collect();
                    let mut actual_rates = w.rates(secs);
                    assert!(actual_rates
                        .windows(2)
                        .all(|r| r[0].1 + r[0].2 >= r[1].1 + r[1].2));
                    actual_rates.sort_by(|a, b| a.0.cmp(&b.0));
                    assert_eq!(actual_rates, expected_rates);
                    let (rx, tx) = expected
                        .iter()
                        .fold((0u64, 0u64), |(rx, tx), (_, r, t)| (rx + r, tx + t));
                    assert_eq!(w.total_rate(secs), (rx as f64 / denom, tx as f64 / denom));
                    let expected_series: Vec<_> = (from..=second)
                        .map(|at| series.get(&at).copied().unwrap_or_default() as f32)
                        .collect();
                    assert_eq!(w.series(secs), expected_series);
                }
            }
        }
    }

    #[test]
    fn full_history_reuses_its_allocation_and_sleep_releases_expired_apps() {
        let mut w = WindowTracker::new(60);
        let start = Instant::now();
        w.push_at(start, &[]);
        w.push_at(start + Duration::from_secs(59), &[(app("a"), 100, 0)]);
        let capacity = w.per_app[&app("a")].slots.capacity();
        for tick in 60..120 {
            w.push_at(start + Duration::from_secs(tick), &[(app("a"), 100, 0)]);
        }
        assert_eq!(w.per_app[&app("a")].slots.capacity(), capacity);
        w.push_at(start + Duration::from_secs(86_400), &[]);
        assert_eq!(w.app_count(), 0);
        assert_eq!(w.series(60), vec![0.0; 60]);
        w.push_at(start + Duration::from_secs(86_401), &[(app("a"), 120, 60)]);
        assert_eq!(w.rates(60), vec![(app("a"), 2.0, 1.0)]);
    }

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
    fn queued_samples_keep_their_original_buckets_after_the_clock_advances() {
        let start = Instant::now();
        let mut w = WindowTracker::default();
        w.push_at(start, &[]);
        w.push_at(start + Duration::from_secs(121), &[]);
        w.push_at(start + Duration::from_secs(1), &[(app("old"), 6000, 1200)]);
        w.push_at(start + Duration::from_secs(120), &[(app("recent"), 60, 6)]);
        assert_eq!(w.bytes(60), vec![(app("recent"), 60, 6)]);
        assert_eq!(w.total_rate(300), (6060.0 / 121.0, 1206.0 / 121.0));
        assert_eq!(&w.series(3)[..], &[0.0, 60.0, 0.0]);

        w.push_at(start + Duration::from_secs(301), &[]);
        assert_eq!(w.bytes(300), vec![(app("recent"), 60, 6)]);
        w.push_at(start + Duration::from_secs(421), &[]);
        assert!(w.bytes(300).is_empty());
    }

    #[test]
    fn late_samples_respect_subsecond_boundaries_without_rewinding_warmup() {
        let start = Instant::now();
        let mut w = WindowTracker::new(60);
        w.push_at(start, &[]);
        w.push_at(start + Duration::from_millis(5500), &[]);
        w.push_at(start + Duration::from_millis(4999), &[(app("a"), 55, 0)]);
        assert!(w.bytes(1).is_empty());
        assert_eq!(w.rates(60), vec![(app("a"), 10.0, 0.0)]);
        w.push_at(start + Duration::from_secs(5), &[(app("a"), 11, 0)]);
        assert_eq!(w.bytes(1), vec![(app("a"), 11, 0)]);
        assert_eq!(w.series(2), vec![55.0, 11.0]);
    }

    #[test]
    fn expired_samples_are_dropped_and_late_saturated_buckets_expire_cleanly() {
        let start = Instant::now();
        let mut w = WindowTracker::new(60);
        w.push_at(start, &[]);
        w.push_at(start + Duration::from_secs(120), &[]);
        for at in [
            start - Duration::from_secs(1),
            start + Duration::from_secs(60),
        ] {
            w.push_at(at, &[(app("expired"), 100, 0)]);
        }
        assert_eq!(w.app_count(), 0);
        for _ in 0..2 {
            w.push_at(start + Duration::from_secs(61), &[(app("a"), u64::MAX, 0)]);
        }
        assert_eq!(w.bytes(60), vec![(app("a"), u64::MAX, 0)]);
        assert!(w.bytes(59).is_empty());
        w.push_at(start + Duration::from_secs(121), &[]);
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
