//! UI 端有界等待的管道采样器。无 detached 读线程，Drop 即关闭连接。
use crate::pipe::{PipeClient, PipeError};
use crate::proto::Frame;
use std::time::{Duration, Instant};
use tm_core::{AppKey, ProcSample};
use tm_sampler::{Sampler, SamplerError};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};

fn source_error(error: PipeError) -> SamplerError {
    let hint = match error {
        PipeError::Connect(ERROR_FILE_NOT_FOUND) => {
            "未连接到采集助手，请重新运行安装程序以修复采集助手"
        }
        PipeError::Connect(ERROR_ACCESS_DENIED) => {
            "无法访问采集助手，请在当前 Windows 账户下重新安装采集助手"
        }
        PipeError::Connect(ERROR_PIPE_BUSY) => {
            "采集连接被占用，请退出其他账户或旧版本中的 Tramoing"
        }
        _ => "采集连接中断，正在重试",
    };
    SamplerError::Source(format!("{hint}；{error}"))
}

pub struct WindowsSampler {
    client: Option<PipeClient>,
    reset: bool,
    last_seq: Option<u64>,
    source_clock: Option<(u64, Instant)>,
    last_source_us: Option<u64>,
    sampled_at: Option<Instant>,
}

impl WindowsSampler {
    pub fn spawn() -> Result<Self, SamplerError> {
        Ok(Self {
            client: None,
            reset: false,
            last_seq: None,
            source_clock: None,
            last_source_us: None,
            sampled_at: None,
        })
    }

    fn reset_connection(&mut self) {
        self.reset = true;
        self.last_seq = None;
        self.source_clock = None;
        self.last_source_us = None;
        self.sampled_at = None;
    }

    fn accept_frame(&mut self, frame: Frame, received_at: Instant) -> Option<Vec<ProcSample>> {
        if self.last_seq == Some(frame.seq) {
            return None;
        }
        if self.last_seq.is_some_and(|last| frame.seq < last)
            || self
                .last_source_us
                .zip(frame.sampled_at_us)
                .is_some_and(|(last, at)| at < last)
        {
            self.reset_connection();
        }
        self.last_seq = Some(frame.seq);
        self.last_source_us = frame.sampled_at_us;
        self.sampled_at = if let Some(at) = frame.sampled_at_us {
            let (base, anchor) = *self.source_clock.get_or_insert((at, received_at));
            anchor.checked_add(Duration::from_micros(at - base))
        } else {
            // 旧格式没有源时钟；再次收到时间戳时从当前接收时间重新锚定。
            self.source_clock = None;
            None
        };
        Some(
            frame
                .procs
                .into_iter()
                .map(|p| ProcSample::new(p.pid, AppKey::new(p.name), p.rx, p.tx))
                .collect(),
        )
    }
}

