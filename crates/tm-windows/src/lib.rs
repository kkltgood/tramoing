//! Windows 采样实现。
//!
//! 结构上和 macOS 那套最大的区别：**这里必须提权**。
//! Windows 没有普通权限的「按进程字节数」API，只能开 ETW 内核网络会话。
//! 所以拆成两个进程：
//!
//! ```text
//!   tramoing-helper（管理员，登录时自启）
//!       └─ ETW 实时会话 → Aggregator → 每秒一帧 → 命名管道
//!                                                     │
//!   tramoing（普通权限，托盘）  ←───────────────┘
//! ```
//!
//! 界面进程永远不需要提权，装一次就好。
//!
//! [`proto`]、[`agg`] 和 [`network`] 的纯逻辑测试可在两种平台执行，
//! 系统调用部分只在 `cfg(windows)` 下编译。

pub mod agg;
pub mod network;
pub mod network_policy;
pub mod proto;
pub mod temperature_protocol;
#[cfg(any(windows, test))]
mod temperature_sources;

#[cfg(windows)]
pub mod connections;
#[cfg(windows)]
pub mod firewall;
#[cfg(windows)]
pub mod network_control;

#[cfg(windows)]
pub mod badge;
#[cfg(windows)]
pub mod etw;
#[cfg(windows)]
pub mod icon;
#[cfg(windows)]
pub mod instance;
#[cfg(windows)]
pub mod launch;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod proc;
#[cfg(windows)]
pub mod registry_run;
#[cfg(windows)]
pub mod sampler;
#[cfg(windows)]
pub mod scheduled_task;
#[cfg(windows)]
pub mod screen;
#[cfg(windows)]
pub mod temperature;
#[cfg(windows)]
pub mod temperature_service;
#[cfg(windows)]
pub mod theme;
#[cfg(windows)]
pub mod utilization;

#[cfg(windows)]
pub use sampler::WindowsSampler;

/// 命名管道名字。两端必须一致。
pub const PIPE_NAME: &str = r"\\.\pipe\tramoing-v1";

/// ETW 会话名。同一时间只能有一个同名会话，重名会启动失败。
pub const SESSION_NAME: &str = "Tramoing-Etw";
