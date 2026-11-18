//! 系统外观：任务栏是深色还是浅色。
//!
//! 托盘图标不是 macOS 的模板图：Windows 会照原样画，不会按主题反色。
//! 所以深色任务栏上必须给白线、浅色任务栏上必须给黑线，否则图标等于隐形。
//! 值来自注册表，`SystemUsesLightTheme` 才是任务栏用的那一个
//! （`AppsUseLightTheme` 只决定应用窗口的主题，Win10 之后两者可以不一致）。

use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// 任务栏是否为浅色。读不到时按深色处理 —— Win11 的默认值就是深色。
pub fn taskbar_is_light() -> bool {
    read_dword("SystemUsesLightTheme")
        .or_else(|| read_dword("AppsUseLightTheme"))
        .map(|v| v != 0)
        .unwrap_or(false)
}

fn read_dword(name: &str) -> Option<u32> {
    let subkey = to_wide(PERSONALIZE);
    let value = to_wide(name);

    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };

    // ERROR_SUCCESS == 0
    (rc == 0).then_some(data)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