impl Sampler for WindowsSampler {
    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError> {
        if self.client.is_none() {
            match PipeClient::connect(0) {
                Ok(client) => {
                    self.client = Some(client);
                    self.reset_connection();
                }
                Err(error) => return Err(source_error(error)),
            }
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return Err(SamplerError::Timeout);
            }
            match self.client.as_mut().unwrap().poll_frame(wait) {
                Ok(frame) => {
                    if let Some(samples) = self.accept_frame(frame, Instant::now()) {
                        return Ok(samples);
                    }
                }
                Err(PipeError::Timeout) => return Err(SamplerError::Timeout),
                Err(error) => {
                    self.client = None;
                    return Err(source_error(error));
                }
            }
        }
    }

    fn sampled_at(&self) -> Option<Instant> {
        self.sampled_at
    }

    fn counters_start_at_zero(&self) -> bool {
        true
    }
    fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::ProcEntry;
    use tm_core::RateCalculator;

    #[test]
    fn pipe_failures_keep_their_cause_instead_of_becoming_timeouts() {
        for (error, expected) in [
            (PipeError::Connect(ERROR_FILE_NOT_FOUND), "未连接到采集助手"),
            (PipeError::Connect(ERROR_ACCESS_DENIED), "无法访问采集助手"),
            (PipeError::Connect(ERROR_PIPE_BUSY), "采集连接被占用"),
            (PipeError::Io(109), "109"),
            (PipeError::FrameTooLarge(5_000_000), "5000000"),
        ] {
            let detail = error.to_string();
            let error = source_error(error);
            assert!(matches!(error, SamplerError::Source(_)));
            let message = error.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(message.contains(&detail), "{message}");
        }
    }

    fn frame(seq: u64, at_us: Option<u64>, rx: u64) -> Frame {
        let mut frame = Frame::new(
            seq,
            vec![ProcEntry {
                pid: 1,
                name: "app.exe".into(),
                rx,
                tx: 0,
            }],
        );
        frame.sampled_at_us = at_us;
        frame
    }

    #[test]
    fn duplicate_frames_do_not_advance_the_rate_baseline() {
        let mut sampler = WindowsSampler::spawn().unwrap();
        let mut calc = RateCalculator::new();
        let start = Instant::now();
        for (seq, us, rx, arrived) in [
            (1, 0, 0, 0),
            (2, 1_000_000, 10_000, 1000),
            (2, 1_000_000, 10_000, 1500),
            (3, 2_000_000, 20_000, 3000),
            (4, 3_000_000, 30_000, 3010),
        ] {
            if let Some(samples) = sampler.accept_frame(
                frame(seq, Some(us), rx),
                start + Duration::from_millis(arrived),
            ) {
                calc.update(sampler.sampled_at().unwrap(), &samples);
            }
            if seq >= 2 {
                assert!((calc.total_bps().0 - 10_000.0).abs() < 0.1);
            }
        }
        assert_eq!(calc.session_total(), (30_000, 0));
    }

    #[test]
    fn reconnect_accepts_same_sequence_and_reanchors_source_time() {
        let mut sampler = WindowsSampler::spawn().unwrap();
        let start = Instant::now();
        sampler
            .accept_frame(frame(10, Some(9_000_000), 100), start)
            .unwrap();
        sampler.reset_connection();
        assert!(sampler
            .accept_frame(
                frame(10, Some(9_000_000), 100),
                start + Duration::from_secs(5)
            )
            .is_some());
        assert!(sampler.take_reset());
        assert_eq!(sampler.sampled_at(), Some(start + Duration::from_secs(5)));
        sampler
            .accept_frame(frame(1, Some(0), 0), start + Duration::from_secs(6))
            .unwrap();
        assert!(sampler.take_reset());
        assert_eq!(sampler.sampled_at(), Some(start + Duration::from_secs(6)));
    }

    #[test]
    fn legacy_frames_keep_arrival_time_fallback_and_deduplication() {
        let mut sampler = WindowsSampler::spawn().unwrap();
        let now = Instant::now();
        assert!(sampler.accept_frame(frame(1, None, 100), now).is_some());
        assert_eq!(sampler.sampled_at(), None);
        assert!(sampler.accept_frame(frame(1, None, 100), now).is_none());
        assert!(sampler.accept_frame(frame(2, None, 200), now).is_some());
    }

    #[test]
    fn source_clock_regression_resets_baseline_but_preserves_session_bytes() {
        let mut sampler = WindowsSampler::spawn().unwrap();
        let mut calc = RateCalculator::new();
        let start = Instant::now();
        for (seq, us, rx) in [
            (1, 9_000_000, 100),
            (2, 10_000_000, 10_100),
            (3, 0, 0),
            (4, 1_000_000, 10_000),
        ] {
            let samples = sampler
                .accept_frame(frame(seq, Some(us), rx), start + Duration::from_secs(seq))
                .unwrap();
            if sampler.take_reset() {
                assert_eq!(seq, 3);
                calc.reset_baseline();
            }
            calc.update(sampler.sampled_at().unwrap(), &samples);
        }
        assert!((calc.total_bps().0 - 10_000.0).abs() < 0.1);
        assert_eq!(calc.session_total(), (20_000, 0));
    }
}
