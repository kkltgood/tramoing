//! GPU: WDDM telemetry. CPU: supervised built-in sensors, then optional WMI.
//! ACPI thermal zones are intentionally not labelled as CPU temperatures.
use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tm_core::temperature::{hottest, valid, Temperatures};
use windows_sys::Wdk::Graphics::Direct3D::*;

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
    pub fn sample(&mut self) -> Temperatures {
        if Instant::now() >= self.helper_retry {
            self.helper_reading = match crate::temperature_service::request() {
                Ok(reading) => {
                    self.helper_error.clear();
                    self.helper_retry = Instant::now() + Duration::from_secs(2);
                    Some(reading)
                }
                Err(error) => {
                    self.helper_error = format!("Built-in sensor helper unavailable: {error}");
                    self.helper_retry = Instant::now() + Duration::from_secs(10);
                    None
                }
            };
        }
        let native = self.helper_reading.as_ref().map(|r| r.temperatures());
        let driver_gpu = gpu_temperature();
        if let Some(native) = native
            .as_ref()
            .filter(|r| r.cpu.is_some() && driver_gpu.is_some())
        {
            // Built-in sensors recovered: release the fallback PowerShell process.
            self.provider = None;
            return with_gpu(native.clone(), driver_gpu);
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
        if let Some(native) = native {
            if native.cpu.is_some() {
                result.cpu = native.cpu;
                result.cpu_unavailable = None;
            }
            if result.gpu.is_none() {
                result.gpu = native.gpu;
            }
            if result.cpu.is_none() {
                result.cpu_unavailable = native.cpu_unavailable;
            }
            result.source = format!("{}; {}", native.source, result.source);
        } else {
            if result.cpu.is_none() {
                result.cpu_unavailable =
                    Some(tm_core::temperature::CpuUnavailable::HelperUnavailable);
            }
            result.source = format!("{}; {}", self.helper_error, result.source);
        }
        with_gpu(result, driver_gpu)
    }
}

fn with_gpu(mut result: Temperatures, driver_gpu: Option<f64>) -> Temperatures {
    if let Some(gpu) = driver_gpu {
        result.gpu = Some(gpu);
        result
            .source
            .push_str("; GPU=WDDM driver (hottest adapter)");
    } else {
        result.source.push_str("; GPU WDDM temperature unavailable");
    }
    result
}
