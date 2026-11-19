//! macOS 采样实现。
//!
//! 数据源是 `nettop` 子进程的 CSV 输出（免 root）。按**连接**解析而不是进程汇总：
//! 走系统代理时同一份流量会出现在代理和客户端的多个 socket 上，只有按连接才能
//! 既算对总量、又把流量归到发起方。pid → 应用由 `libproc` 完成，Chrome 的一堆
//! helper 会合并成一行「Google Chrome」。

// 整个 crate 都是 macOS 专属：workspace 里成员是无条件列出的，
// Windows 上 `cargo check --workspace` 也会来编它。crate 级门控之后，
// 非 macOS 平台编出来是个空 crate，不会去碰 libc / objc2 这些 macOS-only 依赖。
#![cfg(target_os = "macos")]

pub mod appid;
pub mod flows;
pub mod icon;
pub mod procname;
pub mod pty;
pub mod sampler;
pub mod status_item;

pub use appid::{bundle_of, identify, AppIdentity, AppResolver};
pub use flows::{Bytes, FlowAccounting, FlowParser, FlowRow};
pub use icon::{app_icon, IconBitmap, ICON_SIDE};
pub use sampler::NettopSampler;
pub use status_item::{SpeedLines, StatusItem, Tone};
