//! `nettop` 采样器：把子进程输出变成一帧帧 [`ProcSample`]。
//!
//! 结构上分两个线程：
//! - 读线程：`BufReader` 阻塞读 nettop 的输出，凑满一帧就丢进 channel；
//! - 调用方：`sample()` 阻塞等 channel，拿到帧后把 pid 解析成应用。
//!
//! 这样 `sample()` 天然是「阻塞到下一帧」，UI 主循环不会被 `read()` 卡住。
//!
//! stdout 走**伪终端**而不是管道：管道会让 stdio 变成全缓冲，
//! 每帧要攒满 4 KB 才吐一次，实测 3 秒才来一批（详见 [`crate::pty`]）。

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use tm_core::{AppKey, ProcSample};
use tm_sampler::{Sampler, SamplerError};

use crate::appid::AppResolver;
use crate::nettop::{NettopParser, ProcRow};
use crate::pty;

/// 默认的 nettop 路径。可用 `TM_NETTOP_BIN` 覆盖（测试或非标准安装）。
pub const DEFAULT_NETTOP: &str = "/usr/bin/nettop";

/// 每多少帧清理一次应用缓存
const CACHE_GC_FRAMES: u64 = 300;

/// 伪终端的宽高（字符数）。给足宽度，免得 nettop 把名字和数字挤成一列。
const PTY_COLS: u16 = 400;
const PTY_ROWS: u16 = 60;

pub struct NettopSampler {
    rx: Receiver<Vec<ProcRow>>,
    child: Child,
    resolver: AppResolver,
    interval: Duration,
    frames: u64,
}

impl NettopSampler {
    pub fn spawn() -> Result<Self, SamplerError> {
        Self::spawn_with_interval(Duration::from_secs(1))
    }

    pub fn spawn_with_interval(interval: Duration) -> Result<Self, SamplerError> {
        let bin = std::env::var("TM_NETTOP_BIN").unwrap_or_else(|_| DEFAULT_NETTOP.to_owned());
        // nettop 只认整数秒。小于 1 秒按 1 秒算 —— 传 0 会让它退化成忙轮询。
        let delay = interval.as_secs().max(1).to_string();

        let mut cmd = Command::new(&bin);
        cmd.args([
            "-P",
            "-x",
            "-l",
            "0",
            "-s",
            delay.as_str(),
            "-J",
            "bytes_in,bytes_out",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null());

        // 关键：stdout 必须是 pty，否则 nettop 走全缓冲，3 秒才吐一批
        let (child, source): (Child, Box<dyn Read + Send>) = match pty::open(PTY_COLS, PTY_ROWS)
        {
            Ok((master, slave)) => {
                cmd.stdout(Stdio::from(slave));
                let child = cmd.spawn().map_err(SamplerError::Spawn)?;
                (child, Box::new(master))
            }
            Err(e) => {
                // 拿不到 pty 也别直接罢工：管道能跑，只是刷新会慢到 3 秒一批
                eprintln!("[nettop] 伪终端打不开（{e}），退回管道：刷新率会降到 3 秒");
                cmd.stdout(Stdio::piped());
                let mut child = cmd.spawn().map_err(SamplerError::Spawn)?;
                let out = child
                    .stdout
                    .take()
                    .ok_or(SamplerError::Unsupported("拿不到 nettop 的 stdout"))?;
                (child, Box::new(out))
            }
        };

        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("nettop-reader".to_owned())
            .spawn(move || {
                let reader = BufReader::new(source);
                let mut parser = NettopParser::new();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Some(rows) = parser.feed(&line) {
                        // 接收端没了（程序退出）就结束线程
                        if tx.send(rows).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(SamplerError::Spawn)?;

        Ok(Self {
            rx,
            child,
            resolver: AppResolver::new(),
            interval,
            frames: 0,
        })
    }

    /// 已采集的帧数，调试用。
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

impl Sampler for NettopSampler {
    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError> {
        // 正常情况下 nettop 每 interval 给一帧，给 3 倍余量后仍没有就认为卡住了
        let wait = self.interval * 3 + Duration::from_secs(2);
        let rows = match self.rx.recv_timeout(wait) {
            Ok(rows) => rows,
            Err(RecvTimeoutError::Timeout) => return Err(SamplerError::Timeout),
            Err(RecvTimeoutError::Disconnected) => return Err(SamplerError::Closed),
        };

        self.frames += 1;

        let samples: Vec<ProcSample> = rows
            .iter()
            .map(|r| {
                let id = self.resolver.resolve(r.pid);
                ProcSample::new(r.pid, AppKey::new(id.key), r.rx, r.tx)
            })
            .collect();

        if self.frames % CACHE_GC_FRAMES == 0 {
            let alive: Vec<u32> = rows.iter().map(|r| r.pid).collect();
            self.resolver.retain_only(&alive);
        }

        Ok(samples)
    }

    fn interval(&self) -> Duration {
        self.interval
    }
}

impl Drop for NettopSampler {
    fn drop(&mut self) {
        // 一定要杀子进程，否则 nettop 会一直留着
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_fails_cleanly_when_binary_missing() {
        // 指向一个不存在的可执行文件，应返回 Spawn 错误而不是 panic
        let old = std::env::var("TM_NETTOP_BIN").ok();
        std::env::set_var("TM_NETTOP_BIN", "/nonexistent/nettop");
        let r = NettopSampler::spawn();
        match old {
            Some(v) => std::env::set_var("TM_NETTOP_BIN", v),
            None => std::env::remove_var("TM_NETTOP_BIN"),
        }
        assert!(matches!(r, Err(SamplerError::Spawn(_))));
    }
}
