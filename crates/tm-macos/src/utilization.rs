//! 整机 CPU 与 GPU 使用率。Mach / IOKit 只读采集，不启动子进程或要求提权。

use std::ffi::{c_char, c_void, CStr};

use core_foundation_sys::base::{CFAllocatorRef, CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
};
use core_foundation_sys::number::{kCFNumberDoubleType, CFNumberGetTypeID, CFNumberGetValue};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringRef};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(port: u32, matching: CFDictionaryRef, iter: *mut u32) -> i32;
    fn IOIteratorNext(iter: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
}

extern "C" {
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

struct IoObject(u32);

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: 仅包装由 IOKit 返回、归调用者持有的对象。
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

struct CfObject(CFTypeRef);

impl CfObject {
    fn string(value: &CStr) -> Option<Self> {
        // SAFETY: 参数是有效的静态 UTF-8 C 字符串，创建的引用由 Self 回收。
        let value = unsafe {
            CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), kCFStringEncodingUTF8)
        };
        (!value.is_null()).then(|| Self(value.cast()))
    }
}

impl Drop for CfObject {
    fn drop(&mut self) {
        // SAFETY: Self 仅持有非空的 Create-rule 引用。
        unsafe { CFRelease(self.0) };
    }
}

#[derive(Default)]
pub struct CpuSampler {
    previous: Option<[u32; 4]>,
}

impl CpuSampler {
    /// 首次采样只建立基线；之后返回所有逻辑核心的平均忙碌比例 0..100。
    pub fn sample(&mut self) -> Option<f64> {
        let current = cpu_ticks();
        let previous = std::mem::replace(&mut self.previous, current);
        cpu_percent(previous?, current?)
    }
}

#[allow(deprecated)] // libc 保留的 Mach ABI；避免为两次调用额外增加 mach2 依赖。
fn cpu_ticks() -> Option<[u32; 4]> {
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: info 的布局来自 libc，count 按 Mach 的 integer_t 数目填写。
    let status = unsafe {
        let host = libc::mach_host_self();
        let status = libc::host_statistics(
            host,
            libc::HOST_CPU_LOAD_INFO,
            (&mut info as *mut libc::host_cpu_load_info).cast(),
            &mut count,
        );
        // mach_host_self 返回一个 send right，每次读取后归还。
        mach_port_deallocate(libc::mach_task_self(), host);
        status
    };
    (status == 0 && count == libc::HOST_CPU_LOAD_INFO_COUNT).then_some(info.cpu_ticks)
}

fn cpu_percent(previous: [u32; 4], current: [u32; 4]) -> Option<f64> {
    let deltas: [u64; 4] = std::array::from_fn(|i| current[i].wrapping_sub(previous[i]) as u64);
    let total: u64 = deltas.iter().sum();
    let idle = deltas[libc::CPU_STATE_IDLE as usize];
    (total > 0).then(|| (total - idle) as f64 / total as f64 * 100.0)
}

/// 驱动报告的设备忙碌比例。多 GPU 使用最大值，不把百分比累加；不支持时为 None。
pub fn gpu_percent() -> Option<f64> {
    let stats_key = CfObject::string(c"PerformanceStatistics")?;
    let device_key = CfObject::string(c"Device Utilization %")?;
    let activity_key = CfObject::string(c"GPU Activity(%)")?;
    // SAFETY: IOKit 消费 matching 的引用，包括失败路径；iterator 由 RAII 释放。
    let iterator = unsafe {
        let matching = IOServiceMatching(c"IOAccelerator".as_ptr());
        if matching.is_null() {
            return None;
        }
        let mut iterator = 0;
        let status = IOServiceGetMatchingServices(0, matching, &mut iterator);
        let iterator = IoObject(iterator);
        if status != 0 {
            return None;
        }
        iterator
    };
    let mut maximum: Option<f64> = None;
    loop {
        // SAFETY: 有效 iterator；0 表示迭代结束。
        let service = IoObject(unsafe { IOIteratorNext(iterator.0) });
        if service.0 == 0 {
            break;
        }
        let property = unsafe {
            IORegistryEntryCreateCFProperty(service.0, stats_key.0.cast(), std::ptr::null(), 0)
        };
        if property.is_null() {
            continue;
        }
        let stats = CfObject(property);
        // 类型可能随驱动而异，检查后才转换；字典内的数值是借用，不单独 release。
        if unsafe { CFGetTypeID(stats.0) != CFDictionaryGetTypeID() } {
            continue;
        }
        let value = dictionary_number(stats.0.cast(), device_key.0)
            .or_else(|| dictionary_number(stats.0.cast(), activity_key.0));
        if let Some(value) = value.filter(|v| valid_percent(*v)) {
            maximum = Some(maximum.map_or(value, |old| old.max(value)));
        }
    }
    maximum
}

fn dictionary_number(dict: CFDictionaryRef, key: CFTypeRef) -> Option<f64> {
    // SAFETY: 调用方已验证字典类型，key 是活跃的 CFString，值的类型也先检查。
    unsafe {
        let number = CFDictionaryGetValue(dict, key);
        if number.is_null() || CFGetTypeID(number) != CFNumberGetTypeID() {
            return None;
        }
        let mut result = 0.0_f64;
        let ok = CFNumberGetValue(
            number.cast(),
            kCFNumberDoubleType,
            (&mut result as *mut f64).cast::<c_void>(),
        );
        ok.then_some(result)
    }
}

fn valid_percent(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_interval_deltas_and_normalizes_all_cores() {
        assert_eq!(
            cpu_percent([100, 200, 300, 400], [110, 210, 370, 410]),
            Some(30.0)
        );
        assert_eq!(cpu_percent([0; 4], [40, 40, 0, 0]), Some(100.0));
        assert_eq!(cpu_percent([0; 4], [0, 0, 80, 0]), Some(0.0));
        assert_eq!(cpu_percent([9; 4], [9; 4]), None);
    }

    #[test]
    fn cpu_handles_wrapping_kernel_counters() {
        assert_eq!(
            cpu_percent([u32::MAX - 4, 0, 0, 0], [5, 0, 10, 0]),
            Some(50.0)
        );
    }

    #[test]
    fn invalid_gpu_readings_are_not_presented_as_zero() {
        for v in [-1.0, 101.0, f64::NAN, f64::INFINITY] {
            assert!(!valid_percent(v));
        }
        assert!(valid_percent(0.0));
        assert!(valid_percent(100.0));
    }

    #[test]
    #[ignore = "requires a macOS machine with an accelerator driver"]
    fn native_cpu_and_gpu_return_percentages() {
        let mut cpu = CpuSampler::default();
        assert_eq!(cpu.sample(), None);
        std::thread::sleep(std::time::Duration::from_millis(250));
        let cpu = cpu.sample().expect("CPU interval");
        assert!(valid_percent(cpu));
        let gpu = gpu_percent().expect("GPU driver utilization");
        assert!(valid_percent(gpu));
        eprintln!("CPU {cpu:.1}% GPU {gpu:.1}%");
    }
}
