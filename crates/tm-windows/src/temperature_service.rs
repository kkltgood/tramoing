//! Isolated sensor worker supervised by the elevated helper. The UI can only
//! request a reading or recheck the installed component; it cannot supply paths,
//! arguments, files, or driver commands.
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::SystemInformation::IMAGE_FILE_MACHINE_AMD64;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

use crate::network_control::{self, Server};
use crate::temperature_protocol::{Request, Snapshot, Status, MAX_FRAME};

// Allow several request attempts even at the slowest (five-second) UI cadence.
// This lease only controls worker lifetime; WATCHDOG independently rejects stale data.
const LEASE: Duration = Duration::from_secs(20);
const WATCHDOG: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_WARNING_INTERVAL: Duration = Duration::from_secs(30);

fn pipe_name() -> Result<String, String> {
    Ok(format!(
        r"\\.\pipe\Tramoing-Temperature-{}-v1",
        crate::scheduled_task::current_user_sid()?
    ))
}

pub fn request() -> Result<Snapshot, String> {
    network_control::request_named(&pipe_name()?, &Request::Read {}, REQUEST_TIMEOUT)
        .map_err(|error| error.replace("网络控制", "温度采集"))
}

/// Recheck only the component at the helper's fixed installation path. This is
/// separate from downloading/installing and never grants callers an execute API.
pub fn refresh() -> Result<Snapshot, String> {
    network_control::request_named(&pipe_name()?, &Request::Refresh {}, REQUEST_TIMEOUT)
        .map_err(|error| error.replace("网络控制", "温度采集"))
}

/// Check the native OS, not the process target: an x64 process under ARM64
/// emulation must not enter LibreHardwareMonitor's x86 CPU/driver code.
pub fn supported_platform() -> bool {
    let mut process = 0;
    let mut native = 0;
    unsafe {
        IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) != 0
            && native == IMAGE_FILE_MACHINE_AMD64
    }
}

/// FDD apphosts use the registered machine runtime. Do not inherit environment
/// overrides that could redirect an elevated worker/probe to user-owned code,
/// enable a profiler/startup hook, change roll-forward, or write a trace file.
pub fn clean_runtime_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if ["DOTNET_", "COMPLUS_", "COR_", "CORECLR_", "COREHOST_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
            || upper == "DEVPATH"
        {
            command.env_remove(key);
        }
    }
}

struct Worker {
    child: Child,
    job: Option<OwnedHandle>,
    receiver: Receiver<Snapshot>,
    updated: Instant,
    reading: Snapshot,
}

