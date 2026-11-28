//! On-demand application Present FPS, isolated from the network/temperature workers.
//!
//! DXGI and D3D9 event identities/property names follow the Windows manifests,
//! also documented by Intel PresentMon's PresentData/ETW headers and consumer:
//! https://github.com/GameTechDev/PresentMon/tree/main/PresentData
//! This deliberately does not claim display FPS, Vulkan/OpenGL support, or count
//! frame-generation output. No hooks or code injection are used.
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::fps_counter::Counter;
use crate::fps_protocol::Request;
pub use crate::fps_protocol::{Reading, Status};
use crate::network_control::{self, Server};

const LEASE: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(1);
const WARMUP_FLUSH_INTERVAL: Duration = Duration::from_millis(150);
const STEADY_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const FLUSH_WARMUP: Duration = Duration::from_secs(3);
const DXGI: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
const D3D9: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x783aca0a_790e_4d7f_8451_aa850511c6b9);

fn pipe_name() -> Result<String, String> {
    Ok(format!(
        r"\\.\pipe\Tramoing-FPS-{}-v1",
        crate::scheduled_task::current_user_sid()?
    ))
}

/// Call from a worker. A request renews a five-second lease on the ETW session.
pub fn read(pid: u32) -> Result<Reading, String> {
    network_control::request_named(&pipe_name()?, &Request::Read { pid }, TIMEOUT)
        .map_err(|error| error.replace("网络控制", "FPS 采集"))
}

/// Stop promptly when disabled; lease expiry also handles a disconnected UI.
pub fn stop() -> Result<(), String> {
    let _: Reading = network_control::request_named(&pipe_name()?, &Request::Stop {}, TIMEOUT)
        .map_err(|error| error.replace("网络控制", "FPS 采集"))?;
    Ok(())
}

pub fn foreground_pid() -> Option<u32> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
    }
    (pid != 0 && pid != std::process::id()).then_some(pid)
}

/// A specific process object, retained while the FPS worker follows its frames.
/// Keeping its handle prevents a reused PID from being mistaken for that process.
#[derive(Debug)]
pub struct TrackedProcess(OwnedHandle);

/// Opens only synchronization access, without reading memory or altering the
/// target. An inaccessible or already-exited process cannot become a pinned target.
pub fn track_process(pid: u32) -> Option<TrackedProcess> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let process = TrackedProcess(unsafe { OwnedHandle::from_raw_handle(handle) });
    process.is_running().then_some(process)
}

impl TrackedProcess {
    /// Nonblocking: a process handle is signaled after exit. Unlike checking
    /// STILL_ACTIVE, this also handles a process whose real exit code is 259.
    pub fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT }
    }
}

fn qpc() -> i64 {
    let mut now = 0;
    unsafe {
        QueryPerformanceCounter(&mut now);
    }
    now
}

fn frequency() -> i64 {
    let mut frequency = 0;
    unsafe {
        QueryPerformanceFrequency(&mut frequency);
    }
    frequency
}

#[repr(C)]
struct Properties {
    header: EVENT_TRACE_PROPERTIES,
    logger: [u16; 1025],
    file: [u16; 1025],
}

impl Properties {
    fn new() -> Self {
        let mut properties: Self = unsafe { std::mem::zeroed() };
        properties.header.Wnode.BufferSize = size_of::<Self>() as u32;
        properties.header.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        properties.header.Wnode.ClientContext = 1; // QPC; preserved by RAW_TIMESTAMP.
        properties.header.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        properties.header.BufferSize = 64;
        properties.header.MinimumBuffers = 4;
        properties.header.MaximumBuffers = 32;
        properties.header.FlushTimer = 1;
        properties.header.LoggerNameOffset = std::mem::offset_of!(Self, logger) as u32;
        properties
    }

    fn pointer(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        (self as *mut Self).cast()
    }
}

