//! 屏幕几何：托盘图标在哪块显示器上，那块显示器的**工作区**在哪。
//!
//! 面板往哪个方向展开不能写死。Windows 任务栏通常在屏幕底部，面板必须从图标
//! **往上**长；macOS 菜单栏在顶部，方向正好相反。写死一边，另一边必然长到屏幕外。
//!
//! 工作区（`rcWork`）是已经扣掉任务栏的矩形，所以「图标中心在工作区中线以下」
//! 就等于「任务栏在底部」，不需要去查任务栏停靠位置。

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};

/// 一块矩形，单位是**物理像素**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// `(x, y)` 所在显示器的工作区。
///
/// 用 `MONITOR_DEFAULTTONEAREST` 而不是主屏尺寸：托盘挂在副屏、竖屏、
/// 或者高 DPI 屏时，按主屏算出来的坐标会整体偏出去（面板就飞到另一块屏上）。
pub fn work_area_at(x: i32, y: i32) -> Option<Rect> {
    let pt = POINT { x, y };
    let monitor = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }

    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info) };
    if ok == 0 {
        return None;
    }

    let r = info.rcWork;
    Some(Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    })
}
