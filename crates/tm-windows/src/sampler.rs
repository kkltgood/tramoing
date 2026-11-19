//! UI 端有界等待的管道采样器。无 detached 读线程，Drop 即关闭连接。
use crate::pipe::{PipeClient, PipeError};
use std::time::Duration;
use tm_core::{AppKey, ProcSample};
use tm_sampler::{Sampler, SamplerError};

pub struct WindowsSampler {
    client: Option<PipeClient>,
    reset: bool,
    last_seq: Option<u64>,
}

impl WindowsSampler {
    pub fn spawn() -> Result<Self, SamplerError> {
        Ok(Self {
            client: None,
            reset: false,
            last_seq: None,
        })
    }
}

impl Sampler for WindowsSampler {
    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError> {
        if self.client.is_none() {
            match PipeClient::connect(0) {
                Ok(client) => {
                    self.client = Some(client);
                    self.reset = true;
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(500));
                    return Err(SamplerError::Timeout);
                }
            }
        }
        match self
            .client
            .as_mut()
            .unwrap()
            .poll_frame(Duration::from_millis(500))
        {
            Ok(frame) => {
                if self.last_seq.is_some_and(|last| frame.seq < last) {
                    self.reset = true;
                }
                self.last_seq = Some(frame.seq);
                Ok(frame
                    .procs
                    .into_iter()
                    .map(|p| ProcSample::new(p.pid, AppKey::new(p.name), p.rx, p.tx))
                    .collect())
            }
            Err(PipeError::Timeout) => Err(SamplerError::Timeout),
            Err(_) => {
                self.client = None;
                Err(SamplerError::Closed)
            }
        }
    }

    fn counters_start_at_zero(&self) -> bool {
        true
    }
    fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }
}
