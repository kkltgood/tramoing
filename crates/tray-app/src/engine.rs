//! 后台采样、原始字节记账、按时间推进的窗口和可重试的历史写入。
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tm_core::{AppKey, AppRate, RateCalculator, WindowTracker, MAX_SLOTS};
use tm_sampler::{MockSampler, Sampler, SamplerError};
use tm_store::Store;

const FLUSH_EVERY: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(4);
const RESTART_AFTER: Duration = Duration::from_secs(15);
type Deltas = Vec<(AppKey, u64, u64)>;

#[derive(Debug, Clone, Default)]
pub struct WindowedRates {
    pub min1: Vec<AppRate>,
    pub min5: Vec<AppRate>,
    pub hour1: Vec<AppRate>,
    /// 提醒直接使用观测到的字节数，启动预热期也不会放大。
    pub min10_bytes: Deltas,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rates: Vec<AppRate>,
    pub windowed: WindowedRates,
    pub total_rx: f64,
    pub total_tx: f64,
    pub session_rx: u64,
    pub session_tx: u64,
    pub day_rx: u64,
    pub day_tx: u64,
    pub error: Option<String>,
    pub mock: bool,
    pub store_error: Option<String>,
}

/// App 持有线程所有权。正常退出时通知停止、提交最后一批，再回收采集器。
pub struct Engine {
    rx: Receiver<Snapshot>,
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn try_recv(&self) -> Result<Snapshot, mpsc::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 按采样发生的分钟合并；写失败时保留原时间，成功提交后才移除。
#[derive(Default)]
struct Pending(BTreeMap<i64, BTreeMap<AppKey, (u64, u64)>>);

#[derive(serde::Serialize, serde::Deserialize)]
struct Recovery {
    id: String,
    minutes: Vec<RecoveryMinute>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecoveryMinute {
    minute: u64,
    apps: Vec<(String, u64, u64)>,
}

fn recovery_dir() -> std::path::PathBuf {
    db_path().with_extension("pending")
}

impl Pending {
    fn save_recovery(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Ok(());
        }
        let dir = recovery_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("{stamp}-{}", std::process::id());
        let recovery = Recovery {
            id: id.clone(),
            minutes: self
                .0
                .iter()
                .map(|(minute, apps)| RecoveryMinute {
                    minute: *minute as u64,
                    apps: apps
                        .iter()
                        .map(|(app, (rx, tx))| (app.as_str().to_owned(), *rx, *tx))
                        .collect(),
                })
                .collect(),
        };
        let path = dir.join(format!("{id}.tmp"));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, &recovery).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&path, path.with_extension("json")).map_err(|e| e.to_string())
    }
}

