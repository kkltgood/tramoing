//! Opt-in overlay lifecycle, independent hardware sampler and game Present FPS.
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use tm_core::color::Rgb;
use tm_windows::fps::Status as FpsStatus;
use tm_windows::game_overlay::{Content, GameOverlay, Segment};

use crate::i18n::Lang;
use crate::performance::{menu_reading, Monitor, Reading};
use crate::settings::GameOverlaySettings;

pub const FPS_COLOR: Rgb = Rgb(255, 210, 100);

pub struct Runtime {
    window: GameOverlay,
    hardware: Monitor,
    fps: FpsMonitor,
    pub error: Option<String>,
}

impl Runtime {
    pub fn new(ctx: egui::Context) -> Self {
        let wake = ctx.clone();
        Self {
            window: GameOverlay::new(),
            hardware: Monitor::new(move || wake.request_repaint()),
            fps: FpsMonitor::new(ctx),
            error: None,
        }
    }

    /// Returns true only when a drag or screen change requires saving position.
    pub fn update(
        &mut self,
        config: &mut GameOverlaySettings,
        network: Option<(f64, f64)>,
        lang: Lang,
    ) -> bool {
        if !config.enabled {
            self.shutdown();
            return false;
        }
        self.hardware.configure_nonblocking(
            config.enabled && (config.show_cpu || config.show_gpu),
            1,
            config.show_temperature,
        );
        self.fps.configure(config.enabled && config.show_fps);
        let content = config.enabled.then(|| Content {
            x: config.x,
            y: config.y,
            x_percent: config.x_percent,
            y_percent: config.y_percent,
            locked: config.locked,
            font_size: config.font_size,
            opacity: config.opacity,
            segments: segments(
                config,
                network,
                self.hardware.reading().as_ref(),
                self.fps.reading().as_ref(),
                lang,
            ),
        });
        match self.window.update(content) {
            Ok(position) => {
                self.error = None;
                if let Some(position) = position {
                    if (config.x, config.y, config.x_percent, config.y_percent)
                        != (
                            position.x,
                            position.y,
                            position.x_percent,
                            position.y_percent,
                        )
                    {
                        config.x = position.x;
                        config.y = position.y;
                        config.x_percent = position.x_percent;
                        config.y_percent = position.y_percent;
                        return true;
                    }
                }
            }
            Err(error) => {
                if self.error.as_deref() != Some(error) {
                    log::warn!(target: "tramoing::overlay", "{error}");
                }
                self.error = Some(error.into());
            }
        }
        false
    }

    pub fn fps_status(&self) -> (&'static str, String) {
        if let Some(value) = self.fps.reading().filter(|r| r.value().is_some()) {
            return ("overlay.fps_ready", format!("PID {}", value.pid));
        }
        let error = self.fps.last_error().unwrap_or_default();
        ("overlay.frame_rate_no_data", error)
    }

    pub fn shutdown(&mut self) {
        // Remove the window before asking the background samplers to stop.
        let _ = self.window.update(None);
        self.fps.configure(false);
        self.hardware.configure_nonblocking(false, 1, false);
        self.error = None;
    }
}

fn segments(
    config: &GameOverlaySettings,
    network: Option<(f64, f64)>,
    hardware: Option<&Reading>,
    fps: Option<&FpsReading>,
    lang: Lang,
) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut add = |text: String, color: Rgb| segments.push(Segment { text, color });
    if std::env::var_os("TM_FORCE_MOCK").is_some() {
        add(lang.t("overlay.demo").into(), Rgb(190, 190, 190));
    }
    if config.show_fps {
        add(frame_rate_text(fps), config.fps_color.unwrap_or(FPS_COLOR));
    }
    if config.show_cpu {
        add(
            format!(
                "CPU {}",
                menu_reading(
                    hardware.and_then(|r| r.cpu),
                    hardware.and_then(|r| r.temperatures.cpu),
                    config.show_temperature,
                )
            ),
            config.colors.cpu(true),
        );
    }
    if config.show_gpu {
        add(
            format!(
                "GPU {}",
                menu_reading(
                    hardware.and_then(|r| r.gpu),
                    hardware.and_then(|r| r.temperatures.gpu),
                    config.show_temperature,
                )
            ),
            config.colors.gpu(true),
        );
    }
    if config.show_network {
        let (down, up) = match network {
            Some((rx, tx)) => (tm_core::format_speed(rx), tm_core::format_speed(tx)),
            None => ("--".into(), "--".into()),
        };
        add(format!("↓ {down}"), config.colors.download(true));
        add(format!("↑ {up}"), config.colors.upload(true));
    }
    if segments.is_empty() {
        segments.push(Segment {
            text: "Tramoing".into(),
            color: Rgb(240, 240, 240),
        });
    }
    segments
}

