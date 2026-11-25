//! Keep the native frame consistent with our undecorated OpenGL panel.
//!
//! winit 0.30 retains WS_CAPTION on top-level undecorated windows and hides it
//! with WM_NCCALCSIZE. Some Windows OpenGL drivers still subtract that frame
//! from their drawable, shifting the image relative to the mouse coordinates.
//! See https://github.com/emilk/egui/issues/2770. This fixed-size popup needs
//! no native caption or resize frame, so remove them instead of moving input.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{GetLastError, SetLastError, HWND};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_THICKFRAME,
};

pub fn prepare(window: &impl HasWindowHandle) {
    let Ok(handle) = window.window_handle() else {
        return; // Headless egui tests have no platform window.
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    match remove_native_frame(handle.hwnd.get() as HWND) {
        Ok(true) => {
            log::info!(target: "tramoing::window", "已移除无边框面板的原生标题栏样式，统一绘制与鼠标坐标")
        }
        Ok(false) => {}
        Err(error) => log::warn!(target: "tramoing::window", "无边框窗口兼容处理失败：{error}"),
    }
}

fn remove_native_frame(hwnd: HWND) -> std::io::Result<bool> {
    // Called only on the UI thread for a window owned by this process.
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let extended = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let new_style = style & !(WS_CAPTION | WS_THICKFRAME);
        let new_extended = extended
            & !(WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE | WS_EX_DLGMODALFRAME);
        if (style, extended) == (new_style, new_extended) {
            return Ok(false);
        }
        for (index, before, after) in [
            (GWL_STYLE, style, new_style),
            (GWL_EXSTYLE, extended, new_extended),
        ] {
            if before != after {
                SetLastError(0);
                if SetWindowLongW(hwnd, index, after as i32) == 0 && GetLastError() != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        if SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetClientRect, GetWindowRect, IsWindowVisible,
        WS_CLIPSIBLINGS, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_SYSMENU,
    };

    #[test]
    fn native_frame_removal_aligns_client_origin_and_preserves_popup_behavior() {
        // Hidden test window: no taskbar changes, focus stealing or real app handles.
        unsafe {
            let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
            let style = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_CLIPSIBLINGS;
            let extended = WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
            let hwnd = CreateWindowExW(
                extended,
                class.as_ptr(),
                std::ptr::null(),
                style,
                100,
                100,
                408,
                482,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!hwnd.is_null(), "{}", std::io::Error::last_os_error());
            struct Window(HWND);
            impl Drop for Window {
                fn drop(&mut self) {
                    unsafe { DestroyWindow(self.0) };
                }
            }
            let _window = Window(hwnd);
            let mut original: RECT = std::mem::zeroed();
            assert_ne!(GetWindowRect(hwnd, &mut original), 0);
            for _ in 0..2 {
                // winit can restore styles when focus/visibility/level changes.
                SetWindowLongW(hwnd, GWL_STYLE, style as i32);
                SetWindowLongW(hwnd, GWL_EXSTYLE, extended as i32);
                assert!(remove_native_frame(hwnd).unwrap());
                assert!(!remove_native_frame(hwnd).unwrap());
                let mut outer: RECT = std::mem::zeroed();
                let mut client: RECT = std::mem::zeroed();
                let mut origin = POINT { x: 0, y: 0 };
                assert_ne!(GetWindowRect(hwnd, &mut outer), 0);
                assert_ne!(GetClientRect(hwnd, &mut client), 0);
                assert_ne!(ClientToScreen(hwnd, &mut origin), 0);
                assert_eq!(
                    (outer.left, outer.top, outer.right, outer.bottom),
                    (original.left, original.top, original.right, original.bottom)
                );
                assert_eq!((origin.x, origin.y), (outer.left, outer.top));
                assert_eq!(
                    (client.right, client.bottom),
                    (outer.right - outer.left, outer.bottom - outer.top)
                );
                assert_eq!(
                    GetWindowLongW(hwnd, GWL_STYLE) as u32,
                    WS_SYSMENU | WS_CLIPSIBLINGS
                );
                assert_eq!(
                    GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST
                );
                assert_eq!(IsWindowVisible(hwnd), 0);
            }
        }
    }
}
