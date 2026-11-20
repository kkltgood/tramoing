//! 速率计算：把「累计字节」差分 + 平滑成「字节/秒」，并按应用归并。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::types::{AppKey, AppRate, ProcSample};

/// 上一帧某个 pid 的计数器。
#[derive(Debug, Clone)]
struct Prev {
    app: AppKey,
    rx: u64,
    tx: u64,
}

/// 有状态速率计算器。
///
/// 设计要点：
/// 1. 按 **pid** 保存基线（不是按 app），因为同一应用有多个进程，各自计数独立；
/// 2. 同一个 pid 换了 app 说明 pid 被系统复用，这一帧不做差分，避免出现天文数字；
/// 3. 计数器回退（进程重启后从 0 开始）时按「从 0 重新计」处理，不产生负值；
/// 4. 输出做 EMA 平滑，否则 1s 采样会让数字疯狂跳动，看不清趋势。
#[derive(Debug)]
pub struct RateCalculator {
    prev: HashMap<u32, Prev>,
    ema: HashMap<AppKey, (f64, f64)>,
    session: HashMap<AppKey, (u64, u64)>,
    alpha: f64,
    last_at: Option<Instant>,
    /// 两次更新间隔小于此值就丢弃这一帧，防止采样抖动导致速率被放大。
    min_dt: Duration,
    deltas: Vec<(AppKey, u64, u64)>,
    new_counters_start_at_zero: bool,
}

impl Default for RateCalculator {
    fn default() -> Self {
        Self::new()
    }
}

impl RateCalculator {
    pub fn new() -> Self {
        // 0.7 是「跟得上」和「不抖」的折中：1 秒一帧时，阶跃变化两帧（2 秒）
        // 就能收敛到 95% 以上。0.5 更顺滑，但菜单栏上的数字会明显慢半拍，
        // 用户会以为采样没生效。
        Self::with_smoothing(0.7)
    }

    /// `alpha` 是 **1 秒间隔下**的权重，越大越贴近瞬时值，越小越平滑。
    /// 实际帧间隔不是 1 秒时会按 `1-(1-alpha)^dt` 换算，
    /// 保证「平滑的时间常数」不随采样抖动而变。
    pub fn with_smoothing(alpha: f64) -> Self {
        Self {
            prev: HashMap::new(),
            ema: HashMap::new(),
            session: HashMap::new(),
            alpha: alpha.clamp(0.05, 1.0),
            last_at: None,
            min_dt: Duration::from_millis(100),
            deltas: Vec::new(),
            new_counters_start_at_zero: false,
        }
    }

