//! 采样驱动线程：把 [`Sampler`] 的输出算成速率快照，通过 channel 推给界面。
//!
//! 界面线程只做渲染，任何可能阻塞的动作（起子进程、读流、syscall、写数据库）
//! 都在这里，保证托盘弹窗永远不会因为采样或落盘而掉帧。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tm_core::{AppKey, AppRate, RateCalculator, WindowTracker, MAX_SLOTS};
use tm_sampler::{MockSampler, Sampler, SamplerError};
use tm_store::Store;

/// 多久往库里落一次盘。太频繁白白增加 IO，太稀疏又怕丢数据，5 秒是个折中。
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// 多久刷新一次「24 小时」合计
const DAY_QUERY_EVERY: Duration = Duration::from_secs(30);
/// 多久清一次过期数据
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// 不同时间窗口下的应用速率。界面的分段控件直接取这三份。
#[derive(Debug, Clone, Default)]
pub struct WindowedRates {
    pub min1: Vec<AppRate>,
    pub min5: Vec<AppRate>,
    pub hour1: Vec<AppRate>,
    /// 最近 10 分钟的平均速率。只给流量提醒用：拿它乘 600 就是这 10 分钟的字节数
    pub min10: Vec<AppRate>,
}

/// 一帧给界面的快照。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 实时（平滑后的瞬时速率）
    pub rates: Vec<AppRate>,
    pub windowed: WindowedRates,
    pub total_rx: f64,
    pub total_tx: f64,
    /// 本次运行累计
    pub session_rx: u64,
    pub session_tx: u64,
    /// 最近 24 小时累计（来自历史库；库打不开时一直是 0）
    pub day_rx: u64,
    pub day_tx: u64,
}

