//! `nettop` 采样器：按连接解析，再把流量归属回发起方。
//!
//! 结构上分两个线程：
//! - 读线程：`BufReader` 阻塞读 nettop 的 CSV 输出，凑满一帧就丢进 channel；
//! - 调用方：`sample()` 阻塞等 channel，拿到帧后做归属并输出累计值。
//!
//! 这样 `sample()` 天然是「阻塞到下一帧」，UI 主循环不会被 `read()` 卡住。
//!
//! stdout 走**伪终端**而不是管道：管道会让 stdio 变成全缓冲，
//! 每帧要攒满 4 KB 才吐一次，实测 3 秒才来一批（详见 [`crate::pty`]）。
//!
//! # 为什么不用 `-P` 的进程汇总
//!
//! 见 [`crate::flows`]：汇总口径下走系统代理的流量会被重复计算，或者只能记到
//! 代理名下。按连接解析才能既保证总量、又归到发起方。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use tm_core::{AppKey, ProcSample};
use tm_sampler::{Sampler, SamplerError};

use crate::appid::AppResolver;
use crate::flows::{Bytes, FlowAccounting, FlowParser, FlowRow};
use crate::pty;

/// 默认的 nettop 路径。可用 `TM_NETTOP_BIN` 覆盖（测试或非标准安装）。
pub const DEFAULT_NETTOP: &str = "/usr/bin/nettop";

/// 每多少帧清理一次应用缓存
const CACHE_GC_FRAMES: u64 = 300;

/// 多久没出现就把应用从累计表里摘掉（约 10 分钟）
const APP_IDLE_FRAMES: u64 = 600;

/// 伪终端的宽高（字符数）。给足宽度，免得 nettop 把名字和数字挤成一列。
const PTY_COLS: u16 = 400;
const PTY_ROWS: u16 = 60;

/// 合成 pid 的起点。真实 pid 不会到这里，避免和进程表混淆。
const SYNTH_PID_BASE: u32 = 1_000_000;

pub struct NettopSampler {
    rx: Receiver<(Instant, Vec<FlowRow>)>,
    child: Child,
    resolver: AppResolver,
    accounting: FlowAccounting,
    /// 归属后的累计字节（按应用）
    cum: HashMap<AppKey, (u64, u64)>,
    synth: HashMap<AppKey, u32>,
    next_pid: u32,
    last_seen: HashMap<AppKey, u64>,
    interval: Duration,
    frames: u64,
    reader: Option<thread::JoinHandle<()>>,
    sampled_at: Option<Instant>,
}

impl NettopSampler {
    pub fn spawn() -> Result<Self, SamplerError> {
        Self::spawn_with_interval(Duration::from_secs(1))
    }

    pub fn spawn_with_interval(interval: Duration) -> Result<Self, SamplerError> {
        let bin = std::env::var("TM_NETTOP_BIN").unwrap_or_else(|_| DEFAULT_NETTOP.to_owned());
        Self::spawn_command(&bin, interval)
    }