    /// 喂入一帧采样，返回按速率降序排好的应用列表。
    ///
    /// 第一帧只建立基线，返回空列表（此时没有速率可言）。
    ///
    /// 界面那一两秒的空档由骨架屏承担，不要在这里返回「零速率列表」：
    /// 那会把全部进程都列出来（本机 26 个），下一帧只剩真正有流量的几个，
    /// 列表在一秒内从 26 行塌到 3 行，比空白更晃眼。
    pub fn update(&mut self, now: Instant, samples: &[ProcSample]) -> Vec<AppRate> {
        self.deltas.clear();
        let dt = match self.last_at {
            None => {
                self.rebuild_baseline(samples);
                self.last_at = Some(now);
                return Vec::new();
            }
            Some(t) => now.saturating_duration_since(t).as_secs_f64(),
        };

        if dt < self.min_dt.as_secs_f64() {
            return self.snapshot();
        }

        // 1) 逐进程差分，并按 app 累加
        let mut per_app: HashMap<&AppKey, (f64, f64)> = HashMap::new();
        let mut deltas: HashMap<AppKey, (u64, u64)> = HashMap::new();
        for s in samples {
            let (drx, dtx) = match self.prev.get(&s.pid) {
                Some(p) if p.app == s.app => (
                    // 同一个 pid 同一个应用，计数器却变小了 —— 只可能是底层计数器重置。
                    // 此时把新值整体当作增量（相当于「从 0 重新计」），
                    // 而不是 saturating_sub 得 0 把这段流量丢掉。
                    counter_delta(s.rx_total, p.rx),
                    counter_delta(s.tx_total, p.tx),
                ),
                // 首次见到的 pid，或 pid 被复用：本帧无基线，不计增量
                _ if self.new_counters_start_at_zero => (s.rx_total, s.tx_total),
                _ => (0, 0),
            };

            let entry = per_app.entry(&s.app).or_insert((0.0, 0.0));
            entry.0 += drx as f64 / dt;
            entry.1 += dtx as f64 / dt;
            if drx > 0 || dtx > 0 {
                let d = deltas.entry(s.app.clone()).or_default();
                d.0 = d.0.saturating_add(drx);
                d.1 = d.1.saturating_add(dtx);
            }

            let sess = self.session.entry(s.app.clone()).or_insert((0, 0));
            sess.0 = sess.0.saturating_add(drx);
            sess.1 = sess.1.saturating_add(dtx);
        }
        self.deltas = deltas
            .into_iter()
            .map(|(app, (rx, tx))| (app, rx, tx))
            .collect();

        // 2) EMA 平滑；本帧没出现的应用向 0 衰减，避免残留旧速度
        //    权重按实际间隔换算：间隔越长，越该相信这一帧的瞬时值。
        let a = 1.0 - (1.0 - self.alpha).powf(dt);
        for (app, (rx, tx)) in &per_app {
            match self.ema.get_mut(*app) {
                Some(e) => {
                    e.0 = a * rx + (1.0 - a) * e.0;
                    e.1 = a * tx + (1.0 - a) * e.1;
                }
                // 首次出现的应用直接取瞬时值。若也从 0 开始平滑，
                // 启动后第一秒会显示成真实速度的一半，观感很差。
                None => {
                    self.ema.insert((*app).clone(), (*rx, *tx));
                }
            }
        }
        for (app, e) in self.ema.iter_mut() {
            if !per_app.contains_key(app) {
                e.0 *= 1.0 - a;
                e.1 *= 1.0 - a;
            }
            // 不到 1 B/s 的一律归零。每帧都出现的进程（挂着但没流量）走的是
            // `a*0 + (1-a)*prev` 这条指数衰减，永远到不了 0，于是列表底部会堆一串
            // 「0 B/s / 0 B/s」的空行 —— 界面上显示的就是 0，留着没有任何信息量。
            if e.0 < 1.0 {
                e.0 = 0.0;
            }
            if e.1 < 1.0 {
                e.1 = 0.0;
            }
        }
        self.ema.retain(|_, (rx, tx)| *rx > 0.0 || *tx > 0.0);

        // 3) 重建基线（同时清掉已经消失的 pid）
        self.rebuild_baseline(samples);
        self.last_at = Some(now);

        self.snapshot()
    }

    fn rebuild_baseline(&mut self, samples: &[ProcSample]) {
        self.prev.clear();
        for s in samples {
            self.prev.insert(
                s.pid,
                Prev {
                    app: s.app.clone(),
                    rx: s.rx_total,
                    tx: s.tx_total,
                },
            );
        }
    }

    /// 采集器自行从零累计的计数器：启动基线之后，新出现的进程首帧也是流量。
    pub fn set_new_counters_start_at_zero(&mut self, value: bool) {
        self.new_counters_start_at_zero = value;
    }

    /// 原始整数增量，独立于 EMA 和 UI 的最小显示阈值。
    pub fn take_deltas(&mut self) -> Vec<(AppKey, u64, u64)> {
        std::mem::take(&mut self.deltas)
    }

    /// 数据源重建后重新取基线，保留用户的本次累计。
    pub fn reset_baseline(&mut self) {
        self.prev.clear();
        self.ema.clear();
        self.deltas.clear();
        self.last_at = None;
    }

    pub fn clear_rates(&mut self) {
        self.ema.clear();
    }