fn stop_named(name: &[u16]) {
    let mut properties = Properties::new();
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            properties.pointer(),
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

struct Sink {
    counter: Mutex<Counter>,
    frequency: i64,
    alive: AtomicBool,
    consumer: Mutex<ConsumerState>,
    events: AtomicU64,
    target_events: AtomicU64,
    decode_failures: AtomicU64,
}

impl Sink {
    fn diagnostics(&self) -> String {
        format!(
            "events={} target_events={} decode_failures={}",
            self.events.load(Ordering::Relaxed),
            self.target_events.load(Ordering::Relaxed),
            self.decode_failures.load(Ordering::Relaxed)
        )
    }

    fn reset_diagnostics(&self) {
        self.events.store(0, Ordering::Relaxed);
        self.target_events.store(0, Ordering::Relaxed);
        self.decode_failures.store(0, Ordering::Relaxed);
    }
}

/// Serializes publishing a late OpenTrace result with startup cancellation.
/// Closing an as-yet-unpublished atomic handle is insufficient: OpenTrace may
/// return after the three-second startup deadline and enter ProcessTrace.
#[derive(Default)]
struct ConsumerState {
    cancelled: bool,
    trace: Option<u64>,
}

impl ConsumerState {
    fn publish(&mut self, trace: u64) -> bool {
        if self.cancelled {
            return false;
        }
        self.trace = Some(trace);
        true
    }

    fn cancel(&mut self) -> Option<u64> {
        self.cancelled = true;
        self.trace.take()
    }
}

struct Session {
    handle: CONTROLTRACE_HANDLE,
    sink: Arc<Sink>,
    consumer: Option<JoinHandle<()>>,
}

/// ETW's FlushTimer uses whole seconds and may deliver per-processor buffers
/// late. Explicit flushes keep latency bounded while this opt-in session is
/// leased; a short burst helps new targets, then at most two calls per second.
struct FlushSchedule {
    last_flush: Instant,
    warmup_until: Instant,
    was_ready: bool,
    failed: bool,
}

impl FlushSchedule {
    fn new(now: Instant) -> Self {
        Self {
            last_flush: now,
            warmup_until: now + FLUSH_WARMUP,
            was_ready: false,
            failed: false,
        }
    }

    fn due(&mut self, now: Instant, ready: bool) -> bool {
        if self.failed {
            return false;
        }
        if self.was_ready && !ready {
            self.warmup_until = now + FLUSH_WARMUP;
        }
        self.was_ready = ready;
        let interval = if !ready && now < self.warmup_until {
            WARMUP_FLUSH_INTERVAL
        } else {
            STEADY_FLUSH_INTERVAL
        };
        if now.saturating_duration_since(self.last_flush) < interval {
            return false;
        }
        // Do not catch up missed ticks after suspension or a slow Win32 call.
        self.last_flush = now;
        true
    }

    fn run_due(
        &mut self,
        sink: &Sink,
        now: Instant,
        source_now: i64,
        flush: impl FnOnce() -> u32,
    ) -> Option<u32> {
        let ready = {
            let counter = sink.counter.lock().unwrap();
            if counter.pid == 0 {
                return None;
            }
            counter.fps(source_now, sink.frequency).is_some()
        };
        if !self.due(now, ready) {
            return None;
        }
        // ControlTrace may wait for the consumer to finish a buffer. The
        // callback must be able to acquire counter while this call is running.
        let code = flush();
        self.failed = code != ERROR_SUCCESS;
        Some(code)
    }
}

impl Session {
    fn flush_due(&self, schedule: &mut FlushSchedule, now: Instant) -> Option<u32> {
        schedule.run_due(&self.sink, now, qpc(), || {
            // ControlTrace mutates the properties allocation. Never reuse the
            // output from StartTrace or an earlier flush as its next input.
            let mut properties = Properties::new();
            unsafe {
                ControlTraceW(
                    self.handle,
                    null(),
                    properties.pointer(),
                    EVENT_TRACE_CONTROL_FLUSH,
                )
            }
        })
    }

    fn start(name: Vec<u16>, sink: Arc<Sink>) -> Result<Self, String> {
        // Every retry is a new stream. Never pair a previous session's start
        // event with a stop event from its replacement, even for the same PID.
        *sink.consumer.lock().unwrap() = ConsumerState::default();
        sink.alive.store(false, Ordering::Release);
        sink.counter.lock().unwrap().restart(qpc());
        sink.reset_diagnostics();
        let mut properties = Properties::new();
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        let code = unsafe { StartTraceW(&mut handle, name.as_ptr(), properties.pointer()) };
        if code != ERROR_SUCCESS {
            return Err(format!("Cannot start FPS ETW session (Windows {code})"));
        }
        let mut session = Self {
            handle,
            sink: sink.clone(),
            consumer: None,
        };
        for provider in [&DXGI, &D3D9] {
            let code = unsafe {
                EnableTraceEx2(
                    handle,
                    provider,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                    5,
                    0x8000_0000_0000_0002,
                    0,
                    0,
                    null(),
                )
            };
            if code != ERROR_SUCCESS {
                return Err(format!("Cannot enable FPS provider (Windows {code})"));
            }
        }
        let (ready, receiver) = mpsc::channel();
        session.consumer = Some(
            std::thread::Builder::new()
                .name("fps-etw".into())
                .spawn(move || {
                    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
                    logfile.LoggerName = name.as_ptr() as *mut u16;
                    logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
                        | PROCESS_TRACE_MODE_EVENT_RECORD
                        | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
                    logfile.Anonymous2.EventRecordCallback = Some(event);
                    logfile.Context = Arc::as_ptr(&sink) as *mut c_void;
                    let trace = unsafe { OpenTraceW(&mut logfile) };
                    if trace.Value == u64::MAX {
                        let _ = ready.send(false);
                        return;
                    }
                    if !sink.consumer.lock().unwrap().publish(trace.Value) {
                        unsafe {
                            CloseTrace(trace);
                        }
                        return;
                    }
                    sink.alive.store(true, Ordering::Release);
                    let _ = ready.send(true);
                    let code = unsafe { ProcessTrace(&trace, 1, null(), null()) };
                    if code != ERROR_SUCCESS && code != ERROR_CANCELLED {
                        log::warn!(target: "tramoing::fps", "FPS ProcessTrace stopped with Windows error {code}");
                    }
                    sink.alive.store(false, Ordering::Release);
                    close_consumer(&sink);
                })
                .map_err(|error| error.to_string())?,
        );
        match receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(true) => Ok(session),
            _ => Err("Cannot open FPS ETW consumer".into()),
        }
    }
}