    fn spawn_command(bin: &str, interval: Duration) -> Result<Self, SamplerError> {
        // nettop 只认整数秒。小于 1 秒按 1 秒算 —— 传 0 会让它退化成忙轮询。
        let interval = Duration::from_secs(interval.as_secs().max(1));
        let delay = interval.as_secs().to_string();

        // -L 0：CSV + 一直采；-n：不做地址反解（稳定、快，名字里也不会冒逗号）
        // 不加 -t：回环连接是回填代理流量的依据，总量在 flows 模块里只取外网。
        let args: Vec<&str> = vec!["-x", "-n", "-L", "0", "-s", delay.as_str()];

        let mut cmd = Command::new(bin);
        // nettop 即使在日志模式也监听 stdin。/dev/null 会持续报告 EOF，
        // 在部分 macOS 版本上触发 dispatch 忙循环，占满一个 CPU 核心。
        // 用管道并由 Child 持有写端：不发送输入，也不提前关闭它。
        cmd.args(&args).stdin(Stdio::piped()).stderr(Stdio::null());

        // 关键：stdout 必须是 pty，否则 nettop 走全缓冲，3 秒才吐一批
        let (mut child, source): (Child, Box<dyn Read + Send>) = match pty::open(PTY_COLS, PTY_ROWS)
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
        let reader = thread::Builder::new()
            .name("nettop-reader".to_owned())
            .spawn(move || {
                let reader = BufReader::new(source);
                let mut parser = FlowParser::new();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Some(rows) = parser.feed(&line) {
                        // 接收端没了（程序退出）就结束线程
                        // 在读线程记录帧到达时间，数据库阻塞后消费积压帧时
                        // 仍按原始间隔计算速率，不能按几毫秒的消费间隔计算。
                        if tx.send((Instant::now(), rows)).is_err() {
                            return;
                        }
                    }
                }
                // 进程退出后没有下一次表头来切帧，已读到的连接仍应交付。
                let rows = parser.flush();
                if !rows.is_empty() {
                    let _ = tx.send((Instant::now(), rows));
                }
            })
            .map_err(|e| {
                let _ = child.kill();
                let _ = child.wait();
                SamplerError::Spawn(e)
            })?;