#[derive(Clone)]
struct FpsReading {
    pid: u32,
    fps: Option<f64>,
    status: FpsStatus,
    error: Option<String>,
    at: Instant,
}

impl FpsReading {
    fn value(&self) -> Option<f64> {
        self.fps.filter(|v| {
            self.status == FpsStatus::Ready
                && self.at.elapsed() <= Duration::from_secs(3)
                && v.is_finite()
                && *v >= 0.0
        })
    }
}

fn frame_rate_text(reading: Option<&FpsReading>) -> String {
    if let Some(value) = reading.and_then(FpsReading::value) {
        return format!("FPS  {value:.0}");
    }
    "FPS  --".into()
}

enum Command {
    Enabled(bool, u64),
    Shutdown,
}

#[derive(Default)]
struct FpsSamples {
    generation: u64,
    reading: Option<FpsReading>,
}

impl FpsSamples {
    fn reset(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.reading = None;
        self.generation
    }

    fn publish(&mut self, generation: u64, reading: Option<FpsReading>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.reading = reading;
        true
    }
}

const TARGET_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const FPS_START_INTERVAL: Duration = Duration::from_millis(250);
const FPS_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
const MIN_REQUEST_GAP: Duration = Duration::from_millis(50);

/// Scan cheaply between requests. Requests use start-to-start deadlines so IPC
/// latency does not add another full polling interval to the first frame.
struct FpsPollSchedule {
    target: Option<u32>,
    next_poll: Instant,
    retry_not_before: Instant,
    failures: u8,
}

impl FpsPollSchedule {
    fn new(now: Instant) -> Self {
        Self {
            target: None,
            next_poll: now,
            retry_not_before: now,
            failures: 0,
        }
    }

    fn select(&mut self, target: Option<u32>, now: Instant) -> bool {
        if self.target == target {
            return false;
        }
        self.target = target;
        self.next_poll = now;
        true
    }

    fn due(&self, now: Instant) -> bool {
        self.target.is_some() && now >= self.next_poll && now >= self.retry_not_before
    }

    fn wait_duration(&self, now: Instant) -> Duration {
        if self.target.is_none() {
            return TARGET_CHECK_INTERVAL;
        }
        self.next_poll
            .max(self.retry_not_before)
            .saturating_duration_since(now)
            .min(TARGET_CHECK_INTERVAL)
    }

    fn completed(&mut self, started: Instant, finished: Instant, status: FpsStatus) {
        let interval = match status {
            FpsStatus::Failed => {
                self.failures = self.failures.saturating_add(1).min(2);
                // Stay below the helper's five-second lease, including IPC time.
                Duration::from_secs(u64::from(self.failures))
            }
            FpsStatus::Ready => {
                self.failures = 0;
                FPS_UPDATE_INTERVAL
            }
            FpsStatus::Starting | FpsStatus::NoFrames => {
                self.failures = 0;
                FPS_START_INTERVAL
            }
        };
        self.next_poll = (started + interval).max(finished + MIN_REQUEST_GAP);
        self.retry_not_before = if status == FpsStatus::Failed {
            self.next_poll
        } else {
            finished
        };
    }
}

struct FpsMonitor {
    enabled: bool,
    command: Option<mpsc::Sender<Command>>,
    latest: Arc<Mutex<FpsSamples>>,
    ctx: egui::Context,
    mock: bool,
}

impl FpsMonitor {
    fn new(ctx: egui::Context) -> Self {
        Self {
            enabled: false,
            command: None,
            latest: Arc::new(Mutex::new(FpsSamples::default())),
            ctx,
            mock: std::env::var_os("TM_FORCE_MOCK").is_some(),
        }
    }

