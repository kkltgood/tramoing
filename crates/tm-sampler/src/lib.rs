//! 采样源抽象。
//!
//! 上层的速率聚合（`tm-core`）和界面（`tray-app`）只认 [`Sampler`]，
//! 不关心下面到底是 nettop、ETW 还是一段假数据。这样三件事变得可能：
//! - 在 Windows 上开发和调试 macOS 的界面逻辑（反之亦然）；
//! - 没有采样权限的机器也能跑通整条管道；
//! - 单测不需要真网络、真进程。

use std::time::{Duration, Instant};

use tm_core::{AppKey, ConnectionReport, ProcSample};

/// 采样失败的原因。刻意做得很薄，避免为一个内部错误引入 anyhow。
#[derive(Debug)]
pub enum SamplerError {
    /// 采样器起不来（可执行文件不存在、权限不足等）
    Spawn(std::io::Error),
    /// 采样管道断了（子进程退出、ETW 会话被停）
    Closed,
    /// 等不到下一帧（系统卡住或采样器挂了）
    Timeout,
    /// 采样源连接或协议错误，保留平台提供的原因，不能混同于短暂等待。
    Source(String),
    /// 当前平台没有实现
    Unsupported(&'static str),
}

impl std::fmt::Display for SamplerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "采样器启动失败：{e}"),
            Self::Closed => write!(f, "采样管道已关闭"),
            Self::Timeout => write!(f, "等待采样数据超时"),
            Self::Source(message) => f.write_str(message),
            Self::Unsupported(what) => write!(f, "{what} 在当前平台尚不支持"),
        }
    }
}

impl std::error::Error for SamplerError {}

impl From<std::io::Error> for SamplerError {
    fn from(e: std::io::Error) -> Self {
        Self::Spawn(e)
    }
}

/// 采样源。实现者阻塞到「下一帧数据可用」再返回。
///
/// 返回的是**累计值**而非速率 —— 差分和平滑由 `tm_core::RateCalculator` 负责。
/// 这样采样器不需要知道时间窗口，算法也只有一份。
pub trait Sampler: Send {
    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError>;

    /// Current sockets owned by this application, queried only while details are visible.
    /// This must not change traffic counters or infer the original destination of a proxy.
    fn connections(&mut self, _app: &AppKey) -> Result<ConnectionReport, SamplerError> {
        Err(SamplerError::Unsupported("应用连接详情"))
    }

    /// 最近成功一帧的采集时间，映射到本进程的单调时钟。没有源时间戳时用接收时间。
    fn sampled_at(&self) -> Option<Instant> {
        None
    }

    /// 建议的采样间隔，UI 用它决定刷新节奏。
    fn interval(&self) -> Duration {
        Duration::from_secs(1)
    }

    /// true 表示计数器由采集器从零累计，新应用的首批字节可以直接计入。
    fn counters_start_at_zero(&self) -> bool {
        false
    }

    /// 计数器来源重启后只建立一次新基线，避免把旧助手历史当成新流量。
    fn take_reset(&mut self) -> bool {
        false
    }
}

/// 假采样源：按固定节奏生成几条渐变流量，用来在没有采样权限的机器上
/// 跑通界面、调试排版和动画。
#[derive(Debug)]
pub struct MockSampler {
    tick: u32,
    interval: Duration,
    totals: [(u64, u64); 5],
}

impl Default for MockSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSampler {
    pub fn new() -> Self {
        Self {
            tick: 0,
            interval: Duration::from_secs(1),
            totals: [(0, 0); 5],
        }
    }

    /// 加快节奏，做 UI 动画调试时有用。
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

impl Sampler for MockSampler {
    fn connections(&mut self, app: &AppKey) -> Result<ConnectionReport, SamplerError> {
        use tm_core::{
            Connection, ConnectionEndpoint, ConnectionProtocol as P, ConnectionState as S,
        };
        let pid = match app.as_str() {
            "/Applications/Google Chrome.app" => 1000,
            "/Applications/Docker.app" => 1001,
            "/Applications/Slack.app" => 1002,
            "/Applications/WeChat.app" => 1003,
            "/System/Library/CoreServices/mDNSResponder" => 1004,
            _ => return Ok(ConnectionReport::default()),
        };
        let connections = [
            (
                P::Tcp4,
                "192.0.2.10",
                52148,
                Some(("198.51.100.24", 443)),
                S::Established,
            ),
            (
                P::Tcp6,
                "2001:db8::10",
                52149,
                Some(("2001:db8::25", 443)),
                S::Established,
            ),
            (
                P::Tcp4,
                "127.0.0.1",
                52150,
                Some(("127.0.0.1", 7890)),
                S::Established,
            ),
            (P::Tcp4, "", 8080, None, S::Listen),
            (P::Udp4, "", 5353, None, S::Bound),
        ]
        .into_iter()
        .map(|(protocol, local, port, remote, state)| Connection {
            pid,
            protocol,
            local: ConnectionEndpoint {
                address: local.into(),
                port,
            },
            remote: remote.map(|(address, port)| ConnectionEndpoint {
                address: address.into(),
                port,
            }),
            state,
        })
        .collect();
        Ok(ConnectionReport {
            connections,
            warning: None,
            unverified_processes: 0,
        })
    }

    fn sample(&mut self) -> Result<Vec<ProcSample>, SamplerError> {
        self.tick = self.tick.wrapping_add(1);
        let t = self.tick as f64;

        // 每个应用一个不同的相位，动画看起来才自然
        let mut out = Vec::new();
        for (i, (key, base, amp)) in [
            ("/Applications/Google Chrome.app", 900_000.0, 400_000.0),
            ("/Applications/Docker.app", 260_000.0, 120_000.0),
            ("/Applications/Slack.app", 120_000.0, 90_000.0),
            ("/Applications/WeChat.app", 60_000.0, 40_000.0),
            (
                "/System/Library/CoreServices/mDNSResponder",
                20_000.0,
                15_000.0,
            ),
        ]
        .iter()
        .enumerate()
        {
            let phase = t * 0.35 + i as f64 * 1.7;
            let rx = (base + amp * phase.sin()) * self.interval.as_secs_f64();
            let tx = rx * 0.11;
            self.totals[i].0 += rx as u64;
            self.totals[i].1 += tx as u64;
            out.push(ProcSample::new(
                1000 + i as u32,
                *key,
                self.totals[i].0,
                self.totals[i].1,
            ));
        }

        std::thread::sleep(self.interval);
        Ok(out)
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    fn counters_start_at_zero(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_produces_monotonic_counters() {
        let mut s = MockSampler::new().with_interval(Duration::from_millis(1));
        let mut a = s.sample().expect("第一帧");
        for _ in 0..100 {
            let b = s.sample().expect("后续帧");
            assert_eq!(a.len(), b.len());

            // 累计值必须单调递增，否则下游会判定成计数器重置
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(x.app, y.app);
                assert!(y.rx_total >= x.rx_total);
                assert!(y.tx_total >= x.tx_total);
            }
            a = b;
        }
    }

    #[test]
    fn error_display_is_human_readable() {
        let e = SamplerError::Unsupported("ETW");
        assert!(e.to_string().contains("ETW"));
    }
}
