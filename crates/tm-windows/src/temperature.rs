//! GPU: WDDM telemetry, then built-in sensors. CPU: built-in sensors. WMI fills gaps.
//! ACPI thermal zones are intentionally not labelled as CPU temperatures.
use crate::temperature_sources::{merge, needs_wmi};
use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tm_core::temperature::{hottest, valid, Temperatures};
use windows_sys::Wdk::Graphics::Direct3D::*;

const HELPER_INTERVAL: Duration = Duration::from_secs(2);
const HELPER_RETRY: Duration = Duration::from_secs(1);

// https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_adapter_perfdata
fn gpu_temperature() -> Option<f64> {
    unsafe {
        let mut adapters: D3DKMT_ENUMADAPTERS = std::mem::zeroed();
        if D3DKMTEnumAdapters(&mut adapters) < 0 {
            return None;
        }
        let mut values = Vec::new();
        for adapter in adapters.Adapters.iter().take(adapters.NumAdapters as usize) {
            // EnumAdapters opens the handles; close every one, including failed queries.
            let mut count = 1u32;
            let mut query = D3DKMT_QUERYADAPTERINFO {
                hAdapter: adapter.hAdapter,
                Type: KMTQAITYPE_PHYSICALADAPTERCOUNT,
                pPrivateDriverData: (&mut count as *mut u32).cast(),
                PrivateDriverDataSize: std::mem::size_of_val(&count) as u32,
            };
            if D3DKMTQueryAdapterInfo(&mut query) < 0 {
                count = 1;
            }
            for index in 0..count.min(16) {
                let mut data: D3DKMT_ADAPTER_PERFDATA = std::mem::zeroed();
                data.PhysicalAdapterIndex = index;
                query.Type = KMTQAITYPE_ADAPTERPERFDATA;
                query.pPrivateDriverData = (&mut data as *mut D3DKMT_ADAPTER_PERFDATA).cast();
                query.PrivateDriverDataSize = std::mem::size_of_val(&data) as u32;
                if D3DKMTQueryAdapterInfo(&mut query) >= 0 {
                    values.push(data.Temperature as f64 / 10.0);
                }
            }
            D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER {
                hAdapter: adapter.hAdapter,
            });
        }
        hottest(values)
    }
}

#[derive(serde::Deserialize)]
struct Frame {
    sequence: u64,
    cpu: Option<f64>,
    gpu: Option<f64>,
    source: String,
}