fn recover_pending(store: &mut Store) -> Result<(), String> {
    let dir = recovery_dir();
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let recovery: Recovery = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        for RecoveryMinute { minute, apps } in recovery.minutes {
            let deltas: Deltas = apps
                .into_iter()
                .map(|(app, rx, tx)| (AppKey::new(app), rx, tx))
                .collect();
            store
                .add_recovered_deltas(
                    UNIX_EPOCH + Duration::from_secs(minute),
                    &deltas,
                    &format!("{}:{minute}", recovery.id),
                )
                .map_err(|e| e.to_string())?;
        }
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Pending {
    fn add(&mut self, at: SystemTime, deltas: &[(AppKey, u64, u64)]) {
        let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        for (app, rx, tx) in deltas {
            let entry = self
                .0
                .entry(secs / 60 * 60)
                .or_default()
                .entry(app.clone())
                .or_default();
            entry.0 = entry.0.saturating_add(*rx);
            entry.1 = entry.1.saturating_add(*tx);
        }
    }

    fn flush(&mut self, store: &mut Store) -> Result<(), tm_store::StoreError> {
        while let Some((&minute, apps)) = self.0.first_key_value() {
            let deltas: Deltas = apps
                .iter()
                .map(|(k, (rx, tx))| (k.clone(), *rx, *tx))
                .collect();
            store.add_deltas(UNIX_EPOCH + Duration::from_secs(minute as u64), &deltas)?;
            self.0.remove(&minute);
        }
        Ok(())
    }
}

pub fn spawn<F>(on_change: F, retention_days: Arc<AtomicI64>) -> Engine
where
    F: Fn() + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let (stop, stop_rx) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mock = std::env::var_os("TM_FORCE_MOCK").is_some();
            let mut sampler: Option<Box<dyn Sampler>> = None;
            let mut calc = RateCalculator::new();
            let mut tracker = WindowTracker::new(MAX_SLOTS);
            let mut pending = Pending::default();
            let mut store = None;
            let mut store_error = None;
            let mut last_flush = Instant::now() - FLUSH_EVERY;
            let mut last_prune = Instant::now();
            let mut last_good = Instant::now();
            let mut error = None;
            let mut day = (0, 0);
            let mut retry_at = Instant::now();
            let mut backoff = 1u64;

            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                if sampler.is_none() && Instant::now() >= retry_at {
                    match build_sampler(mock) {
                        Ok(s) => {
                            calc.reset_baseline();
                            calc.set_new_counters_start_at_zero(s.counters_start_at_zero());
                            sampler = Some(s);
                            last_good = Instant::now();
                        }
                        Err(e) => {
                            set_error(&mut error, e.to_string());
                            retry_at = Instant::now() + Duration::from_secs(backoff);
                            backoff = (backoff * 2).min(30);
                        }
                    }
                }

                if let Some(s) = sampler.as_mut() {
                    match s.sample() {
                        Ok(samples) => {
                            if s.take_reset() {
                                calc.reset_baseline();
                            }
                            last_good = Instant::now();
                            error = None;
                            backoff = 1;
                            calc.update(last_good, &samples);
                            let deltas = calc.take_deltas();
                            tracker.push_at(last_good, &deltas);
                            pending.add(SystemTime::now(), &deltas);
                        }
                        Err(SamplerError::Timeout) => {
                            if last_good.elapsed() >= STALE_AFTER {
                                set_error(&mut error, "采集暂时中断，正在重新连接".into());
                                calc.clear_rates();
                            }
                            if last_good.elapsed() >= RESTART_AFTER {
                                sampler = None;
                                retry_at = Instant::now() + Duration::from_secs(backoff);
                                backoff = (backoff * 2).min(30);
                            }
                        }
                        Err(e) => {
                            set_error(&mut error, e.to_string());
                            calc.clear_rates();
                            sampler = None;
                            retry_at = Instant::now() + Duration::from_secs(backoff);
                            backoff = (backoff * 2).min(30);
                        }
                    }
                } else if stop_rx.recv_timeout(Duration::from_millis(200)).is_ok() {
                    break;
                }
                tracker.push_at(Instant::now(), &[]);

                if last_flush.elapsed() >= FLUSH_EVERY {
                    if store.is_none() {
                        match open_store(mock) {
                            Ok(s) => {
                                store = Some(s);
                                store_error = None;
                            }
                            Err(e) => set_error(&mut store_error, format!("历史库暂不可用：{e}")),
                        }
                    }
                    if let Some(s) = store.as_mut() {
                        if !mock {
                            if let Err(e) = recover_pending(s) {
                                log_error(&format!("恢复未写入历史失败：{e}"));
                            }
                        }
                        match pending.flush(s) {
                            Ok(()) => store_error = None,
                            Err(e) => {
                                set_error(&mut store_error, format!("历史写入失败，将重试：{e}"))
                            }
                        }
                        if let Ok(rows) = s.totals_between(unix_now() - 86400, unix_now() + 60) {
                            day = rows.iter().fold((0u64, 0u64), |acc, r| {
                                (acc.0.saturating_add(r.rx), acc.1.saturating_add(r.tx))
                            });
                        }
                    }
                    last_flush = Instant::now();
                }
                if last_prune.elapsed() >= Duration::from_secs(3600) {
                    if let Some(s) = store.as_mut() {
                        let days = retention_days.load(Ordering::Relaxed).clamp(1, 365);
                        if let Err(e) = s.prune_before(unix_now() - days * 86400) {
                            log_error(&format!("清理历史失败：{e}"));
                        }
                    }
                    tracker.sweep_idle(MAX_SLOTS);
                    last_prune = Instant::now();
                }

                let (total_rx, total_tx) = calc.total_bps();
                let (session_rx, session_tx) = calc.session_total();
                let snapshot = Snapshot {
                    rates: calc.snapshot(),
                    windowed: WindowedRates {
                        min1: to_rates(tracker.rates(60)),
                        min5: to_rates(tracker.rates(300)),
                        hour1: to_rates(tracker.rates(3600)),
                        min10_bytes: tracker.bytes(600),
                    },
                    total_rx,
                    total_tx,
                    session_rx,
                    session_tx,
                    day_rx: day.0,
                    day_tx: day.1,
                    error: error.clone(),
                    mock,
                    store_error: store_error.clone(),
                };
                if tx.send(snapshot).is_err() {
                    break;
                }
                on_change();
            }
            // 正常退出先落盘，Drop 随后结束 nettop/管道。
            if store.is_none() {
                store = open_store(mock).ok();
            }
            if let Some(s) = store.as_mut() {
                if let Err(e) = pending.flush(s) {
                    log_error(&format!("退出时历史写入失败：{e}"));
                }
            }
            if !mock {
                if let Err(e) = pending.save_recovery() {
                    log_error(&format!("保留待恢复历史失败：{e}"));
                }
            }
        })
        .expect("无法启动采样线程");
    Engine {
        rx,
        stop,
        worker: Some(worker),
    }
}

