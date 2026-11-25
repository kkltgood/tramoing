//! Read-only AppleSMC temperature keys. No privileged helper or fan writes.
//! ABI / sensor identities: https://github.com/exelban/stats/tree/master/SMC
//! and Modules/Sensors/values.swift. Unknown chips deliberately have no guessed keys.
use core_foundation_sys::dictionary::CFDictionaryRef;
use std::{
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    time::{Duration, Instant},
};
use tm_core::temperature::{hottest, valid, Temperatures};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Limits {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    memory: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInfo {
    size: u32,
    kind: u32,
    attributes: u8,
}
#[repr(C)]
#[derive(Default)]
struct Data {
    key: u32,
    version: Version,
    limits: Limits,
    info: KeyInfo,
    result: u8,
    status: u8,
    command: u8,
    index: u32,
    bytes: [u8; 32],
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const i8) -> CFDictionaryRef;
    fn IOServiceGetMatchingService(port: u32, matching: CFDictionaryRef) -> u32;
    fn IOServiceOpen(service: u32, task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOConnectCallStructMethod(
        connect: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
}

struct Smc(u32, HashMap<u32, KeyInfo>);
impl Smc {
    #[allow(deprecated)] // Match the existing Mach ABI without a new dependency.
    fn open() -> Result<Self, String> {
        unsafe {
            let matching = IOServiceMatching(c"AppleSMC".as_ptr());
            if matching.is_null() {
                return Err("AppleSMC matching unavailable".into());
            }
            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                return Err("AppleSMC service unavailable".into());
            }
            let mut connection = 0;
            let result = IOServiceOpen(service, libc::mach_task_self(), 0, &mut connection);
            IOObjectRelease(service);
            if result != 0 {
                return Err(format!("AppleSMC open: {result:#x}"));
            }
            Ok(Self(connection, HashMap::new()))
        }
    }
    fn call(&self, input: &Data) -> Option<Data> {
        let mut output = Data::default();
        let mut size = size_of::<Data>();
        let result = unsafe {
            IOConnectCallStructMethod(
                self.0,
                2,
                (input as *const Data).cast(),
                size_of::<Data>(),
                (&mut output as *mut Data).cast(),
                &mut size,
            )
        };
        (result == 0 && size == size_of::<Data>() && output.result == 0).then_some(output)
    }
    fn read(&mut self, key: &str) -> Option<f64> {
        let key = u32::from_be_bytes(key.as_bytes().try_into().ok()?);
        let info = if let Some(info) = self.1.get(&key) {
            *info
        } else {
            let info = self
                .call(&Data {
                    key,
                    command: 9,
                    ..Data::default()
                })?
                .info;
            if !matches!(
                (&info.kind.to_be_bytes(), info.size),
                (b"sp78", 2) | (b"flt ", 4)
            ) {
                return None;
            }
            self.1.insert(key, info);
            info
        };
        let result = self.call(&Data {
            key,
            info,
            command: 5,
            ..Data::default()
        })?;
        decode(info.kind.to_be_bytes(), info.size, &result.bytes)
    }
}
impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.0);
        }
    }
}

fn decode(kind: [u8; 4], size: u32, bytes: &[u8; 32]) -> Option<f64> {
    let value = match (&kind, size) {
        (b"sp78", 2) => i16::from_be_bytes([bytes[0], bytes[1]]) as f64 / 256.0,
        (b"flt ", 4) => f32::from_le_bytes(bytes[..4].try_into().ok()?) as f64,
        _ => return None,
    };
    valid(value)
}

