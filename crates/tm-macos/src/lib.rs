//! macOS 采样实现。
//!
//! 数据源是 `nettop` 子进程（免 root），配合 `libproc` 把 pid 还原成
//! 「应用」——把 Chrome 的一堆 helper 进程合并成一行「Google Chrome」。

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