impl Worker {
    fn start() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("sensors")
            .join("tramoing-sensors.exe");
        let mut command = Command::new(&executable);
        command.current_dir(executable.parent().unwrap());
        Self::start_command(command)
    }

    fn start_command(mut command: Command) -> Result<Self, String> {
        clean_runtime_environment(&mut command);
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|e| e.to_string())?;
        // The worker waits for this handshake before loading any sensor libraries.
        // A job closes with the helper, including taskkill /F and installer upgrades.
        if unsafe { AssignProcessToJobObject(raw, child.as_raw_handle()) } == 0 {
            let error = std::io::Error::last_os_error().to_string();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let start = child.stdin.take().unwrap().write_all(b"start\n");
        if let Err(error) = start {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.to_string());
        }
        let mut stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::sync_channel(2);
        if let Err(error) = std::thread::Builder::new()
            .name("sensor-output".into())
            .spawn(move || {
                while let Ok(frame) = read_frame(&mut stdout) {
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
            })
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.to_string());
        }
        Ok(Self {
            child,
            job: Some(job),
            receiver,
            updated: Instant::now(),
            reading: Snapshot::unavailable(Status::Starting, "Sensor worker starting"),
        })
    }

    fn poll(&mut self) -> bool {
        // Even a faulty producer emitting continuously cannot starve requests.
        for _ in 0..2 {
            let Ok(reading) = self.receiver.try_recv() else {
                break;
            };
            self.reading = reading;
            self.updated = Instant::now();
        }
        match self.child.try_wait() {
            Ok(None) => {}
            result => {
                self.reading = Snapshot::unavailable(
                    Status::ProcessExited,
                    format!("Sensor worker exited: {result:?}"),
                );
                return false;
            }
        }
        if self.updated.elapsed() > WATCHDOG {
            self.reading = Snapshot::unavailable(
                Status::TimedOut,
                "Sensor worker did not respond within 15 seconds",
            );
            return false;
        }
        true
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.job.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_frame(reader: &mut impl Read) -> std::io::Result<Snapshot> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

struct Supervisor {
    worker: Option<Worker>,
    requested: Option<Instant>,
    retry: Instant,
    reading: Snapshot,
}

impl Supervisor {
    fn new() -> Self {
        Self {
            worker: None,
            requested: None,
            retry: Instant::now(),
            reading: Snapshot::unavailable(Status::Starting, "Sensor worker idle"),
        }
    }

    fn tick(&mut self) {
        if self.requested.is_none_or(|at| at.elapsed() > LEASE) {
            if self.worker.take().is_some() {
                log::info!(target: "tramoing::temperature", "Sensor worker stopped: no temperature request within {} seconds", LEASE.as_secs());
                self.retry = Instant::now();
            }
            self.reading = Snapshot::unavailable(Status::Starting, "Sensor worker idle");
            return;
        }
        // Launch outside the pipe request so process creation never extends a
        // caller's deadline or blocks the UI's utilization sampling thread.
        if self.worker.is_none() && Instant::now() >= self.retry {
            self.start();
        }
        if let Some(worker) = &mut self.worker {
            let running = worker.poll();
            self.reading = worker.reading.clone();
            if !running {
                log::warn!(target: "tramoing::temperature", "{}", self.reading.source);
                self.worker = None;
                self.retry = Instant::now() + RETRY;
            }
        }
    }

    fn start(&mut self) {
        self.retry = Instant::now() + RETRY;
        if !supported_platform() {
            self.reading = Snapshot::unavailable(
                Status::UnsupportedPlatform,
                "Built-in temperature sensors support native Windows x64 only",
            );
            return;
        }
        let installed = std::env::current_exe().ok().is_some_and(|p| {
            p.with_file_name("sensors")
                .join("tramoing-sensors.exe")
                .is_file()
        });
        self.reading = if !installed {
            Snapshot::unavailable(
                Status::ComponentMissing,
                "Built-in sensor component missing; download it from Settings",
            )
        } else {
            match Worker::start() {
                Ok(worker) => {
                    self.worker = Some(worker);
                    Snapshot::unavailable(Status::Starting, "Sensor worker starting")
                }
                Err(e) => Snapshot::unavailable(
                    Status::Failed,
                    format!("Cannot start sensor worker: {e}"),
                ),
            }
        };
    }

    fn reading(&mut self) -> Snapshot {
        self.requested = Some(Instant::now());
        self.reading.clone()
    }

    fn refresh(&mut self) -> Snapshot {
        self.worker.take();
        self.retry = Instant::now();
        self.requested = Some(self.retry);
        self.reading = Snapshot::unavailable(Status::Starting, "Rechecking sensor component");
        self.reading.clone()
    }
}

/// Separate from ETW and firewall control. One short request renews a 20-second
/// lease, so hiding temperatures, closing the UI or a UI crash stops sampling.
pub fn run() -> ! {
    let mut supervisor = Supervisor::new();
    let mut request_warning_at: Option<Instant> = None;
    let mut suppressed_requests = 0u64;
    loop {
        let server = pipe_name().and_then(|name| Server::create_named(&name));
        let server = match server {
            Ok(server) => server,
            Err(error) => {
                log::warn!(target: "tramoing::temperature", "{error}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let handle = server.raw_handle();
        loop {
            supervisor.tick();
            let ok = unsafe { ConnectNamedPipe(handle, null_mut()) };
            let code = unsafe { GetLastError() };
            if ok != 0 || code != ERROR_PIPE_CONNECTED {
                if ok == 0 && code == ERROR_NO_DATA {
                    unsafe { DisconnectNamedPipe(handle) };
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut serve = || -> Result<(), String> {
                let deadline = Instant::now() + REQUEST_TIMEOUT;
                network_control::verify_peer(handle, false)?;
                let reading = match network_control::receive(handle, deadline)? {
                    Request::Read {} => supervisor.reading(),
                    Request::Refresh {} => supervisor.refresh(),
                };
                network_control::send(handle, &reading, deadline)?;
                network_control::read(handle, &mut [0; 1], deadline)
            };
            if let Err(error) = serve() {
                let now = Instant::now();
                if request_warning_at
                    .is_none_or(|last| now.duration_since(last) >= REQUEST_WARNING_INTERVAL)
                {
                    log::warn!(target: "tramoing::temperature", "Temperature request failed (deadline {} ms, {} similar failures suppressed): {}", REQUEST_TIMEOUT.as_millis(), suppressed_requests, error.replace("网络控制", "温度采集"));
                    request_warning_at = Some(now);
                    suppressed_requests = 0;
                } else {
                    suppressed_requests = suppressed_requests.saturating_add(1);
                }
            }
            unsafe { DisconnectNamedPipe(handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_protocol_rejects_large_truncated_and_unknown_frames() {
        assert!(read_frame(&mut (MAX_FRAME as u32 + 1).to_le_bytes().as_slice()).is_err());
        assert!(read_frame(&mut [2, 0, 0, 0, b'{'].as_slice()).is_err());
        let frame = Snapshot::unavailable(Status::DriverMissing, "PawnIO missing");
        let bytes = serde_json::to_vec(&frame).unwrap();
        let mut wire = (bytes.len() as u32).to_le_bytes().to_vec();
        wire.extend(bytes);
        assert_eq!(
            read_frame(&mut wire.as_slice()).unwrap().status,
            Status::DriverMissing
        );
    }

    #[test]
    fn expired_ui_lease_discards_cached_temperature() {
        let mut supervisor = Supervisor::new();
        supervisor.requested = Some(Instant::now() - LEASE - Duration::from_secs(1));
        supervisor.reading = Snapshot {
            cpu: Some(72.0),
            gpu: None,
            status: Status::Ready,
            source: "test".into(),
        };
        supervisor.tick();
        assert_eq!(supervisor.reading.temperatures().cpu, None);
    }

    fn fake_worker(script: &str) -> Worker {
        let mut command = Command::new(
            std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        );
        command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        Worker::start_command(command).unwrap()
    }

    #[test]
    fn crashed_and_stalled_workers_clear_readings_and_are_reaped() {
        let mut crashed = fake_worker("[void][Console]::ReadLine(); exit 0");
        crashed.child.wait().unwrap();
        crashed.reading = Snapshot {
            cpu: Some(65.0),
            gpu: None,
            status: Status::Ready,
            source: "old".into(),
        };
        assert!(!crashed.poll());
        assert_eq!(crashed.reading.status, Status::ProcessExited);
        assert_eq!(crashed.reading.temperatures().cpu, None);

        let mut stalled = fake_worker("[void][Console]::ReadLine(); Start-Sleep -Seconds 60");
        let process = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
                0,
                stalled.child.id(),
            )
        };
        assert!(!process.is_null());
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        stalled.updated = Instant::now() - WATCHDOG - Duration::from_secs(1);
        assert!(!stalled.poll());
        assert_eq!(stalled.reading.status, Status::TimedOut);
        drop(stalled);
        assert_eq!(
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    process.as_raw_handle(),
                    2000,
                )
            },
            WAIT_OBJECT_0
        );
    }

    #[test]
    fn transient_request_gap_does_not_restart_a_live_sensor_worker() {
        let mut supervisor = Supervisor::new();
        let worker = fake_worker("[void][Console]::ReadLine(); Start-Sleep -Seconds 60");
        let pid = worker.child.id();
        supervisor.worker = Some(worker);
        // Includes a missed five-second poll, a one-second request deadline,
        // the next five-second poll and some scheduling delay.
        supervisor.requested = Some(Instant::now() - Duration::from_secs(13));
        supervisor.tick();
        assert_eq!(supervisor.worker.as_ref().unwrap().child.id(), pid);

        supervisor.requested = Some(Instant::now() - LEASE - Duration::from_secs(1));
        supervisor.tick();
        assert!(supervisor.worker.is_none());
        assert_eq!(supervisor.reading.temperatures().cpu, None);
        assert_eq!(supervisor.reading.source, "Sensor worker idle");
    }

    #[test]
    fn closing_supervisor_job_terminates_sensor_child() {
        let mut worker = fake_worker("[void][Console]::ReadLine(); Start-Sleep -Seconds 60");
        // The helper may be forcibly stopped without running Worker::drop.
        // Closing its job handle must still terminate the sensor process.
        worker.job.take();
        assert_eq!(
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    worker.child.as_raw_handle(),
                    2000,
                )
            },
            WAIT_OBJECT_0
        );
        let _ = worker.child.wait();
    }
}