fn keys(brand: &str) -> (&'static str, &'static str) {
    if !brand.contains("Apple") {
        return ("TC0D TCAD TCAC TCBC TC0F", "TCGC TG0D TG1D TGDD");
    }
    let chip = brand.split_whitespace().nth(1).unwrap_or_default();
    if chip == "M1" {
        (
            "Tp09 Tp0T Tp01 Tp05 Tp0D Tp0H Tp0L Tp0P Tp0X Tp0b",
            "Tg05 Tg0D Tg0L Tg0T",
        )
    } else if chip == "M2" {
        (
            "Tp1h Tp1t Tp1p Tp1l Tp01 Tp05 Tp09 Tp0D Tp0X Tp0b Tp0f Tp0j",
            "Tg0f Tg0j",
        )
    } else if chip == "M3" {
        (
            "Te05 Te0L Te0P Te0S Tf04 Tf09 Tf0A Tf0B Tf0D Tf0E Tf44 Tf49 Tf4A Tf4B Tf4D Tf4E",
            "Tf14 Tf18 Tf19 Tf1A Tf24 Tf28 Tf29 Tf2A",
        )
    } else if chip == "M4" {
        (
            "Te05 Te0S Te09 Te0H Tp01 Tp05 Tp09 Tp0D Tp0V Tp0Y Tp0b Tp0e",
            "Tg0G Tg0H Tg1U Tg1k Tg0K Tg0L Tg0d Tg0e Tg0j Tg0k",
        )
    } else if chip == "M5" {
        ("Tp00 Tp04 Tp08 Tp0C Tp0G Tp0K Tp0O Tp0R Tp0U Tp0X Tp0a Tp0d Tp0g Tp0j Tp0m Tp0p Tp0u Tp0y", "Tg0U Tg0X Tg0d Tg0g Tg0j Tg1Y Tg1c Tg1g")
    } else {
        ("", "")
    }
}

fn brand() -> String {
    let mut bytes = [0u8; 256];
    let mut len = bytes.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"machdep.cpu.brand_string".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return "Apple unknown".into();
    }
    String::from_utf8_lossy(&bytes[..len.min(bytes.len())])
        .trim_end_matches('\0')
        .to_owned()
}

pub struct Sampler {
    smc: Option<Smc>,
    retry: Instant,
    brand: String,
    failure: String,
}
impl Default for Sampler {
    fn default() -> Self {
        Self {
            smc: None,
            retry: Instant::now(),
            brand: brand(),
            failure: String::new(),
        }
    }
}
impl Sampler {
    pub fn sample(&mut self) -> Temperatures {
        if self.smc.is_none() && Instant::now() >= self.retry {
            self.retry = Instant::now() + Duration::from_secs(30);
            match Smc::open() {
                Ok(smc) => self.smc = Some(smc),
                Err(e) => self.failure = e,
            }
        }
        let Some(smc) = &mut self.smc else {
            return Temperatures {
                source: self.failure.clone(),
                ..Default::default()
            };
        };
        let (cpu_keys, gpu_keys) = keys(&self.brand);
        let cpu = hottest(cpu_keys.split_whitespace().filter_map(|key| smc.read(key)));
        let gpu = hottest(gpu_keys.split_whitespace().filter_map(|key| smc.read(key)));
        if cpu.is_none() && gpu.is_none() {
            self.smc = None;
        }
        Temperatures {
            cpu,
            gpu,
            source: format!(
                "AppleSMC ({}); CPU={}, GPU={}; hottest supported die/core sensor",
                self.brand,
                if cpu.is_some() {
                    "available"
                } else {
                    "unavailable"
                },
                if gpu.is_some() {
                    "available"
                } else {
                    "unavailable"
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_and_decoding_match_smc_wire_format() {
        assert_eq!(size_of::<Data>(), 80);
        assert_eq!(std::mem::offset_of!(Data, bytes), 48);
        let mut bytes = [0; 32];
        bytes[..2].copy_from_slice(&[0x36, 0x80]);
        assert_eq!(decode(*b"sp78", 2, &bytes), Some(54.5));
        bytes[..4].copy_from_slice(&47.25f32.to_le_bytes());
        assert_eq!(decode(*b"flt ", 4, &bytes), Some(47.25));
        assert_eq!(decode(*b"flt ", 2, &bytes), None);
        assert_eq!(decode(*b"ui32", 4, &bytes), None);
        assert_eq!(decode(*b"flt ", 4, &[0; 32]), None);
    }
    #[test]
    fn unknown_apple_chips_do_not_use_intel_proximity_keys() {
        assert_eq!(keys("Apple M99"), ("", ""));
        assert_eq!(keys("Apple M10"), ("", ""));
        for chip in [
            "Apple M1",
            "Apple M2 Max",
            "Apple M3 Pro",
            "Apple M4",
            "Apple M5",
        ] {
            let (cpu, gpu) = keys(chip);
            assert!(!cpu.is_empty() && !gpu.is_empty());
            assert!(cpu
                .split_whitespace()
                .chain(gpu.split_whitespace())
                .all(|k| k.len() == 4));
        }
    }
    #[test]
    #[ignore = "reads physical AppleSMC sensors"]
    fn physical_sensors() {
        eprintln!("{:?}", Sampler::default().sample());
    }
}