/// 启动采样线程。`on_change` 在每帧到达后调用（用来唤醒界面重绘）。
///
/// `retention_days` 由界面线程写、这里读：用户在设置里改保留天数后，
/// 下一次整点清理就会按新值来，不需要重启程序。
pub fn spawn<F>(on_change: F, retention_days: Arc<AtomicI64>) -> Receiver<Snapshot>
where
    F: Fn() + Send + 'static,
{
    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("sampler".to_owned())
        .spawn(move || {
            let mut sampler = build_sampler();
            let mut calc = RateCalculator::new();
            let mut tracker = WindowTracker::new(MAX_SLOTS);
            let mut store = open_store();

            // 逐应用上一次的「本次累计」，用来算增量
            let mut prev_session: HashMap<AppKey, (u64, u64)> = HashMap::new();
            let mut pending: Vec<(AppKey, u64, u64)> = Vec::new();

            let mut last_flush = Instant::now();
            let mut last_day_query = Instant::now() - DAY_QUERY_EVERY;
            let mut last_prune = Instant::now();
            let mut day = (0u64, 0u64);
            let mut last_err: Option<String> = None;

            // `TM_TRACE=1` 时把每帧间隔打出来。判断「采样是不是真的 1 秒一帧」
            // 只看这一个数字，比盯界面猜快得多。
            let trace = std::env::var_os("TM_TRACE").is_some();
            let mut last_frame: Option<Instant> = None;
            let mut frame_no: u64 = 0;

            loop {
                match sampler.sample() {
                    Ok(samples) => {
                        last_err = None;

                        frame_no += 1;
                        if trace {
                            let dt = last_frame.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                            eprintln!(
                                "[trace] 帧 {frame_no} 行 {} 间隔 {dt:.2}s",
                                samples.len()
                            );
                            last_frame = Some(Instant::now());
                        }

                        let now = Instant::now();
                        let rates = calc.update(now, &samples);
                        let (rx_bps, tx_bps) = calc.total_bps();
                        let (session_rx, session_tx) = calc.session_total();

                        // 1) 逐应用算增量，喂给窗口聚合和待落盘队列
                        let mut deltas: Vec<(AppKey, u64, u64)> = Vec::with_capacity(rates.len());
                        for r in &rates {
                            let cur = (r.rx_session, r.tx_session);
                            if let Some(prev) = prev_session.get(&r.app) {
                                let drx = cur.0.saturating_sub(prev.0);
                                let dtx = cur.1.saturating_sub(prev.1);
                                if drx > 0 || dtx > 0 {
                                    deltas.push((r.app.clone(), drx, dtx));
                                }
                            }
                            prev_session.insert(r.app.clone(), cur);
                        }
                        tracker.push(&deltas);
                        pending.extend(deltas);

                        // 2) 定期落盘
                        let mut flushed = false;
                        if last_flush.elapsed() >= FLUSH_EVERY {
                            if let Some(s) = store.as_mut() {
                                if let Err(e) = s.add_deltas(SystemTime::now(), &pending) {
                                    eprintln!("[store] 写入失败：{e}");
                                }
                            }
                            pending.clear();
                            last_flush = Instant::now();
                            flushed = true;
                        }

                        // 3) 定期刷新 24 小时合计。
                        // 刚写过盘且还没有历史数时立刻补查一次，否则顶部会先显示
                        // 「本次」再突然跳成「24小时」，看起来像 bug。
                        let day_unknown = day.0 + day.1 == 0;
                        if last_day_query.elapsed() >= DAY_QUERY_EVERY
                            || (flushed && day_unknown)
                        {
                            if let Some(s) = store.as_ref() {
                                let now_secs = unix_now();
                                match s.totals_between(now_secs - 86_400, now_secs + 60) {
                                    Ok(rows) => {
                                        day = rows.iter().fold((0u64, 0u64), |acc, r| {
                                            (acc.0 + r.rx, acc.1 + r.tx)
                                        });
                                    }
                                    Err(e) => eprintln!("[store] 查询失败：{e}"),
                                }
                            }
                            last_day_query = Instant::now();
                        }

                        // 4) 每小时清理过期数据和空闲应用
                        if last_prune.elapsed() >= PRUNE_EVERY {
                            if let Some(s) = store.as_mut() {
                                let days = retention_days.load(Ordering::Relaxed).clamp(1, 365);
                                let cutoff = unix_now() - days * 86_400;
                                let _ = s.prune_before(cutoff);
                            }
                            tracker.sweep_idle(MAX_SLOTS);
                            last_prune = Instant::now();
                        }

                        let snap = Snapshot {
                            rates,
                            windowed: WindowedRates {
                                min1: to_rates(tracker.rates(60)),
                                min5: to_rates(tracker.rates(300)),
                                hour1: to_rates(tracker.rates(3600)),
                                min10: to_rates(tracker.rates(600)),
                            },
                            total_rx: rx_bps,
                            total_tx: tx_bps,
                            session_rx,
                            session_tx,
                            day_rx: day.0,
                            day_tx: day.1,
                        };

                        // 第一帧只是基准：`RateCalculator` 要两个采样点才能差分，
                        // 这一帧的速率必然全是 0、应用列表必然是空的。发上去的话
                        // 界面会先闪一下「当前没有流量 / 0 B/s」，看着像坏了。
                        // 直接不发，让界面保持骨架屏，等第二帧的真实数据。
                        if frame_no == 1 {
                            continue;
                        }

                        if tx.send(snap).is_err() {
                            break; // 界面已退出
                        }
                        on_change();
                    }
                    Err(SamplerError::Timeout) => {
                        // 偶尔卡一次不算错，继续等下一帧
                        continue;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if last_err.as_deref() != Some(msg.as_str()) {
                            eprintln!("[sampler] {msg}");
                            last_err = Some(msg);
                        }
                        thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        })
        .expect("无法启动采样线程");

    rx
}

/// 把窗口聚合结果转成界面认识的 [`AppRate`]。
/// 窗口视图不携带「本次累计」，所以 session 字段填 0，界面也不用它。
fn to_rates(v: Vec<(AppKey, f64, f64)>) -> Vec<AppRate> {
    v.into_iter()
        .map(|(app, rx, tx)| AppRate {
            app,
            rx_bps: rx,
            tx_bps: tx,
            rx_session: 0,
            tx_session: 0,
        })
        .collect()
}

/// 历史库位置。界面导出 CSV 时要打开同一个文件，所以放在这里统一算。
pub fn db_path() -> std::path::PathBuf {
    std::env::var_os("TM_DB_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(tm_store::default_path)
}

fn open_store() -> Option<Store> {
    let path = db_path();

    match Store::open(&path) {
        Ok(s) => {
            eprintln!("[store] 历史库：{}", path.display());
            Some(s)
        }
        Err(e) => {
            eprintln!("[store] 打开历史库失败（{e}），本次运行不记录历史");
            None
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 按平台挑采样器。Windows 走 ETW，macOS 走 nettop，其他平台给假数据。
fn build_sampler() -> Box<dyn Sampler> {
    if std::env::var_os("TM_FORCE_MOCK").is_some() {
        eprintln!("[sampler] 使用演示数据（TM_FORCE_MOCK）");
        return Box::new(MockSampler::new());
    }

    #[cfg(target_os = "macos")]
    {
        match tm_macos::NettopSampler::spawn() {
            Ok(s) => return Box::new(s),
            Err(e) => eprintln!("[sampler] nettop 启动失败（{e}），退回演示数据"),
        }
    }

    #[cfg(windows)]
    {
        match tm_windows::WindowsSampler::spawn() {
            Ok(s) => return Box::new(s),
            Err(e) => eprintln!("[sampler] ETW 启动失败（{e}），退回演示数据"),
        }
    }

    Box::new(MockSampler::new())
}
