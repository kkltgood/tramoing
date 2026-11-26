//! Versioned, bounded sensor messages. This module does not depend on Windows.
use serde::{Deserialize, Serialize};
use tm_core::temperature::{valid, CpuUnavailable, Temperatures};

pub const MAX_FRAME: usize = 8192;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", deny_unknown_fields)]
pub enum Request {
    Read {},
    /// Discard a previous failure and recheck the fixed installed component.
    /// No paths, installer commands or caller-supplied arguments are accepted.
    Refresh {},
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Starting,
    ComponentMissing,
    DriverMissing,
    DriverUnavailable,
    PermissionDenied,
    UnsupportedPlatform,
    SensorUnavailable,
    TimedOut,
    ProcessExited,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
    pub status: Status,
    pub source: String,
}

impl Snapshot {
    pub fn unavailable(status: Status, source: impl Into<String>) -> Self {
        Self {
            cpu: None,
            gpu: None,
            status,
            source: source.into(),
        }
    }

    pub fn temperatures(&self) -> Temperatures {
        // Failure frames must not preserve the previous reading, even if a
        // malfunctioning provider includes numbers in the error response.
        let ready = self.status == Status::Ready;
        Temperatures {
            cpu: ready.then_some(self.cpu).flatten().and_then(valid),
            gpu: ready.then_some(self.gpu).flatten().and_then(valid),
            source: format!("Built-in sensors ({:?}): {}", self.status, self.source),
            cpu_unavailable: if ready && self.cpu.and_then(valid).is_some() {
                None
            } else {
                Some(match self.status {
                    Status::Starting => CpuUnavailable::Starting,
                    Status::ComponentMissing => CpuUnavailable::ComponentMissing,
                    Status::DriverMissing => CpuUnavailable::DriverMissing,
                    Status::DriverUnavailable => CpuUnavailable::DriverUnavailable,
                    Status::PermissionDenied => CpuUnavailable::PermissionDenied,
                    Status::UnsupportedPlatform => CpuUnavailable::UnsupportedPlatform,
                    Status::Ready | Status::SensorUnavailable => CpuUnavailable::SensorUnavailable,
                    _ => CpuUnavailable::Failed,
                })
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_or_invalid_readings_never_become_temperatures() {
        let mut frame = Snapshot {
            cpu: Some(61.0),
            gpu: Some(54.0),
            status: Status::Ready,
            source: "test".into(),
        };
        assert_eq!(frame.temperatures().cpu, Some(61.0));
        frame.status = Status::TimedOut;
        assert_eq!(frame.temperatures().cpu, None);
        assert_eq!(frame.temperatures().gpu, None);
        frame.status = Status::Ready;
        frame.cpu = Some(f64::NAN);
        frame.gpu = Some(0.0);
        assert_eq!(frame.temperatures().cpu, None);
        assert_eq!(frame.temperatures().gpu, None);
    }

    #[test]
    fn ready_gpu_does_not_require_a_cpu_sensor() {
        let frame: Snapshot = serde_json::from_str(
            r#"{"cpu":null,"gpu":54.0,"status":"ready","source":"GPU=NVIDIA GeForce GT 730"}"#,
        )
        .unwrap();
        let reading = frame.temperatures();
        assert_eq!(reading.cpu, None);
        assert_eq!(reading.gpu, Some(54.0));
        assert_eq!(
            reading.cpu_unavailable,
            Some(CpuUnavailable::SensorUnavailable)
        );
    }

    #[test]
    fn request_cannot_supply_a_program_or_arguments() {
        assert!(serde_json::from_str::<Request>(r#"{"command":"Read"}"#).is_ok());
        assert!(serde_json::from_str::<Request>(r#"{"command":"Refresh"}"#).is_ok());
        assert!(
            serde_json::from_str::<Request>(r#"{"command":"Read","path":"other.exe"}"#).is_err()
        );
        assert!(serde_json::from_str::<Request>(r#"{"command":"Launch"}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"command":"Refresh","path":"other.exe"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<Request>(r#"{"command":"Refresh","args":["--install"]}"#)
                .is_err()
        );
        let frame: Snapshot = serde_json::from_str(r#"{"cpu":null,"gpu":null,"status":"driver_missing","source":"PawnIO is not installed"}"#).unwrap();
        assert_eq!(frame.status, Status::DriverMissing);
    }
}
