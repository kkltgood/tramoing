//! 可启停的独立性能采样，不被 nettop 超时或历史库 I/O 阻塞。

use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Reading {
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
    at: Instant,
}

enum Command {
    Interval(Duration),
    Stop,
}

struct Worker {
    command: mpsc::Sender<Command>,
    latest: Arc<Mutex<Option<Reading>>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(
        interval: Duration,
        mut sample: impl FnMut() -> (Option<f64>, Option<f64>) + Send + 'static,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Self> {
        let (command, commands) = mpsc::channel();
        let latest = Arc::new(Mutex::new(None));
        let output = latest.clone();
        let thread = thread::Builder::new()
            .name("performance".into())
            .spawn(move || {
                let mut interval = interval;
                let publish = |sample: &mut dyn FnMut() -> (Option<f64>, Option<f64>)| {
                    let (cpu, gpu) = sample();
                    *output.lock().unwrap() = Some(Reading {
                        cpu,
                        gpu,
                        at: Instant::now(),
                    });
                    wake();
                };
                publish(&mut sample);
                let mut next = Instant::now() + interval;
                loop {
                    match commands.recv_timeout(next.saturating_duration_since(Instant::now())) {
                        Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Ok(Command::Interval(new)) => {
                            interval = new;
                            next = Instant::now() + interval;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            publish(&mut sample);
                            next = Instant::now() + interval;
                        }
                    }
                }
            })?;
        Ok(Self {
            command,
            latest,
            thread: Some(thread),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.command.send(Command::Stop);
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

pub struct Monitor {
    worker: Option<Worker>,
    interval: Duration,
    wake: Arc<dyn Fn() + Send + Sync>,
    pub error: Option<String>,
}

impl Monitor {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            worker: None,
            interval: Duration::from_secs(2),
            wake: Arc::new(wake),
            error: None,
        }
    }

    pub fn configure(&mut self, enabled: bool, seconds: u64) {
        if !enabled {
            self.worker = None;
            self.error = None;
            return;
        }
        let interval = Duration::from_secs(match seconds {
            1 | 2 | 5 => seconds,
            _ => 2,
        });
        if let Some(worker) = &self.worker {
            if self.interval != interval {
                let _ = worker.command.send(Command::Interval(interval));
                self.interval = interval;
            }
            return;
        }
        self.interval = interval;
        let mut cpu = tm_macos::utilization::CpuSampler::default();
        let mock = std::env::var_os("TM_FORCE_MOCK").is_some();
        let started = Instant::now();
        match Worker::spawn(
            interval,
            move || {
                if mock {
                    let t = started.elapsed().as_secs_f64();
                    (
                        Some(28.0 + 9.0 * (t * 0.4).sin()),
                        Some(14.0 + 6.0 * (t * 0.3).cos()),
                    )
                } else {
                    (cpu.sample(), tm_macos::utilization::gpu_percent())
                }
            },
            self.wake.clone(),
        ) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.error = None;
            }
            Err(error) => {
                crate::update::log_line(&format!("[performance] {error}"));
                self.error = Some(error.to_string());
            }
        }
    }

    pub fn reading(&self) -> Option<Reading> {
        let worker = self.worker.as_ref()?;
        if worker
            .thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
        {
            return None;
        }
        worker
            .latest
            .lock()
            .unwrap()
            .clone()
            .filter(|reading| reading.at.elapsed() <= self.interval * 3 + Duration::from_secs(2))
    }
}

/// 和菜单栏网速一样使用固定宽度；不可用是 --%，不误报成空闲的 0%。
pub fn menu_percent(value: Option<f64>) -> String {
    match value.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)) {
        Some(value) => format!("{value:>3.0}%"),
        None => " --%".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_percent_has_constant_width_and_distinguishes_unavailable() {
        for value in [
            Some(0.0),
            Some(9.5),
            Some(99.6),
            Some(100.0),
            None,
            Some(f64::NAN),
            Some(-1.0),
        ] {
            assert_eq!(menu_percent(value).len(), 4);
        }
        assert_eq!(menu_percent(None), " --%");
        assert_eq!(menu_percent(Some(0.0)), "  0%");
        assert_eq!(menu_percent(Some(99.6)), "100%");
    }

    #[test]
    fn worker_changes_interval_stops_promptly_and_keeps_only_latest_reading() {
        let (changed, changes) = mpsc::channel();
        let mut count = 0.0;
        let worker = Worker::spawn(
            Duration::from_secs(5),
            move || {
                count += 1.0;
                (Some(count), None)
            },
            Arc::new(move || {
                let _ = changed.send(());
            }),
        )
        .unwrap();
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        worker
            .command
            .send(Command::Interval(Duration::from_millis(10)))
            .unwrap();
        for _ in 0..3 {
            changes.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(worker.latest.lock().unwrap().as_ref().unwrap().cpu.unwrap() >= 4.0);
        worker
            .command
            .send(Command::Interval(Duration::from_secs(5)))
            .unwrap();
        let before = Instant::now();
        drop(worker);
        assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn disable_removes_worker_and_stale_readings_are_hidden() {
        let (changed, changes) = mpsc::channel();
        let mut monitor = Monitor::new(|| {});
        monitor.configure(false, 2);
        assert!(monitor.worker.is_none());
        monitor.worker = Some(
            Worker::spawn(
                Duration::from_secs(5),
                || (Some(12.0), Some(34.0)),
                Arc::new(move || {
                    let _ = changed.send(());
                }),
            )
            .unwrap(),
        );
        changes.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(monitor.reading().unwrap().cpu, Some(12.0));
        monitor
            .worker
            .as_ref()
            .unwrap()
            .latest
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .at = Instant::now() - Duration::from_secs(30);
        assert!(monitor.reading().is_none());
        monitor.configure(false, 2);
        assert!(monitor.worker.is_none());
        assert!(monitor.reading().is_none());
    }
}
