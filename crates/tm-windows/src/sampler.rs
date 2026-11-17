//! 界面侧的采样器：从命名管道读助手推来的帧。
//!
//! 和 macOS 的 [`tm_macos::NettopSampler`] 行为一致：`sample()` 阻塞到下一帧，
//! 返回的是**累计值**，速率由 `tm_core::RateCalculator` 差分算出。
//!
//! 助手没启动 / 被关掉时不会 panic，而是持续重连；界面端表现为「等待数据」，
//! 日志里会打一行怎么装助手的提示。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use tm_core::{AppKey, ProcSample};
use tm_sampler::{Sampler, SamplerError};

use crate::pipe::PipeClient;
use crate::proto::Frame;

/// 连不上助手时重试的间隔
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub struct WindowsSampler {
    rx: Receiver<Frame>,
    interval: Duration,
}

impl WindowsSampler {
    pub fn spawn() -> Result<Self, SamplerError> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new()
            .name("pipe-reader".to_owned())
            .spawn(move || reader_loop(tx))
            .map_err(SamplerError::Spawn)?;

        Ok(Self {
            rx,
            interval: Duration::from_secs(1),
        })
    }
}

impl Sampler for WindowsSampler {
    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError> {
        // 助手每秒一帧，给 4 秒余量
        match self.rx.recv_timeout(Duration::from_secs(4)) {
            Ok(frame) => Ok(frame
                .procs
                .into_iter()
                .map(|p| ProcSample::new(p.pid, AppKey::new(p.name), p.rx, p.tx))
                .collect()),
            Err(RecvTimeoutError::Timeout) => Err(SamplerError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(SamplerError::Closed),
        }
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}

/// 读线程：连上就读，断了就重连，直到界面退出（发送端被丢弃）。
fn reader_loop(tx: mpsc::Sender<Frame>) {
    let mut warned = false;

    loop {
        let mut client = match PipeClient::connect(2000) {
            Ok(c) => c,
            Err(e) => {
                if !warned {
                    eprintln!("[windows] 连不上采集助手（{e}）。");
                    eprintln!(
                        "[windows] 请以管理员身份运行一次：tm-helper.exe --install"
                    );
                    warned = true;
                }
                std::thread::sleep(RECONNECT_DELAY);
                continue;
            }
        };

        if warned {
            eprintln!("[windows] 已连上采集助手");
            warned = false;
        }

        loop {
            match client.recv_frame() {
                Ok(frame) => {
                    if tx.send(frame).is_err() {
                        return; // 界面退出
                    }
                }
                Err(_) => break, // 助手重启或断开，回外层重连
            }
        }
    }
}