struct Provider {
    child: Child,
    directory: tempfile::TempDir,
    sequence: u64,
    updated: Instant,
    reading: Temperatures,
}
impl Provider {
    fn start() -> std::io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("tramoing-temperature-")
            .tempdir()?;
        let script = directory.path().join("read.ps1");
        fs::write(&script, include_str!("temperature.ps1"))?;
        let system = std::env::var_os("SystemRoot")
            .ok_or_else(|| std::io::Error::other("SystemRoot unavailable"))?;
        let child = Command::new(
            std::path::PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        )
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .env("TM_TEMPERATURE_OWNER", std::process::id().to_string())
        .env(
            "TM_TEMPERATURE_OUTPUT",
            directory.path().join("reading.json"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()?;
        Ok(Self {
            child,
            directory,
            sequence: 0,
            updated: Instant::now(),
            reading: Temperatures::default(),
        })
    }
    fn poll(&mut self) -> bool {
        let read = || -> Option<Frame> {
            let mut text = String::new();
            fs::File::open(self.directory.path().join("reading.json"))
                .ok()?
                .take(8192)
                .read_to_string(&mut text)
                .ok()?;
            serde_json::from_str(&text).ok()
        };
        if let Some(frame) = read().filter(|f| f.sequence > self.sequence) {
            self.sequence = frame.sequence;
            self.updated = Instant::now();
            self.reading = Temperatures {
                cpu: frame.cpu.and_then(valid),
                gpu: frame.gpu.and_then(valid),
                source: frame.source,
                ..Default::default()
            };
        }
        // A stalled WMI service must never freeze utilization, settings or shutdown.
        if self.updated.elapsed() > Duration::from_secs(15) {
            self.reading = Temperatures {
                source: "Temperature WMI query timed out".into(),
                ..Default::default()
            };
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                if self.sequence == 0
                    || !status.success()
                    || self.reading.cpu.is_some()
                    || self.reading.gpu.is_some()
                {
                    self.reading = Temperatures {
                        source: format!(
                            "Temperature WMI process exited: {status}; {}",
                            self.reading.source
                        ),
                        ..Default::default()
                    };
                }
                false
            }
            Err(error) => {
                self.reading = Temperatures {
                    source: format!("Temperature WMI process failed: {error}"),
                    ..Default::default()
                };
                false
            }
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Sampler {
    provider: Option<Provider>,
    retry: Instant,
    status: String,
    helper_retry: Instant,
    helper_reading: Option<crate::temperature_protocol::Snapshot>,
    helper_error: String,
}
impl Default for Sampler {
    fn default() -> Self {
        Self {
            provider: None,
            retry: Instant::now(),
            status: "WMI temperature provider starting".into(),
            helper_retry: Instant::now(),
            helper_reading: None,
            helper_error: String::new(),
        }
    }
}
impl Sampler {
    fn accept_helper_result(
        &mut self,
        result: Result<crate::temperature_protocol::Snapshot, String>,
        completed_at: Instant,
    ) {
        self.helper_reading = match result {
            Ok(reading) => {
                self.helper_error.clear();
                self.helper_retry = completed_at + HELPER_INTERVAL;
                Some(reading)
            }
            Err(error) => {
                self.helper_error = format!("Built-in sensor helper unavailable: {error}");
                // Retry on the next performance sample. A short IPC delay must not
                // become a ten-second outage or let the worker's lease expire.
                self.helper_retry = completed_at + HELPER_RETRY;
                // Never keep displaying an old temperature after losing its source.
                None
            }
        };
    }

    pub fn sample(&mut self) -> Temperatures {
        if Instant::now() >= self.helper_retry {
            let result = crate::temperature_service::request();
            self.accept_helper_result(result, Instant::now());
        }
        let native = self.helper_reading.as_ref().map(|r| r.temperatures());
        let driver_gpu = gpu_temperature();
        if !needs_wmi(native.as_ref(), driver_gpu) {
            // Built-in sensors recovered: release the fallback PowerShell process.
            self.provider = None;
            return merge(native, Temperatures::default(), "", driver_gpu);
        }
        if self.provider.is_none() && Instant::now() >= self.retry {
            self.retry = Instant::now() + Duration::from_secs(60);
            match Provider::start() {
                Ok(provider) => self.provider = Some(provider),
                Err(e) => self.status = format!("Temperature provider launch failed: {e}"),
            }
        }
        let mut result = Temperatures {
            source: self.status.clone(),
            ..Default::default()
        };
        if let Some(provider) = &mut self.provider {
            let running = provider.poll();
            if !provider.reading.source.is_empty() {
                result = provider.reading.clone();
            }
            if !running {
                self.status = result.source.clone();
                // Process exit invalidates all cached values, including a previously good reading.
                result.cpu = None;
                result.gpu = None;
                self.provider = None;
                self.retry = Instant::now() + Duration::from_secs(60);
            }
        }
        merge(native, result, &self.helper_error, driver_gpu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temperature_protocol::{Snapshot, Status};

    fn ready(cpu: f64) -> Snapshot {
        Snapshot {
            cpu: Some(cpu),
            gpu: None,
            status: Status::Ready,
            source: "test sensor".into(),
        }
    }

    #[test]
    fn transient_helper_failure_clears_old_temperature_and_recovers_next_sample() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        sampler.accept_helper_result(Ok(ready(65.0)), start);
        let failure_at = start + Duration::from_secs(2);
        sampler.accept_helper_result(Err("读取温度采集消息超时".into()), failure_at);
        assert!(sampler.helper_reading.is_none());
        assert!(sampler.helper_error.contains("超时"));

        let next_sample = failure_at + Duration::from_secs(1);
        assert!(sampler.helper_retry <= next_sample);
        sampler.accept_helper_result(Ok(ready(67.0)), next_sample);
        assert_eq!(
            sampler.helper_reading.as_ref().unwrap().temperatures().cpu,
            Some(67.0)
        );
        assert!(sampler.helper_error.is_empty());
    }

    #[test]
    fn repeated_helper_failures_do_not_restore_cached_values_or_lengthen_backoff() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        sampler.accept_helper_result(Ok(ready(65.0)), start);
        for second in 2..20 {
            let at = start + Duration::from_secs(second);
            sampler.accept_helper_result(Err("helper unavailable".into()), at);
            assert!(sampler.helper_reading.is_none());
            assert!(sampler.helper_retry <= at + Duration::from_secs(1));
            assert!(sampler.helper_error.contains("helper unavailable"));
        }
        // A successful IPC response reporting a sensor failure is not a reading.
        sampler.accept_helper_result(
            Ok(Snapshot::unavailable(
                Status::ProcessExited,
                "worker exited",
            )),
            start + Duration::from_secs(20),
        );
        assert_eq!(
            sampler.helper_reading.as_ref().unwrap().temperatures().cpu,
            None
        );
    }
}