    fn configure(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        let generation = self.latest.lock().unwrap().reset();
        if enabled && self.command.is_none() {
            let (command, commands) = mpsc::channel();
            let latest = self.latest.clone();
            let ctx = self.ctx.clone();
            let mock = self.mock;
            match thread::Builder::new()
                .name("game-fps".into())
                .spawn(move || {
                    let mut active = false;
                    let mut generation = 0;
                    let mut tracked: Option<(u32, tm_windows::fps::TrackedProcess)> = None;
                    let mut schedule = FpsPollSchedule::new(Instant::now());
                    loop {
                        let next = if active {
                            commands.recv_timeout(schedule.wait_duration(Instant::now()))
                        } else {
                            commands
                                .recv()
                                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                        };
                        match next {
                            Ok(Command::Enabled(value, requested_generation)) => {
                                active = value;
                                generation = requested_generation;
                                tracked = None;
                                schedule = FpsPollSchedule::new(Instant::now());
                                if !active {
                                    if !mock {
                                        let _ = tm_windows::fps::stop();
                                    }
                                    continue;
                                }
                            }
                            Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                                break
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        // Rapid toggles can queue another generation while IPC
                        // is in flight. Only the current enable may request data.
                        if latest.lock().unwrap().generation != generation {
                            active = false;
                            continue;
                        }
                        if tracked
                            .as_ref()
                            .is_some_and(|(_, process)| !process.is_running())
                        {
                            tracked = None;
                        }
                        let pid = if mock {
                            Some(1)
                        } else {
                            tracked
                                .as_ref()
                                .map(|(pid, _)| *pid)
                                .or_else(tm_windows::fps::foreground_pid)
                        };
                        if schedule.select(pid, Instant::now()) {
                            let mut latest = latest.lock().unwrap();
                            // A new target invalidates numeric samples, but keep
                            // the helper diagnostic visible during failure backoff.
                            if latest.generation == generation
                                && latest
                                    .reading
                                    .as_ref()
                                    .is_none_or(|r| r.status != FpsStatus::Failed)
                            {
                                latest.reading = None;
                            }
                            ctx.request_repaint();
                        }
                        let started = Instant::now();
                        if !schedule.due(started) {
                            continue;
                        }
                        let reading = pid.map(|pid| {
                            let result = if mock {
                                Ok(tm_windows::fps::Reading {
                                    pid,
                                    fps: Some(144.0),
                                    status: FpsStatus::Ready,
                                    error: None,
                                })
                            } else {
                                tm_windows::fps::read(pid)
                            };
                            match result {
                                Ok(reading) => FpsReading {
                                    pid,
                                    fps: reading.fps,
                                    status: reading.status,
                                    error: reading.error,
                                    at: Instant::now(),
                                },
                                Err(error) => FpsReading {
                                    pid,
                                    fps: None,
                                    status: FpsStatus::Failed,
                                    error: Some(error),
                                    at: Instant::now(),
                                },
                            }
                        });
                        // Never resurrect a sample from before disable/re-enable.
                        if latest.lock().unwrap().generation != generation {
                            active = false;
                            continue;
                        }
                        if let Some(value) = &reading {
                            schedule.completed(started, Instant::now(), value.status);
                        }
                        if !mock && tracked.is_none() {
                            if let Some(value) = reading.as_ref().filter(|r| r.value().is_some()) {
                                // Retain the actual process object across focus changes.
                                // No frames means display --, not a cached numeric FPS;
                                // keep sampling so the same game can resume in background.
                                tracked = tm_windows::fps::track_process(value.pid)
                                    .map(|process| (value.pid, process));
                            }
                        }
                        if latest.lock().unwrap().publish(generation, reading) {
                            ctx.request_repaint();
                        }
                    }
                    if !mock {
                        let _ = tm_windows::fps::stop();
                    }
                }) {
                Ok(_) => self.command = Some(command),
                Err(error) => {
                    self.latest.lock().unwrap().publish(
                        generation,
                        Some(FpsReading {
                            pid: 0,
                            fps: None,
                            status: FpsStatus::Failed,
                            error: Some(error.to_string()),
                            at: Instant::now(),
                        }),
                    );
                }
            }
        }
        if let Some(command) = &self.command {
            let _ = command.send(Command::Enabled(enabled, generation));
        }
    }

    fn reading(&self) -> Option<FpsReading> {
        if !self.enabled {
            return None;
        }
        self.latest
            .lock()
            .unwrap()
            .reading
            .clone()
            .filter(|r| r.at.elapsed() <= Duration::from_secs(3))
    }

    fn last_error(&self) -> Option<String> {
        self.enabled
            .then(|| {
                self.latest
                    .lock()
                    .unwrap()
                    .reading
                    .as_ref()
                    .and_then(|r| r.error.clone())
            })
            .flatten()
    }
}

impl Drop for FpsMonitor {
    fn drop(&mut self) {
        self.latest.lock().unwrap().reset();
        // IPC is bounded, but never hold the UI thread waiting on the helper.
        // The helper's short lease also releases ETW if this process exits early.
        if let Some(command) = &self.command {
            let _ = command.send(Command::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_in_flight_cannot_repopulate_a_disabled_or_reenabled_monitor() {
        let sample = || {
            Some(FpsReading {
                pid: 42,
                fps: Some(144.0),
                status: FpsStatus::Ready,
                error: None,
                at: Instant::now(),
            })
        };
        let mut samples = FpsSamples::default();
        let first_enable = samples.reset();
        assert!(samples.publish(first_enable, sample()));
        let disabled = samples.reset();
        assert!(!samples.publish(first_enable, sample()));
        assert!(samples.reading.is_none());
        let second_enable = samples.reset();
        assert!(!samples.publish(first_enable, sample()));
        assert!(samples.reading.is_none());
        assert!(samples.publish(second_enable, sample()));
        assert!(!samples.publish(disabled, None));
        assert_eq!(frame_rate_text(samples.reading.as_ref()), "FPS  144");
    }

    #[test]
    fn acquisition_reads_immediately_and_does_not_add_ipc_time_to_its_interval() {
        let now = Instant::now();
        let mut schedule = FpsPollSchedule::new(now);
        assert!(!schedule.due(now));
        assert_eq!(schedule.wait_duration(now), TARGET_CHECK_INTERVAL);
        assert!(schedule.select(Some(42), now));
        assert!(schedule.due(now));
        schedule.completed(now, now + Duration::from_millis(200), FpsStatus::Starting);
        assert_eq!(
            schedule.wait_duration(now + Duration::from_millis(200)),
            Duration::from_millis(50)
        );
        assert!(!schedule.due(now + Duration::from_millis(249)));
        assert!(schedule.due(now + Duration::from_millis(250)));
        assert!(schedule.select(Some(43), now + Duration::from_millis(210)));
        assert!(schedule.due(now + Duration::from_millis(210)));
        assert!(schedule.select(None, now + Duration::from_millis(220)));
        assert!(!schedule.due(now + Duration::from_secs(1)));
        assert_eq!(
            schedule.wait_duration(now + Duration::from_secs(1)),
            TARGET_CHECK_INTERVAL
        );
    }

    #[test]
    fn steady_reads_are_slower_and_a_slow_request_never_causes_catch_up_bursts() {
        let now = Instant::now();
        let mut schedule = FpsPollSchedule::new(now);
        schedule.select(Some(42), now);
        schedule.completed(now, now + Duration::from_millis(50), FpsStatus::Ready);
        assert!(!schedule.due(now + Duration::from_millis(499)));
        assert!(schedule.due(now + Duration::from_millis(500)));
        let next = now + Duration::from_millis(500);
        schedule.completed(next, next + Duration::from_millis(600), FpsStatus::NoFrames);
        assert!(!schedule.due(next + Duration::from_millis(600)));
        assert_eq!(
            schedule.wait_duration(next + Duration::from_millis(600)),
            MIN_REQUEST_GAP
        );
        assert!(schedule.due(next + Duration::from_millis(650)));
    }

    #[test]
    fn failed_requests_back_off_across_focus_changes_but_missing_frames_do_not() {
        let now = Instant::now();
        let mut schedule = FpsPollSchedule::new(now);
        schedule.select(Some(42), now);
        schedule.completed(now, now, FpsStatus::Failed);
        schedule.select(Some(43), now + Duration::from_millis(100));
        assert!(!schedule.due(now + Duration::from_millis(999)));
        assert!(schedule.due(now + Duration::from_secs(1)));
        for seconds in [1, 3, 5] {
            let start = now + Duration::from_secs(seconds);
            schedule.completed(start, start, FpsStatus::Failed);
            assert!(!schedule.due(start + Duration::from_millis(1999)));
            assert!(schedule.due(start + Duration::from_secs(2)));
        }
        let recovered = now + Duration::from_secs(7);
        schedule.completed(recovered, recovered, FpsStatus::NoFrames);
        assert!(schedule.due(recovered + FPS_START_INTERVAL));
        let next = recovered + FPS_START_INTERVAL;
        schedule.completed(next, next, FpsStatus::Failed);
        assert!(schedule.due(next + Duration::from_secs(1)));
    }

    #[test]
    fn missing_readings_are_not_reported_as_zero() {
        let config = GameOverlaySettings::default();
        let text = segments(&config, None, None, None, Lang::EN)
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("   ");
        assert!(text.contains("FPS  --"));
        assert!(text.contains("--%"));
        assert!(text.contains("--°C"));
        assert!(text.contains("↓ --   ↑ --"));
    }

    #[test]
    fn each_overlay_metric_and_temperature_can_be_hidden() {
        let mut config = GameOverlaySettings {
            show_temperature: false,
            show_network: false,
            show_fps: false,
            ..Default::default()
        };
        let text = segments(&config, Some((12.0, 34.0)), None, None, Lang::EN)
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("   ");
        assert!(text.contains("CPU"));
        assert!(text.contains("GPU"));
        assert!(!text.contains("°C"));
        assert!(!text.contains("FPS"));
        assert!(!text.contains("Hz"));
        assert!(!text.contains('↓'));
        config.show_cpu = false;
        config.show_gpu = false;
        assert!(segments(&config, None, None, None, Lang::EN)
            .iter()
            .all(|segment| !segment.text.contains('%')));
    }

    #[test]
    fn each_segment_uses_its_own_color_without_line_breaks() {
        let config = GameOverlaySettings {
            colors: tm_core::color::DisplayColors {
                cpu: Some(Rgb(1, 2, 3)),
                gpu: Some(Rgb(4, 5, 6)),
                download: Some(Rgb(7, 8, 9)),
                upload: Some(Rgb(10, 11, 12)),
            },
            fps_color: Some(Rgb(13, 14, 15)),
            ..Default::default()
        };
        let row = segments(&config, None, None, None, Lang::EN);
        for (prefix, color) in [
            ("FPS", Rgb(13, 14, 15)),
            ("CPU", Rgb(1, 2, 3)),
            ("GPU", Rgb(4, 5, 6)),
            ("↓", Rgb(7, 8, 9)),
            ("↑", Rgb(10, 11, 12)),
        ] {
            assert_eq!(
                row.iter()
                    .find(|s| s.text.starts_with(prefix))
                    .unwrap()
                    .color,
                color
            );
        }
        assert!(row.iter().all(|s| !s.text.contains(['\r', '\n'])));
    }

    #[test]
    fn disabled_fps_does_not_start_worker_or_expose_cached_reading() {
        let mut monitor = FpsMonitor::new(egui::Context::default());
        monitor.configure(false);
        assert!(monitor.command.is_none());
        monitor.latest.lock().unwrap().reading = Some(FpsReading {
            pid: 1,
            fps: Some(90.0),
            status: FpsStatus::Ready,
            error: None,
            at: Instant::now(),
        });
        assert!(monitor.reading().is_none());
    }

    #[test]
    fn diagnostics_survive_foreground_changes_but_clear_when_disabled() {
        let mut monitor = FpsMonitor::new(egui::Context::default());
        monitor.enabled = true;
        monitor.latest.lock().unwrap().reading = Some(FpsReading {
            pid: u32::MAX,
            fps: None,
            status: FpsStatus::Failed,
            error: Some("helper unavailable".into()),
            at: Instant::now() - Duration::from_secs(10),
        });
        assert!(monitor.reading().is_none());
        assert_eq!(monitor.last_error().as_deref(), Some("helper unavailable"));
        monitor.configure(false);
        assert!(monitor.last_error().is_none());
    }

    #[test]
    fn only_valid_frames_display_a_number_and_all_missing_states_use_dashes() {
        let mut reading = FpsReading {
            pid: 42,
            fps: Some(144.0),
            status: FpsStatus::Ready,
            error: None,
            at: Instant::now(),
        };
        assert_eq!(frame_rate_text(Some(&reading)), "FPS  144");
        for status in [FpsStatus::Starting, FpsStatus::NoFrames, FpsStatus::Failed] {
            reading.status = status;
            // A previous numeric sample must not leak through an unavailable status.
            assert_eq!(frame_rate_text(Some(&reading)), "FPS  --");
        }
        reading.status = FpsStatus::Ready;
        for value in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            reading.fps = value;
            assert_eq!(frame_rate_text(Some(&reading)), "FPS  --");
        }
        assert_eq!(frame_rate_text(None), "FPS  --");
    }

    #[test]
    fn tracked_frames_survive_focus_changes_but_stale_frames_show_dashes() {
        let mut monitor = FpsMonitor::new(egui::Context::default());
        monitor.enabled = true;
        monitor.mock = true; // The tracked PID may differ from the foreground PID.
        monitor.latest.lock().unwrap().reading = Some(FpsReading {
            pid: 2,
            fps: Some(144.0),
            status: FpsStatus::Ready,
            error: None,
            at: Instant::now(),
        });
        assert_eq!(frame_rate_text(monitor.reading().as_ref()), "FPS  144");
        {
            let mut latest = monitor.latest.lock().unwrap();
            let reading = latest.reading.as_mut().unwrap();
            reading.at = Instant::now() - Duration::from_secs(4);
        }
        assert_eq!(frame_rate_text(monitor.reading().as_ref()), "FPS  --");
        monitor.latest.lock().unwrap().reading.as_mut().unwrap().at = Instant::now();
        assert_eq!(frame_rate_text(monitor.reading().as_ref()), "FPS  144");
    }
}
