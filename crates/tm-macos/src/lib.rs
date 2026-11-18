//! macOS 采样实现。
//!
//! 数据源是 `nettop` 子进程（免 root），配合 `libproc` 把 pid 还原成
//! 「应用」——把 Chrome 的一堆 helper 进程合并成一行「Google Chrome」。

// 整个 crate 都是 macOS 专属：workspace 里成员是无条件列出的，
// Windows 上 `cargo check --workspace` 也会来编它。crate 级门控之后，
// 非 macOS 平台编出来是个空 crate，不会去碰 libc / objc2 这些 macOS-only 依赖。
#![cfg(target_os = "macos")]

pub mod appid;
pub mod icon;
pub mod nettop;
pub mod procname;
pub mod pty;
pub mod sampler;
pub mod status_item;

pub use appid::{bundle_of, identify, AppIdentity, AppResolver};
pub use icon::{app_icon, IconBitmap, ICON_SIDE};
pub use nettop::{NettopParser, ProcRow};
pub use sampler::NettopSampler;
pub use status_item::{SpeedLines, StatusItem, Tone};
