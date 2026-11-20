//! 普通权限下读取 Windows PDH 的整机 CPU / GPU 使用率，不依赖 ETW 助手。
//! CPU 覆盖所有处理器组；GPU 按物理设备与引擎合并进程，再取最忙引擎。

use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Performance::*;

const CPU_PATH: &str = r"\Processor Information(_Total)\% Processor Time";
const GPU_PATH: &str = r"\GPU Engine(*)\Utilization Percentage";
const RETRY: Duration = Duration::from_secs(30);

/// 必须在性能线程中首次调用 sample，PDH 的初始化和采样都可能阻塞。
pub struct Sampler {
    cpu: Option<Query>,
    gpu: Option<Query>,
    retry_at: Instant,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            cpu: None,
            gpu: None,
            retry_at: Instant::now(),
        }
    }
}

impl Sampler {
    pub fn sample(&mut self) -> (Option<f64>, Option<f64>) {
        if Instant::now() >= self.retry_at {
            if self.cpu.is_none() {
                self.cpu = Query::open(CPU_PATH);
            }
            if self.gpu.is_none() {
                self.gpu = Query::open(GPU_PATH);
            }
            self.retry_at = Instant::now() + RETRY;
        }
        let cpu = self
            .cpu
            .as_mut()
            .and_then(|query| query.collect().then(|| query.value()).flatten());
        let gpu = self
            .gpu
            .as_mut()
            .and_then(|query| query.collect().then(|| query.engines()).flatten());
        (cpu, gpu)
    }
}

struct Query {
    handle: isize,
    counter: isize,
    primed: bool,
}

impl Query {
    fn open(path: &str) -> Option<Self> {
        let mut query = Self {
            handle: 0,
            counter: 0,
            primed: false,
        };
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            if PdhOpenQueryW(null(), 0, &mut query.handle) != 0 {
                return None;
            }
            // EnglishCounter 由 PDH 转成本地化名称，中文 Windows 同样可用。
            if PdhAddEnglishCounterW(query.handle, path.as_ptr(), 0, &mut query.counter) != 0 {
                return None;
            }
        }
        Some(query)
    }

    fn collect(&mut self) -> bool {
        let collected = unsafe { PdhCollectQueryData(self.handle) == 0 };
        let previous = std::mem::replace(&mut self.primed, collected);
        // 百分比需要连续两次采样；失败后也重新建立基线。
        collected && previous
    }

    fn value(&self) -> Option<f64> {
        let mut value: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
        if unsafe {
            PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, null_mut(), &mut value)
        } != 0
        {
            return None;
        }
        counter_value(&value).map(|value| value.min(100.0))
    }

    fn engines(&self) -> Option<f64> {
        let mut bytes = 0;
        let mut count = 0;
        if unsafe {
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut count,
                null_mut(),
            )
        } != PDH_MORE_DATA
        {
            return None;
        }
        // GPU 进程会动态增减。容量竞争时有限重试，并给结构体提供 8 字节对齐。
        for _ in 0..3 {
            if bytes == 0 || bytes > 16 * 1024 * 1024 {
                return None;
            }
            let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
            let capacity = buffer.len() * 8;
            bytes = capacity as u32;
            let result = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut bytes,
                    &mut count,
                    buffer.as_mut_ptr().cast(),
                )
            };
            if result == PDH_MORE_DATA {
                continue;
            }
            if result != 0
                || count as usize > capacity / std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()
            {
                return None;
            }
            let items = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                    count as usize,
                )
            };
            let entries: Vec<_> = items
                .iter()
                .filter_map(|item| {
                    let value = counter_value(&item.FmtValue)?;
                    let offset = (item.szName as usize).checked_sub(buffer.as_ptr() as usize)?;
                    if offset >= capacity || offset % 2 != 0 {
                        return None;
                    }
                    let name =
                        unsafe { std::slice::from_raw_parts(item.szName, (capacity - offset) / 2) };
                    let length = name.iter().position(|c| *c == 0)?;
                    Some((String::from_utf16_lossy(&name[..length]), value))
                })
                .collect();
            return busiest_engine(entries.iter().map(|(name, value)| (name.as_str(), *value)));
        }
        None
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe {
                PdhCloseQuery(self.handle);
            }
        }
    }
}