    /// 当前平滑后的快照，不推进时间。UI 在弹窗关闭期间可以反复取。
    pub fn snapshot(&self) -> Vec<AppRate> {
        let mut out: Vec<AppRate> = self
            .ema
            .iter()
            .map(|(app, (rx, tx))| {
                let (srx, stx) = self.session.get(app).copied().unwrap_or((0, 0));
                AppRate {
                    app: app.clone(),
                    rx_bps: *rx,
                    tx_bps: *tx,
                    rx_session: srx,
                    tx_session: stx,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.total_bps()
                .partial_cmp(&a.total_bps())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// 所有应用当前速率之和，菜单栏数字用这个（不必遍历 snapshot）。
    pub fn total_bps(&self) -> (f64, f64) {
        self.ema
            .values()
            .fold((0.0, 0.0), |acc, (rx, tx)| (acc.0 + rx, acc.1 + tx))
    }

    /// 会话累计。
    pub fn session_total(&self) -> (u64, u64) {
        self.session.values().fold((0u64, 0u64), |acc, (rx, tx)| {
            (acc.0.saturating_add(*rx), acc.1.saturating_add(*tx))
        })
    }
}

/// 计数器差分。若新值小于旧值，说明底层计数器重置了，
/// 此时把新值整体当作增量，而不是 saturating_sub 得 0 把这段流量丢掉。
fn counter_delta(cur: u64, prev: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else {
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_totals_saturate_across_apps() {
        let mut calc = RateCalculator::new();
        let start = Instant::now();
        calc.update(start, &[s(1, "a", 0, 0), s(2, "b", 0, 0)]);
        calc.update(
            start + Duration::from_secs(1),
            &[s(1, "a", u64::MAX, u64::MAX), s(2, "b", 1, 1)],
        );
        assert_eq!(calc.session_total(), (u64::MAX, u64::MAX));
        assert_eq!(calc.snapshot()[0].session_total(), u64::MAX);
    }

    #[test]
    fn ledger_preserves_first_delta_and_sub_display_threshold_bytes() {
        let mut c = RateCalculator::new();
        let t = Instant::now();
        c.update(t, &[s(1, "a", 0, 0)]);
        c.update(t + Duration::from_secs(2), &[s(1, "a", 1, 0)]);
        assert!(c.snapshot().is_empty());
        assert_eq!(c.take_deltas(), vec![(AppKey::from("a"), 1, 0)]);
        assert!(c.take_deltas().is_empty());
    }

    #[test]
    fn source_owned_counters_include_new_app_first_bytes() {
        let mut c = RateCalculator::new();
        c.set_new_counters_start_at_zero(true);
        let t = Instant::now();
        c.update(t, &[]);
        c.update(t + Duration::from_secs(1), &[s(2, "new", 10_000, 200)]);
        assert_eq!(c.session_total(), (10_000, 200));
    }

    fn s(pid: u32, app: &str, rx: u64, tx: u64) -> ProcSample {
        ProcSample::new(pid, app, rx, tx)
    }

    #[test]
    fn first_frame_establishes_baseline() {
        let mut c = RateCalculator::new();
        let t0 = Instant::now();
        let out = c.update(t0, &[s(1, "chrome", 1000, 500)]);
        assert!(out.is_empty(), "首帧不应产出速率");
    }

    #[test]
    fn second_frame_yields_rate() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "chrome", 0, 0)]);
        let out = c.update(t0 + Duration::from_secs(1), &[s(1, "chrome", 2048, 1024)]);

        assert_eq!(out.len(), 1);
        assert!((out[0].rx_bps - 2048.0).abs() < 1.0);
        assert!((out[0].tx_bps - 1024.0).abs() < 1.0);
        assert_eq!(out[0].rx_session, 2048);
    }

    #[test]
    fn merges_multiple_pids_of_same_app() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "chrome", 0, 0), s(2, "chrome", 0, 0)]);
        let out = c.update(
            t0 + Duration::from_secs(1),
            &[s(1, "chrome", 1000, 0), s(2, "chrome", 3000, 0)],
        );

        assert_eq!(out.len(), 1, "两个 renderer 应合并成一条");
        assert!((out[0].rx_bps - 4000.0).abs() < 1.0);
    }

    #[test]
    fn counter_reset_does_not_explode() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "app", 10_000_000, 0)]);
        // 进程重启，计数器回到 100 —— 按「从 0 重计」处理，增量应为 100 而不是负数/巨值
        let out = c.update(t0 + Duration::from_secs(1), &[s(1, "app", 100, 0)]);

        assert_eq!(out.len(), 1);
        assert!(
            (out[0].rx_bps - 100.0).abs() < 1.0,
            "得到 {}",
            out[0].rx_bps
        );
    }

    #[test]
    fn pid_reuse_is_ignored_for_one_frame() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(42, "old", 5_000_000, 0)]);
        // 同一个 pid 换了应用：系统复用了 pid，这一帧不应把 5000000 当成新应用的流量
        let out = c.update(t0 + Duration::from_secs(1), &[s(42, "new", 9_000_000, 0)]);
        assert!(out.is_empty(), "pid 复用帧应被跳过");

        // 下一帧才正常计算
        let out = c.update(t0 + Duration::from_secs(2), &[s(42, "new", 9_001_000, 0)]);
        assert_eq!(out.len(), 1);
        assert!((out[0].rx_bps - 1000.0).abs() < 1.0);
    }

    #[test]
    fn idle_app_decays_to_zero() {
        let mut c = RateCalculator::with_smoothing(0.5);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "app", 0, 0)]);
        c.update(t0 + Duration::from_secs(1), &[s(1, "app", 1000, 0)]);
        // 之后这个应用不再出现，速率应逐步衰减直到被移除
        let mut t = t0 + Duration::from_secs(1);
        for _ in 0..20 {
            t += Duration::from_secs(1);
            c.update(t, &[s(2, "other", 0, 0)]);
        }
        let out = c.snapshot();
        assert!(!out.iter().any(|r| r.app.as_str() == "app"), "应已衰减移除");
    }

    #[test]
    fn idle_but_alive_app_drops_out_of_the_list() {
        // 进程还活着、每秒都出现在采样里，但一点流量都没有。
        // EMA 走 a*0+(1-a)*prev 永远到不了 0，以前会一直挂在列表底部显示 0 B/s。
        let mut c = RateCalculator::with_smoothing(0.7);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "app", 0, 0), s(2, "idle", 0, 0)]);
        c.update(
            t0 + Duration::from_secs(1),
            &[s(1, "app", 5000, 0), s(2, "idle", 0, 0)],
        );

        let mut t = t0 + Duration::from_secs(1);
        for _ in 0..20 {
            t += Duration::from_secs(1);
            c.update(t, &[s(1, "app", 5000, 0), s(2, "idle", 0, 0)]);
        }
        let out = c.snapshot();
        assert!(
            !out.iter().any(|r| r.app.as_str() == "idle"),
            "空转的应用应该从列表里消失"
        );
    }

    #[test]
    fn short_interval_is_dropped() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "app", 0, 0)]);
        // 间隔 10ms，应被丢弃，不产生离谱的高速率
        let out = c.update(t0 + Duration::from_millis(10), &[s(1, "app", 100_000, 0)]);
        assert!(out.is_empty());
    }

    #[test]
    fn first_seen_app_is_not_halved() {
        // 首次出现的应用应直接显示真实速率，而不是被平滑成一半
        let mut c = RateCalculator::new();
        let t0 = Instant::now();
        c.update(t0, &[s(1, "new", 0, 0)]);
        let out = c.update(t0 + Duration::from_secs(1), &[s(1, "new", 1000, 0)]);
        assert!(
            (out[0].rx_bps - 1000.0).abs() < 1.0,
            "得到 {}",
            out[0].rx_bps
        );
    }

    #[test]
    fn smoothing_follows_actual_interval() {
        // 同样是一帧，间隔 3 秒时应该几乎完全采信新值 ——
        // 说明平滑权重跟着 dt 走，不会因为采样变稀疏就慢半拍。
        let mut c = RateCalculator::new();
        let t0 = Instant::now();
        c.update(t0, &[s(1, "app", 0, 0)]);
        c.update(t0 + Duration::from_secs(1), &[s(1, "app", 100_000, 0)]);
        let out = c.update(t0 + Duration::from_secs(4), &[s(1, "app", 1_300_000, 0)]);
        // 增量 1.2MB / 3s = 400KB/s；若用固定 0.7 只有 310KB/s
        assert!(out[0].rx_bps > 380_000.0, "得到 {}", out[0].rx_bps);
    }

    #[test]
    fn sorted_by_total_rate() {
        let mut c = RateCalculator::with_smoothing(1.0);
        let t0 = Instant::now();
        c.update(t0, &[s(1, "small", 0, 0), s(2, "big", 0, 0)]);
        let out = c.update(
            t0 + Duration::from_secs(1),
            &[s(1, "small", 10, 0), s(2, "big", 9999, 5000)],
        );
        assert_eq!(out[0].app.as_str(), "big");
        assert_eq!(out[1].app.as_str(), "small");
    }
}
