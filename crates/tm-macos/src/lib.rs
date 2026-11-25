//! macOS 采样实现。
//!
//! 数据源是 `nettop` 子进程的 CSV 输出（免 root）。按**连接**解析而不是进程汇总：
//! 走系统代理时同一份流量会出现在代理和客户端的多个 socket 上，只有按连接才能
//! 既算对总量、又把流量归到发起方。pid → 应用由 `libproc` 完成，Chrome 的一堆
//! helper 会合并成一行「Google Chrome」。

// 连接解析和记账可在所有平台测试；系统 API 模块分别门控。

#[cfg(target_os = "macos")]
pub mod appid;
#[cfg(target_os = "macos")]
pub mod dialog;
pub mod flows;
#[cfg(target_os = "macos")]
pub mod icon;
pub mod installation;
#[cfg(target_os = "macos")]
pub mod procname;
#[cfg(target_os = "macos")]
pub mod pty;
#[cfg(target_os = "macos")]
pub mod sampler;
#[cfg(target_os = "macos")]
pub mod status_item;
#[cfg(target_os = "macos")]
pub mod temperature;
#[cfg(target_os = "macos")]
pub mod utilization;

#[cfg(target_os = "macos")]
pub use appid::{bundle_of, identify, AppIdentity, AppResolver};
pub use flows::{Bytes, FlowAccounting, FlowParser, FlowRow};
#[cfg(target_os = "macos")]
pub use icon::{app_icon, IconBitmap, ICON_SIDE};
#[cfg(target_os = "macos")]
pub use sampler::NettopSampler;
#[cfg(target_os = "macos")]
pub use status_item::{SpeedLines, StatusItem, Tone};
