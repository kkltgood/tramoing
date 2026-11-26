//! Provider precedence shared by the Windows sampler and hardware-free tests.
use tm_core::temperature::{CpuUnavailable, Temperatures};

pub(crate) fn needs_wmi(native: Option<&Temperatures>, driver_gpu: Option<f64>) -> bool {
    native.is_none_or(|r| r.cpu.is_none() || (r.gpu.is_none() && driver_gpu.is_none()))
}

pub(crate) fn merge(
    native: Option<Temperatures>,
    mut fallback: Temperatures,
    helper_error: &str,
    driver_gpu: Option<f64>,
) -> Temperatures {
    let native_gpu = native.as_ref().and_then(|r| r.gpu);
    let gpu_source = if driver_gpu.is_some() {
        "GPU=WDDM driver (hottest adapter)"
    } else if native_gpu.is_some() {
        "GPU=built-in sensors (WDDM unavailable)"
    } else if fallback.gpu.is_some() {
        "GPU=WMI provider (WDDM unavailable)"
    } else {
        "GPU temperature unavailable (WDDM and sensor providers)"
    };
    fallback.gpu = driver_gpu.or(native_gpu).or(fallback.gpu);
    let source = if let Some(native) = native {
        if native.cpu.is_some() {
            fallback.cpu = native.cpu;
            fallback.cpu_unavailable = None;
        } else if fallback.cpu.is_none() {
            fallback.cpu_unavailable = native.cpu_unavailable;
        }
        native.source
    } else {
        if fallback.cpu.is_none() {
            fallback.cpu_unavailable = Some(CpuUnavailable::HelperUnavailable);
        }
        helper_error.to_owned()
    };
    fallback.source = [source.as_str(), fallback.source.as_str(), gpu_source]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(cpu: Option<f64>, gpu: Option<f64>) -> Temperatures {
        Temperatures {
            cpu,
            gpu,
            ..Default::default()
        }
    }

    #[test]
    fn built_in_gpu_works_without_wddm_or_wmi() {
        let native = reading(Some(86.0), Some(54.0));
        assert!(!needs_wmi(Some(&native), None));
        let result = merge(Some(native), Temperatures::default(), "", None);
        assert_eq!((result.cpu, result.gpu), (Some(86.0), Some(54.0)));
        assert!(result.source.contains("GPU=built-in sensors"));
    }

    #[test]
    fn wddm_then_built_in_then_wmi_gpu_precedence() {
        for (driver, native_gpu, expected) in [
            (Some(51.0), Some(54.0), 51.0),
            (None, Some(54.0), 54.0),
            (None, None, 55.0),
        ] {
            let result = merge(
                Some(reading(Some(86.0), native_gpu)),
                reading(Some(85.0), Some(55.0)),
                "",
                driver,
            );
            assert_eq!(result.cpu, Some(86.0));
            assert_eq!(result.gpu, Some(expected));
        }
    }

    #[test]
    fn incomplete_readings_still_request_wmi() {
        assert!(needs_wmi(None, Some(54.0)));
        assert!(needs_wmi(Some(&reading(None, Some(54.0))), None));
        assert!(needs_wmi(Some(&reading(Some(86.0), None)), None));
        assert!(!needs_wmi(Some(&reading(Some(86.0), None)), Some(54.0)));
    }

    #[test]
    fn gpu_only_worker_keeps_cpu_unavailable_reason_until_wmi_recovers() {
        let mut native = reading(None, Some(54.0));
        native.cpu_unavailable = Some(CpuUnavailable::SensorUnavailable);
        let missing = merge(Some(native.clone()), Temperatures::default(), "", None);
        assert_eq!(missing.cpu, None);
        assert_eq!(missing.gpu, Some(54.0));
        assert_eq!(
            missing.cpu_unavailable,
            Some(CpuUnavailable::SensorUnavailable)
        );
        let recovered = merge(Some(native), reading(Some(86.0), None), "", None);
        assert_eq!(recovered.cpu, Some(86.0));
        assert_eq!(recovered.cpu_unavailable, None);
    }

    #[test]
    fn lost_providers_clear_readings_and_keep_diagnostics() {
        let result = merge(None, Temperatures::default(), "Helper disconnected", None);
        assert_eq!((result.cpu, result.gpu), (None, None));
        assert_eq!(
            result.cpu_unavailable,
            Some(CpuUnavailable::HelperUnavailable)
        );
        assert!(result.source.contains("Helper disconnected"));
        assert!(result.source.contains("GPU temperature unavailable"));
    }
}