fn set_error(slot: &mut Option<String>, message: String) {
    if slot.as_ref() != Some(&message) {
        log_error(&message);
    }
    *slot = Some(message);
}

fn log_error(message: &str) {
    crate::update::log_line(&format!("[sampler] {message}"));
}

fn to_rates(rows: Vec<(AppKey, f64, f64)>) -> Vec<AppRate> {
    rows.into_iter()
        .map(|(app, rx_bps, tx_bps)| AppRate {
            app,
            rx_bps,
            tx_bps,
            rx_session: 0,
            tx_session: 0,
        })
        .collect()
}

pub fn db_path() -> std::path::PathBuf {
    std::env::var_os("TM_DB_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(tm_store::default_path)
}

fn open_store(mock: bool) -> Result<Store, tm_store::StoreError> {
    if mock {
        Store::open_memory()
    } else {
        Store::open(&db_path())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn build_sampler(mock: bool) -> Result<Box<dyn Sampler>, SamplerError> {
    if mock {
        return Ok(Box::new(MockSampler::new()));
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(tm_macos::NettopSampler::spawn()?))
    }
    #[cfg(windows)]
    {
        Ok(Box::new(tm_windows::WindowsSampler::spawn()?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(SamplerError::Unsupported("网络采集"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_delta_survives_flush_and_keeps_original_minute() {
        let mut calc = RateCalculator::with_smoothing(1.0);
        let t = Instant::now();
        calc.update(t, &[tm_core::ProcSample::new(1, "app", 0, 0)]);
        calc.update(
            t + Duration::from_secs(1),
            &[tm_core::ProcSample::new(1, "app", 1234, 567)],
        );
        let mut pending = Pending::default();
        pending.add(UNIX_EPOCH + Duration::from_secs(119), &calc.take_deltas());
        pending.add(
            UNIX_EPOCH + Duration::from_secs(121),
            &[(AppKey::from("app"), 20, 0)],
        );
        let mut store = Store::open_memory().unwrap();
        pending.flush(&mut store).unwrap();
        assert!(pending.0.is_empty());
        assert_eq!(store.totals_between(60, 120).unwrap()[0].rx, 1234);
        assert_eq!(store.totals_between(120, 180).unwrap()[0].rx, 20);
        pending.flush(&mut store).unwrap();
        assert_eq!(store.totals_between(0, 180).unwrap()[0].rx, 1254);
    }

    #[test]
    fn mock_stores_are_isolated() {
        let mut a = open_store(true).unwrap();
        a.add_deltas(SystemTime::now(), &[(AppKey::from("demo"), 100, 0)])
            .unwrap();
        let b = open_store(true).unwrap();
        assert!(b.totals_between(0, i64::MAX).unwrap().is_empty());
    }
}
