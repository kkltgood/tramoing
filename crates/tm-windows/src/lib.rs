//! Windows 采样实现。
//!
//! 结构上和 macOS 那套最大的区别：**这里必须提权**。
//! Windows 没有普通权限的「按进程字节数」API，只能开 ETW 内核网络会话。
//! 所以拆成两个进程：
//!
//! ```text
//!   tm-helper（管理员，登录时自启）
//!       └─ ETW 实时会话 → Aggregator → 每秒一帧 → 命名管道
//!                                                     │
//!   traffic-monitor（普通权限，托盘）  ←───────────────┘
//! ```
//!
//! 界面进程永远不需要提权，装一次就好。
//!
//! 能在这里跑单测的只有 [`proto`] 和 [`agg`]（纯逻辑），
//! 系统调用部分只在 `cfg(windows)` 下编译。

pub mod agg;
pub mod proto;

#[cfg(windows)]
pub mod etw;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod proc;
#[cfg(windows)]
pub mod sampler;
#[cfg(windows)]
pub mod screen;
#[cfg(windows)]
pub mod theme;

#[cfg(windows)]
pub use sampler::WindowsSampler;

/// 命名管道名字。两端必须一致。
pub const PIPE_NAME: &str = r"\\.\pipe\traffic-monitor-v1";

/// ETW 会话名。同一时间只能有一个同名会话，重名会启动失败。
pub const SESSION_NAME: &str = "TrafficMonitor-Etw";
