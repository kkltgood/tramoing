//! 从可执行文件路径取系统图标，转成 RGBA 位图。
//!
//! 界面线程不能直接调 `SHGetFileInfoW`：冷启动一次要读文件、解资源段，
//! 几十毫秒起步，出现在弹窗路径上就是明显的白帧。所以这里只提供同步实现，
//! 由 `tray-app::icons` 的加载线程调用。
//!
//! 取不到就返回 `None`（路径不存在、进程已退出、图标资源损坏），
//! 上层退回首字母色块 —— 图标只是装饰，不能因为取不到就让列表崩掉。

use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ, RGBQUAD,
};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// 解码后的图标位图：RGBA8，行优先，非预乘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconBitmap {
    pub width: u32,
    pub height: u32,
    /// 长度恒为 `width * height * 4`
    pub rgba: Vec<u8>,
}

/// 取一个可执行文件的图标（大图标，通常 32×32）。
///
/// 传进来的必须是真实存在的路径；`pid 1234` 这种占位名一律返回 `None`。
pub fn app_icon(path: &str) -> Option<IconBitmap> {
    if path.is_empty() || path.starts_with("pid ") {
        return None;
    }

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    // SAFETY: 路径缓冲以 NUL 结尾；SHFILEINFOW 先清零再交给系统填写；
    // 拿到的 hIcon 由我们负责 DestroyIcon。
    unsafe {
        let mut info: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || info.hIcon.is_null() {
            return None;
        }
        let bmp = icon_to_bitmap(info.hIcon);
        DestroyIcon(info.hIcon);
        bmp
    }
}

/// HICON → RGBA。调用方负责 `DestroyIcon`。
unsafe fn icon_to_bitmap(hicon: HICON) -> Option<IconBitmap> {
    let mut info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(hicon, &mut info) == 0 {
        return None;
    }

    let out = decode(&info);

    // GetIconInfo 会给两个位图的所有权，必须自己删，否则每取一个图标漏两块 GDI 对象
    if !info.hbmColor.is_null() {
        DeleteObject(info.hbmColor as HGDIOBJ);
    }
    if !info.hbmMask.is_null() {
        DeleteObject(info.hbmMask as HGDIOBJ);
    }
    out
}

unsafe fn decode(info: &ICONINFO) -> Option<IconBitmap> {
    if info.hbmColor.is_null() {
        // 纯单色图标（老式 16×16）：没有彩色位图，交给上层退化成色块
        return None;
    }

    let mut bm: BITMAP = std::mem::zeroed();
    if GetObjectW(
        info.hbmColor as HGDIOBJ,
        size_of::<BITMAP>() as i32,
        &mut bm as *mut BITMAP as *mut c_void,
    ) == 0
    {
        return None;
    }
    let width = bm.bmWidth;
    let height = bm.bmHeight;
    if width <= 0 || height == 0 || width > 1024 || height.unsigned_abs() > 1024 {
        return None;
    }
    let (width, height) = (width as u32, height.unsigned_abs());

    let mut pixels = read_32bpp(info.hbmColor, width, height)?;

    // 现代图标（Vista 之后）自带 alpha；老 DDB 图标 alpha 全 0，要用 AND mask 补
    let has_alpha = pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    if has_alpha {
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2); // BGRA → RGBA
            let a = px[3] as u32;
            // Windows 的 32bpp 图标是预乘 alpha，egui 要非预乘
            if a > 0 && a < 255 {
                for c in &mut px[..3] {
                    *c = (((*c as u32) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
    } else {
        // AND mask：bit=1 表示「透出背景」，即透明
        let mask = read_mask(info.hbmMask, width, height)?;
        for (i, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.swap(0, 2);
            px[3] = if mask[i] { 0 } else { 255 };
        }
    }

    Some(IconBitmap {
        width,
        height,
        rgba: pixels,
    })
}

/// 把 DDB 位图读成 32bpp 的 BGRA 缓冲（行优先、自上而下）。
unsafe fn read_32bpp(bmp: HANDLE, width: u32, height: u32) -> Option<Vec<u8>> {
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = width as i32;
    // 负高度 = 自上而下，省得读出来再翻转
    bi.bmiHeader.biHeight = -(height as i32);
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    let hdc = GetDC(std::ptr::null_mut());
    if hdc.is_null() {
        return None;
    }
    let lines = GetDIBits(
        hdc,
        bmp,
        0,
        height,
        buf.as_mut_ptr() as *mut c_void,
        &mut bi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(std::ptr::null_mut(), hdc);
    if lines == 0 {
        return None;
    }
    Some(buf)
}

/// 读 1bpp 的 AND mask，返回「该像素透明」的布尔表。
unsafe fn read_mask(bmp: HANDLE, width: u32, height: u32) -> Option<Vec<bool>> {
    if bmp.is_null() {
        return None;
    }
    let stride = (width as usize).div_ceil(32) * 4;
    // GetDIBits 总会写回两种颜色；BITMAPINFO 的柔性数组占位仅有一项。
    #[repr(C)]
    struct MonoBitmapInfo {
        header: BITMAPINFOHEADER,
        colors: [RGBQUAD; 2],
    }
    let mut bi: MonoBitmapInfo = std::mem::zeroed();
    bi.header.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.header.biWidth = width as i32;
    bi.header.biHeight = -(height as i32);
    bi.header.biPlanes = 1;
    bi.header.biBitCount = 1;
    bi.header.biCompression = BI_RGB;

    let mut buf = vec![0u8; stride * height as usize];
    let hdc = GetDC(std::ptr::null_mut());
    if hdc.is_null() {
        return None;
    }
    let lines = GetDIBits(
        hdc,
        bmp,
        0,
        height,
        buf.as_mut_ptr() as *mut c_void,
        &mut bi as *mut MonoBitmapInfo as *mut BITMAPINFO,
        DIB_RGB_COLORS,
    );
    ReleaseDC(std::ptr::null_mut(), hdc);
    if lines == 0 {
        return None;
    }

    let mut out = vec![false; (width as usize) * (height as usize)];
    for y in 0..height as usize {
        let row = y * stride;
        for x in 0..width as usize {
            let byte = buf[row + x / 8];
            out[y * width as usize + x] = (byte >> (7 - (x % 8))) & 1 == 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_monochrome_mask_with_two_color_palette() {
        use windows_sys::Win32::Graphics::Gdi::CreateBitmap;
        unsafe {
            let bits = [0x80u8, 0]; // DDB scanline is WORD aligned
            let bitmap = CreateBitmap(8, 1, 1, 1, bits.as_ptr().cast());
            assert!(!bitmap.is_null());
            let result = read_mask(bitmap, 8, 1);
            DeleteObject(bitmap);
            assert_eq!(
                result.unwrap(),
                vec![true, false, false, false, false, false, false, false]
            );
        }
    }

    #[test]
    fn placeholder_pid_has_no_icon() {
        assert!(app_icon("pid 1234").is_none());
        assert!(app_icon("").is_none());
    }

    #[test]
    fn missing_file_has_no_icon() {
        assert!(app_icon(r"C:\definitely\not\here\nothing.exe").is_none());
    }

    #[test]
    fn decodes_system_icon_to_rgba() {
        // 随便挑一个系统里一定有的 exe
        let path = r"C:\Windows\System32\notepad.exe";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let bmp = app_icon(path).expect("notepad 应该有图标");
        assert!(bmp.width >= 16 && bmp.height >= 16, "尺寸异常：{bmp:?}");
        assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);
        assert!(
            bmp.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0),
            "整张图全透明，说明 alpha 没解出来"
        );
    }
}
