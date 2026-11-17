//! `tm-core` —— 平台无关的流量聚合核心。
//!
//! 这一层不碰任何系统 API，只做：绝对值差分 → 应用归并 → 平滑 → 排序。
//! 好处是整个速率算法可以在没有网络、没有权限的机器上跑单测，
//! 采集层（nettop / ETW）只要负责吐 [`ProcSample`] 就行。

pub mod rate;
pub mod types;
pub mod window;

pub use rate::RateCalculator;
pub use window::{WindowTracker, MAX_SLOTS};
pub use types::{format_bytes, format_speed, pretty_name, AppKey, AppRate, ProcSample};
