//! `tm-core` —— 平台无关的流量聚合核心。
//!
//! 这一层不碰任何系统 API，只做：绝对值差分 → 应用归并 → 平滑 → 排序。
//! 好处是整个速率算法可以在没有网络、没有权限的机器上跑单测，
//! 采集层（nettop / ETW）只要负责吐 [`ProcSample`] 就行。

pub mod color;
pub mod identity;
pub mod rate;
pub mod taskbar;
pub mod temperature;
pub mod types;
pub mod window;

pub use color::Rgb;
pub use rate::RateCalculator;
pub use types::{
    format_bytes, format_speed, is_system_service, pretty_name, AppKey, AppRate, ProcSample,
};
pub use window::{WindowTracker, MAX_SLOTS};