fn close_consumer(sink: &Sink) {
    let trace = sink.consumer.lock().unwrap().cancel();
    if let Some(value) = trace {
        unsafe {
            CloseTrace(PROCESSTRACE_HANDLE { Value: value });
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Set cancellation before stopping the logger; a slow OpenTrace can
        // otherwise publish its handle after the one shutdown close attempt.
        close_consumer(&self.sink);
        let mut properties = Properties::new();
        unsafe {
            ControlTraceW(
                self.handle,
                null(),
                properties.pointer(),
                EVENT_TRACE_CONTROL_STOP,
            );
        }
        // Closing also wakes ProcessTrace if stopping the controller fails.
        if let Some(consumer) = self.consumer.take() {
            let _ = consumer.join();
        }
    }
}

fn same_guid(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe fn property(record: &EVENT_RECORD, name: *const u16) -> Option<u64> {
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: name as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };
    let mut value = [0u8; 8];
    let result = TdhGetProperty(
        record,
        0,
        null(),
        1,
        &descriptor,
        value.len() as u32,
        value.as_mut_ptr(),
    );
    (result == ERROR_SUCCESS).then(|| u64::from_le_bytes(value))
}

unsafe extern "system" fn event(record: *mut EVENT_RECORD) {
    let Some(record) = record.as_ref() else {
        return;
    };
    let Some(sink) = (record.UserContext as *const Sink).as_ref() else {
        return;
    };
    sink.events.fetch_add(1, Ordering::Relaxed);
    let Ok(mut counter) = sink.counter.lock() else {
        return;
    };
    let header = &record.EventHeader;
    if counter.pid == 0 || counter.pid != header.ProcessId {
        return;
    }
    sink.target_events.fetch_add(1, Ordering::Relaxed);
    let (runtime, start, stop, chain_name) = if same_guid(&header.ProviderId, &DXGI) {
        (
            1,
            matches!(header.EventDescriptor.Id, 42 | 55),
            matches!(header.EventDescriptor.Id, 43 | 56),
            windows_sys::w!("pIDXGISwapChain"),
        )
    } else if same_guid(&header.ProviderId, &D3D9) {
        (
            2,
            header.EventDescriptor.Id == 1,
            header.EventDescriptor.Id == 2,
            windows_sys::w!("pSwapchain"),
        )
    } else {
        return;
    };
    if start {
        // A malformed/new-schema start must not leave a previous call paired
        // with this thread's next stop event.
        counter.start(runtime, header.ThreadId, 0, header.TimeStamp, true);
        if let (Some(chain), Some(flags)) = (
            property(record, chain_name),
            property(record, windows_sys::w!("Flags")),
        ) {
            counter.start(
                runtime,
                header.ThreadId,
                chain,
                header.TimeStamp,
                runtime == 1 && flags & 1 != 0,
            );
        } else {
            sink.decode_failures.fetch_add(1, Ordering::Relaxed);
        }
    } else if stop {
        let result = property(record, windows_sys::w!("Result"));
        if result.is_none() {
            sink.decode_failures.fetch_add(1, Ordering::Relaxed);
        }
        counter.stop(runtime, header.ThreadId, result == Some(0), sink.frequency);
    }
}

struct Supervisor {
    session: Option<Session>,
    sink: Arc<Sink>,
    requested: Option<Instant>,
    retry: Instant,
    error: Option<String>,
    diagnostic_at: Instant,
    reported_ready: bool,
    flush_schedule: FlushSchedule,
}

impl Supervisor {
    fn new() -> Self {
        Self {
            session: None,
            sink: Arc::new(Sink {
                counter: Mutex::new(Counter::default()),
                frequency: frequency(),
                alive: AtomicBool::new(false),
                consumer: Mutex::new(ConsumerState::default()),
                events: AtomicU64::new(0),
                target_events: AtomicU64::new(0),
                decode_failures: AtomicU64::new(0),
            }),
            requested: None,
            retry: Instant::now(),
            error: None,
            diagnostic_at: Instant::now(),
            reported_ready: false,
            flush_schedule: FlushSchedule::new(Instant::now()),
        }
    }

    fn tick(&mut self, name: &[u16]) {
        if self.requested.is_none_or(|at| at.elapsed() >= LEASE) {
            self.session.take();
            self.sink.counter.lock().unwrap().select(0, qpc());
            self.error = None;
            self.retry = Instant::now();
            return;
        }
        if self.session.is_some() && !self.sink.alive.load(Ordering::Acquire) {
            self.session.take();
            self.error = Some("FPS ETW consumer stopped".into());
            self.retry = Instant::now() + Duration::from_secs(10);
        }
        if self.session.is_none() && Instant::now() >= self.retry {
            self.retry = Instant::now() + Duration::from_secs(10);
            match Session::start(name.to_vec(), self.sink.clone()) {
                Ok(session) => {
                    log::info!(target: "tramoing::fps", "FPS ETW session started; QPC frequency={}", self.sink.frequency);
                    self.session = Some(session);
                    self.error = None;
                    self.flush_schedule = FlushSchedule::new(Instant::now());
                }
                Err(error) => {
                    log::warn!(target: "tramoing::fps", "{error}");
                    self.error = Some(error);
                }
            }
        }
        // Only this supervisor thread owns/controls/stops the session. No timer
        // thread can race Drop or keep flushing after the request lease expires.
        if let Some(session) = &self.session {
            if let Some(code) = session.flush_due(&mut self.flush_schedule, Instant::now()) {
                if code != ERROR_SUCCESS {
                    // Preserve ETW's automatic timer if an explicit flush fails;
                    // disable further manual attempts until target/session reset.
                    log::warn!(target: "tramoing::fps", "FPS low-latency flush failed (Windows {code}); using ETW timer");
                }
            }
        }
    }

    fn reading(&mut self, pid: u32) -> Reading {
        self.requested = (pid != 0).then(Instant::now);
        let mut counter = self.sink.counter.lock().unwrap();
        if counter.pid != pid {
            self.sink.reset_diagnostics();
            self.diagnostic_at = Instant::now();
            self.reported_ready = false;
            self.flush_schedule = FlushSchedule::new(Instant::now());
        }
        counter.select(pid, qpc());
        let mut reading = Reading::empty(pid, Status::Starting);
        if let Some(error) = &self.error {
            reading.status = Status::Failed;
            reading.error = Some(error.clone());
        } else if self.session.is_some() {
            reading.fps = counter.fps(qpc(), self.sink.frequency);
            reading.status = if reading.fps.is_some() {
                Status::Ready
            } else {
                Status::NoFrames
            };
            if reading.fps.is_some() && !self.reported_ready {
                log::info!(target: "tramoing::fps", "FPS available for target PID {pid}: {:?}", reading.fps);
                self.reported_ready = true;
            } else if reading.fps.is_none()
                && self.diagnostic_at.elapsed() >= Duration::from_secs(30)
            {
                log::info!(target: "tramoing::fps", "FPS unavailable for target PID {pid}: {} {}", self.sink.diagnostics(), counter.diagnostics(qpc(), self.sink.frequency));
                self.diagnostic_at = Instant::now();
            }
        }
        reading
    }
}

/// Helper-side idle pipe. Graphics providers are enabled only while requested.
pub fn run() -> ! {
    let mut warning_at: Option<Instant> = None;
    loop {
        let result = (|| -> Result<(), String> {
            let sid = crate::scheduled_task::current_user_sid()?;
            let _owner = crate::instance::InstanceGuard::acquire(&format!(
                r"Global\Tramoing-FPS-{sid}-Owner"
            ))
            .map_err(|code| format!("FPS owner mutex failed ({code})"))?
            .ok_or("FPS helper already running")?;
            let name: Vec<u16> = format!("Tramoing-FPS-{sid}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            // A force-killed helper can leave a logger behind. We own its mutex.
            stop_named(&name);
            let server = Server::create_named(&pipe_name()?)?;
            let handle = server.raw_handle();
            let mut supervisor = Supervisor::new();
            loop {
                supervisor.tick(&name);
                let ok = unsafe { ConnectNamedPipe(handle, null_mut()) };
                let code = unsafe { GetLastError() };
                if ok != 0 || code != ERROR_PIPE_CONNECTED {
                    if ok == 0 && code == ERROR_NO_DATA {
                        unsafe {
                            DisconnectNamedPipe(handle);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                let result = (|| -> Result<(), String> {
                    let deadline = Instant::now() + TIMEOUT;
                    network_control::verify_peer(handle, false)?;
                    let reading = match network_control::receive(handle, deadline)? {
                        Request::Read { pid } => supervisor.reading(pid),
                        Request::Stop {} => supervisor.reading(0),
                    };
                    network_control::send(handle, &reading, deadline)?;
                    network_control::read(handle, &mut [0; 1], deadline)
                })();
                if let Err(error) = result {
                    if warning_at.is_none_or(|at| at.elapsed() > Duration::from_secs(30)) {
                        log::warn!(target: "tramoing::fps", "FPS request failed: {error}");
                        warning_at = Some(Instant::now());
                    }
                }
                unsafe {
                    DisconnectNamedPipe(handle);
                }
            }
        })();
        if let Err(error) = result {
            log::warn!(target: "tramoing::fps", "{error}");
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_flush_is_bounded_through_warmup_ready_and_no_frames() {
        let start = Instant::now();
        let mut schedule = FlushSchedule::new(start);
        assert!(!schedule.due(start, false));
        assert!(!schedule.due(start + Duration::from_millis(149), false));
        assert!(schedule.due(start + Duration::from_millis(150), false));
        assert!(!schedule.due(start + Duration::from_millis(150), false));
        // Once valid frames are available, stop fast startup flushing.
        assert!(!schedule.due(start + Duration::from_millis(300), true));
        assert!(!schedule.due(start + Duration::from_millis(649), true));
        assert!(schedule.due(start + Duration::from_millis(650), true));
        // Losing frames grants one bounded recovery burst.
        assert!(!schedule.due(start + Duration::from_millis(700), false));
        assert!(schedule.due(start + Duration::from_millis(800), false));
        assert!(schedule.due(start + Duration::from_millis(950), false));
        assert!(schedule.due(start + Duration::from_millis(3650), false));
        assert!(!schedule.due(start + Duration::from_millis(3800), false));
        assert!(schedule.due(start + Duration::from_millis(4150), false));
        // A late timer performs one flush, never replays missed ticks.
        assert!(schedule.due(start + Duration::from_secs(60), false));
        assert!(!schedule.due(start + Duration::from_secs(60), false));
        assert!(!schedule.due(start + Duration::from_millis(60499), false));
        assert!(schedule.due(start + Duration::from_millis(60500), false));
    }

    #[test]
    fn continuously_missing_frames_do_not_extend_fast_flush_budget() {
        let start = Instant::now();
        let mut schedule = FlushSchedule::new(start);
        let mut calls = Vec::new();
        for millis in 0..=6000 {
            if schedule.due(start + Duration::from_millis(millis), false) {
                calls.push(millis);
            }
        }
        assert_eq!(calls.iter().filter(|ms| **ms < 3000).count(), 19);
        assert!(calls.windows(2).all(|pair| pair[1] - pair[0] >= 150));
        assert!(calls
            .windows(2)
            .filter(|pair| pair[1] >= 3000)
            .all(|pair| pair[1] - pair[0] >= 500));
    }

    #[test]
    fn flush_allows_concurrent_event_ingestion_and_skips_inactive_targets() {
        let supervisor = Supervisor::new();
        let sink = &supervisor.sink;
        let start = Instant::now();
        let mut schedule = FlushSchedule::new(start);
        let due = start + WARMUP_FLUSH_INTERVAL;
        assert_eq!(
            schedule.run_due(sink, due, 0, || panic!("idle flush")),
            None
        );
        sink.counter.lock().unwrap().select(42, 0);
        let frequency = sink.frequency;
        assert_eq!(
            schedule.run_due(sink, due, frequency, || {
                // Mirrors ProcessTrace's consumer acquiring the counter while the
                // controlling helper thread is still inside ControlTrace(FLUSH).
                std::thread::scope(|threads| {
                    threads
                        .spawn(|| {
                            let mut counter = sink
                                .counter
                                .try_lock()
                                .expect("flush must release the event counter");
                            for frame in 0..=6 {
                                counter.start(1, 7, 1, frame * frequency / 60, false);
                                counter.stop(1, 7, true, frequency);
                            }
                        })
                        .join()
                        .unwrap();
                });
                ERROR_SUCCESS
            }),
            Some(ERROR_SUCCESS)
        );
        assert!(sink
            .counter
            .lock()
            .unwrap()
            .fps(frequency, frequency)
            .is_some());
        sink.counter.lock().unwrap().select(0, frequency);
        assert_eq!(
            schedule.run_due(sink, due + STEADY_FLUSH_INTERVAL, frequency, || panic!(
                "stopped target flush"
            )),
            None
        );
    }

    #[test]
    fn flush_failure_stops_repeated_win32_calls_until_target_reset() {
        let supervisor = Supervisor::new();
        supervisor.sink.counter.lock().unwrap().select(42, 0);
        let start = Instant::now();
        let mut schedule = FlushSchedule::new(start);
        assert_eq!(
            schedule.run_due(&supervisor.sink, start + WARMUP_FLUSH_INTERVAL, 0, || {
                ERROR_INVALID_HANDLE
            }),
            Some(ERROR_INVALID_HANDLE)
        );
        assert_eq!(
            schedule.run_due(
                &supervisor.sink,
                start + Duration::from_secs(60),
                0,
                || panic!("repeated failed flush")
            ),
            None
        );
        let next = start + Duration::from_secs(60);
        schedule = FlushSchedule::new(next);
        assert_eq!(
            schedule.run_due(&supervisor.sink, next + WARMUP_FLUSH_INTERVAL, 0, || {
                ERROR_SUCCESS
            }),
            Some(ERROR_SUCCESS)
        );
    }

    #[test]
    fn tracked_process_remains_live_until_its_exact_process_exits() {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let current = track_process(std::process::id()).expect("open current process");
        assert!(current.is_running());
        assert!(track_process(0).is_none());

        // The child waits for a newline, making both sides of its lifecycle
        // deterministic. 259 deliberately overlaps Win32's STILL_ACTIVE value.
        let mut child = Command::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/cmd.exe"),
        )
        .args(["/d", "/c", "set /p TramoingFpsTest= & exit /b 259"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .unwrap();
        let tracked = track_process(child.id()).expect("open live child process");
        assert!(tracked.is_running());
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        assert!(!tracked.is_running());
        assert!(!tracked.is_running());
        assert!(current.is_running());
    }

    /// Run from an elevated terminal with TM_FPS_TEST_PID set to a running
    /// Direct3D game's PID. Uses a unique temporary ETW session, never the
    /// installed helper's session or control pipe.
    #[test]
    #[ignore = "requires elevation and a live Direct3D process in TM_FPS_TEST_PID"]
    fn native_live_present_events_produce_fps() {
        let pid: u32 = std::env::var("TM_FPS_TEST_PID")
            .expect("set TM_FPS_TEST_PID to a running Direct3D process")
            .parse()
            .unwrap();
        let supervisor = Supervisor::new();
        supervisor.sink.counter.lock().unwrap().select(pid, qpc());
        let name: Vec<u16> = format!("Tramoing-FPS-Test-{}-{}", std::process::id(), qpc())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let _session = Session::start(name, supervisor.sink.clone()).unwrap();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            let counter = supervisor.sink.counter.lock().unwrap();
            let fps = counter.fps(qpc(), supervisor.sink.frequency);
            eprintln!(
                "pid={pid} fps={fps:?} {} {}",
                supervisor.sink.diagnostics(),
                counter.diagnostics(qpc(), supervisor.sink.frequency)
            );
            if let Some(fps) = fps {
                assert!(fps.is_finite() && fps > 0.0);
                return;
            }
        }
        panic!("No FPS received: {}", supervisor.sink.diagnostics());
    }

    #[test]
    fn startup_cancellation_closes_each_handle_once_in_both_race_orders() {
        let mut late_open = ConsumerState::default();
        assert_eq!(late_open.cancel(), None);
        assert!(!late_open.publish(42)); // caller closes the unpublished handle
        assert_eq!(late_open.cancel(), None);

        let mut early_open = ConsumerState::default();
        assert!(early_open.publish(42));
        assert_eq!(early_open.cancel(), Some(42)); // shutdown owns the close
        assert_eq!(early_open.cancel(), None); // consumer completion cannot double-close
        assert!(!early_open.publish(43));
    }

    #[test]
    fn windows_manifests_decode_present_properties_for_both_pointer_sizes() {
        for (provider, start_id, stop_id, runtime) in [(DXGI, 42, 43, 1), (D3D9, 1, 2, 2)] {
            for pointer_bytes in [4, 8] {
                let sink = Sink {
                    counter: Mutex::new(Counter::default()),
                    frequency: 60_000,
                    alive: AtomicBool::new(true),
                    consumer: Mutex::new(ConsumerState::default()),
                    events: AtomicU64::new(0),
                    target_events: AtomicU64::new(0),
                    decode_failures: AtomicU64::new(0),
                };
                sink.counter.lock().unwrap().select(42, 0);
                for n in 0..=60 {
                    let mut record: EVENT_RECORD = unsafe { std::mem::zeroed() };
                    record.EventHeader.Size = size_of::<EVENT_HEADER>() as u16;
                    record.EventHeader.Flags = if pointer_bytes == 4 {
                        EVENT_HEADER_FLAG_32_BIT_HEADER
                    } else {
                        EVENT_HEADER_FLAG_64_BIT_HEADER
                    } as u16;
                    record.EventHeader.ProviderId = provider;
                    record.EventHeader.ProcessId = 42;
                    record.EventHeader.ThreadId = 7;
                    record.EventHeader.TimeStamp = n * 1000;
                    record.EventHeader.EventDescriptor.Id = start_id;
                    record.EventHeader.EventDescriptor.Channel = 0x10;
                    record.EventHeader.EventDescriptor.Opcode = 1;
                    record.EventHeader.EventDescriptor.Task = if runtime == 1 { 9 } else { 1 };
                    record.EventHeader.EventDescriptor.Keyword = 0x8000_0000_0000_0002;
                    record.UserContext = (&sink as *const Sink).cast_mut().cast();
                    let mut payload = 0x1234_u64.to_le_bytes()[..pointer_bytes].to_vec();
                    payload.extend_from_slice(&[0; 16]);
                    record.UserData = payload.as_ptr().cast_mut().cast();
                    record.UserDataLength = payload.len() as u16;
                    unsafe {
                        event(&mut record);
                    }
                    let result = 0u32.to_le_bytes();
                    record.EventHeader.EventDescriptor.Id = stop_id;
                    record.EventHeader.EventDescriptor.Opcode = 2;
                    record.UserData = result.as_ptr().cast_mut().cast();
                    record.UserDataLength = result.len() as u16;
                    unsafe {
                        event(&mut record);
                    }
                }
                assert_eq!(
                    sink.counter.lock().unwrap().fps(60_000, 60_000),
                    Some(60.0),
                    "runtime {runtime}, pointer bytes {pointer_bytes}"
                );
            }
        }
    }

    #[test]
    fn idle_and_expired_leases_never_start_sampling_and_clear_state() {
        let mut supervisor = Supervisor::new();
        supervisor.tick(&[0]);
        assert!(supervisor.session.is_none());
        supervisor.reading(42);
        supervisor.requested = Some(Instant::now() - LEASE);
        supervisor.error = Some("old failure".into());
        supervisor.tick(&[0]);
        assert!(supervisor.session.is_none());
        assert_eq!(supervisor.sink.counter.lock().unwrap().pid, 0);
        assert!(supervisor.error.is_none());
        supervisor.reading(42);
        supervisor.reading(0);
        assert!(supervisor.requested.is_none());
    }
}