fn counter_value(value: &PDH_FMT_COUNTERVALUE) -> Option<f64> {
    if !matches!(value.CStatus, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA) {
        return None;
    }
    let value = unsafe { value.Anonymous.doubleValue };
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn engine_key(name: &str) -> Option<&str> {
    let (pid, device) = name.strip_prefix("pid_")?.split_once("_luid_")?;
    pid.parse::<u32>().ok()?;
    let (key, _) = device.split_once("_engtype_")?;
    let fields: Vec<_> = key.split('_').collect();
    if fields.len() != 6 || fields[2] != "phys" || fields[4] != "eng" {
        return None;
    }
    for field in &fields[..2] {
        u32::from_str_radix(field.strip_prefix("0x")?, 16).ok()?;
    }
    fields[3].parse::<u32>().ok()?;
    fields[5].parse::<u32>().ok()?;
    Some(key)
}

fn busiest_engine<'a>(values: impl Iterator<Item = (&'a str, f64)>) -> Option<f64> {
    let mut engines = HashMap::<&str, f64>::new();
    for (name, value) in values {
        if value.is_finite() && value >= 0.0 {
            if let Some(key) = engine_key(name) {
                *engines.entry(key).or_default() += value;
            }
        }
    }
    engines
        .values()
        .copied()
        .reduce(f64::max)
        .map(|v| v.min(100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_sums_processes_on_same_engine_without_summing_devices_or_engines() {
        let entries = [
            (
                "pid_10_luid_0x00000000_0x00000001_phys_0_eng_0_engtype_3D",
                25.0,
            ),
            (
                "pid_11_luid_0x00000000_0x00000001_phys_0_eng_0_engtype_3D",
                35.0,
            ),
            (
                "pid_10_luid_0x00000000_0x00000001_phys_0_eng_1_engtype_Copy",
                50.0,
            ),
            (
                "pid_12_luid_0x00000000_0x00000002_phys_0_eng_0_engtype_3D",
                45.0,
            ),
            (
                "pid_13_luid_0x00000000_0x00000001_phys_1_eng_0_engtype_3D",
                55.0,
            ),
        ];
        assert_eq!(busiest_engine(entries.into_iter()), Some(60.0));
    }

    #[test]
    fn missing_invalid_and_idle_gpu_are_distinct() {
        let name = "pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D";
        assert_eq!(busiest_engine(std::iter::empty()), None);
        assert_eq!(busiest_engine([("unrecognized", 10.0)].into_iter()), None);
        for value in [f64::NAN, f64::INFINITY, -1.0] {
            assert_eq!(busiest_engine([(name, value)].into_iter()), None);
        }
        assert_eq!(busiest_engine([(name, 0.0)].into_iter()), Some(0.0));
        assert_eq!(
            busiest_engine([(name, 60.0), (name, 60.0)].into_iter()),
            Some(100.0)
        );
    }

    #[test]
    fn pdh_status_is_checked_before_using_percentage() {
        let mut value: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
        value.Anonymous.doubleValue = 42.0;
        assert_eq!(counter_value(&value), Some(42.0));
        value.CStatus = PDH_CSTATUS_NEW_DATA;
        assert_eq!(counter_value(&value), Some(42.0));
        value.CStatus = PDH_CSTATUS_INVALID_DATA;
        assert_eq!(counter_value(&value), None);
    }

    #[test]
    #[ignore = "requires Windows performance counters; GPU support depends on the driver"]
    fn native_cpu_and_gpu_return_percentages() {
        let mut sampler = Sampler::default();
        assert_eq!(sampler.sample(), (None, None));
        std::thread::sleep(Duration::from_secs(1));
        let (cpu, gpu) = sampler.sample();
        assert!(
            cpu.is_some_and(|v| (0.0..=100.0).contains(&v)),
            "CPU {cpu:?}"
        );
        assert!(gpu.is_none_or(|v| (0.0..=100.0).contains(&v)));
        eprintln!("CPU {cpu:?}% GPU {gpu:?}%");
    }
}