        Ok(Self {
            rx,
            child,
            resolver: AppResolver::new(),
            accounting: FlowAccounting::new(),
            cum: HashMap::new(),
            synth: HashMap::new(),
            next_pid: SYNTH_PID_BASE,
            last_seen: HashMap::new(),
            interval,
            frames: 0,
            reader: Some(reader),
            sampled_at: None,
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
        let wait = self
            .interval
            .saturating_mul(3)
            .saturating_add(Duration::from_secs(2));
        let (at, rows) = match self.rx.recv_timeout(wait) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => return Err(SamplerError::Timeout),
            Err(RecvTimeoutError::Disconnected) => return Err(SamplerError::Closed),
        };
        self.sampled_at = Some(at);

        self.frames += 1;

        // 拆开借用：accounting 要 &mut，resolver 也要 &mut
        let Self {
            accounting,
            resolver,
            cum,
            last_seen,
            frames,
            ..
        } = self;
        let frame = *frames;
        let deltas = accounting.update(&rows, |pid, name| {
            AppKey::new(resolver.resolve(pid, name).key)
        });

        for (app, d) in deltas {
            let e = cum.entry(app.clone()).or_insert((0, 0));
            e.0 = e.0.saturating_add(d.rx);
            e.1 = e.1.saturating_add(d.tx);
            last_seen.insert(app, frame);
        }

        if frame.is_multiple_of(CACHE_GC_FRAMES) {
            let alive: Vec<u32> = rows.iter().map(|r| r.pid).collect();
            self.resolver.retain_only(&alive);

            // 长时间没流量的应用不再下发，避免累计表无限膨胀
            let cut = frame.saturating_sub(APP_IDLE_FRAMES);
            self.cum
                .retain(|app, _| *self.last_seen.get(app).unwrap_or(&0) >= cut);
            let keep = self.cum.keys().cloned().collect::<Vec<_>>();
            self.last_seen.retain(|app, _| keep.contains(app));
            self.synth.retain(|app, _| keep.contains(app));
        }

        // 每帧都下发全部应用：RateCalculator 的基线是按帧重建的，
        // 某个应用缺席一帧会让它那一帧的增量白白丢掉。
        let Self {
            cum,
            synth,
            next_pid,
            ..
        } = self;
        for app in cum.keys() {
            if !synth.contains_key(app) {
                *next_pid += 1;
                synth.insert(app.clone(), *next_pid);
            }
        }
        let mut out = Vec::with_capacity(cum.len());
        for (app, (rx, tx)) in cum.iter() {
            let pid = *synth.get(app).expect("刚补齐过");
            out.push(ProcSample::new(pid, app.clone(), *rx, *tx));
        }

        Ok(out)
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn sampled_at(&self) -> Option<Instant> {
        self.sampled_at
    }

    fn counters_start_at_zero(&self) -> bool {
        true
    }
}

impl Drop for NettopSampler {
    fn drop(&mut self) {
        // 一定要杀子进程，否则 nettop 会一直留着
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl NettopSampler {
    /// 某个应用累计归属到的字节（调试用）。
    pub fn total_bytes(&self, app: &str) -> Bytes {
        let (rx, tx) = self.cum.get(&AppKey::new(app)).copied().unwrap_or((0, 0));
        Bytes { rx, tx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_exit_preserves_the_last_frame_without_a_following_header() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("collector");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
printf 'time,,interface,state,bytes_in,bytes_out,\ntime,,interface,state,bytes_in,bytes_out,\n'
printf 't,test-client.999999,,,0,0,\n'
printf 't,tcp4 192.0.2.1:50000<->198.51.100.1:443,en0,Established,100,25,\n'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut sampler =
            NettopSampler::spawn_command(bin.to_str().unwrap(), Duration::from_secs(1)).unwrap();
        sampler.sample().expect("initial baseline");
        sampler.sample().expect("final frame before collector exit");
        assert_eq!(
            sampler.total_bytes("test-client"),
            Bytes { rx: 100, tx: 25 }
        );
        assert!(matches!(sampler.sample(), Err(SamplerError::Closed)));
    }

    #[test]
    fn collector_input_remains_open_and_can_receive_commands() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("collector");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
printf 'time,,interface,state,bytes_in,bytes_out,\ntime,,interface,state,bytes_in,bytes_out,\n'
if ! IFS= read -r name; then name=stdin-eof; fi
printf 't,%s.999999,,,0,0,\n' "$name"
printf 't,tcp4 192.0.2.1:50000<->198.51.100.1:443,en0,Established,100,0,\ntime,,interface,state,bytes_in,bytes_out,\n'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut sampler =
            NettopSampler::spawn_command(bin.to_str().unwrap(), Duration::from_secs(1)).unwrap();
        sampler.sample().expect("collector ready");
        sampler
            .child
            .stdin
            .as_mut()
            .expect("keep the input pipe open")
            .write_all(b"test-client\n")
            .unwrap();
        sampler.sample().expect("collector received input");
        assert_eq!(sampler.total_bytes("test-client").rx, 100);
        assert_eq!(sampler.total_bytes("stdin-eof").rx, 0);
    }

    #[test]
    fn queued_frames_use_read_time_instead_of_consumption_time() {
        // 使用立刻退出的本地程序，随后注入帧，不运行真实 nettop 或修改环境变量。
        let mut sampler =
            NettopSampler::spawn_command("/usr/bin/true", Duration::from_millis(20)).unwrap();
        assert_eq!(sampler.interval(), Duration::from_secs(1));
        let (tx, rx) = mpsc::channel();
        sampler.rx = rx;
        let start = Instant::now();
        for i in 0..3 {
            let mut parser = FlowParser::new();
            parser.feed("time,,interface,state,bytes_in,bytes_out,");
            parser.feed("t,test-client.999999,,,0,0,");
            parser.feed(&format!(
                "t,tcp4 192.0.2.1:50000<->198.51.100.1:443,en0,Established,{},0,",
                i * 1000
            ));
            tx.send((start + Duration::from_secs(i), parser.flush()))
                .unwrap();
        }
        let mut calc = tm_core::RateCalculator::with_smoothing(1.0);
        calc.set_new_counters_start_at_zero(true);
        for i in 0..3 {
            let samples = sampler.sample().unwrap();
            assert_eq!(sampler.sampled_at(), Some(start + Duration::from_secs(i)));
            calc.update(sampler.sampled_at().unwrap(), &samples);
            if i > 0 {
                assert_eq!(calc.total_bps(), (1000.0, 0.0));
            }
        }
        assert_eq!(calc.session_total(), (2000, 0));
    }

    #[test]
    fn spawn_fails_cleanly_when_binary_missing() {
        // 指向一个不存在的可执行文件，应返回 Spawn 错误而不是 panic
        let r = NettopSampler::spawn_command("/nonexistent/nettop", Duration::from_secs(1));
        assert!(matches!(r, Err(SamplerError::Spawn(_))));
    }
}
