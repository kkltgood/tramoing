//! 灰度字形覆盖率 → 预乘 BGRA，交给 DWM 与真实任务栏背景合成。
use super::{client_rect, wide, Content};
use std::ptr::{null, null_mut};
use tm_core::color::Rgb;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

// alpha=0 会让 Windows 把空白区域的点击交给任务栏。保留最低的 1/255
// 覆盖率，背景与真实任务栏最多相差一个色阶，整块控件仍接收原生鼠标消息。
const HIT_ALPHA: u32 = 1;

// 12px 字号下保留最宽四字符（如 999M）的空间，收紧箭头列及左右留白。
pub(super) const WIDTH_DIP: i32 = 44;
const ARROW_LEFT_DIP: i32 = 2;
const ARROW_RIGHT_DIP: i32 = 10;
const NUMBER_LEFT_DIP: i32 = 11;
const RIGHT_PADDING_DIP: i32 = 2;

pub(super) unsafe fn present(hwnd: HWND, content: &Content) -> Result<(), &'static str> {
    let rect = client_rect(hwnd).ok_or("无法读取网速控件尺寸")?;
    let size = SIZE {
        cx: rect.right,
        cy: rect.bottom,
    };
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let surface = rasterize(content, size.cx, size.cy, dpi)?;
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // 位置已由父任务栏客户区坐标设置，这里不重复传屏幕坐标。
    if UpdateLayeredWindow(
        hwnd,
        null_mut(),
        null(),
        &size,
        surface.dc,
        &POINT { x: 0, y: 0 },
        0,
        &blend,
        ULW_ALPHA,
    ) == 0
    {
        return Err("当前系统无法合成透明任务栏网速");
    }
    Ok(())
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u32,
    len: usize,
}

impl Surface {
    unsafe fn new(width: i32, height: i32) -> Result<Self, &'static str> {
        // 正常尺寸仅几十像素；拒绝异常尺寸，避免 GDI 分配过大的位图。
        if width <= 0 || height <= 0 || width > 4096 || height > 4096 {
            return Err("任务栏网速位图尺寸无效");
        }
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return Err("无法创建网速绘图上下文");
        }
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(dc);
            return Err("无法创建网速透明位图");
        }
        let original = SelectObject(dc, bitmap);
        if original.is_null() || original as isize == -1 {
            DeleteObject(bitmap);
            DeleteDC(dc);
            return Err("无法选取网速透明位图");
        }
        Ok(Self {
            dc,
            bitmap,
            original,
            bits: bits.cast(),
            len: width as usize * height as usize,
        })
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        // 32bpp 行宽不需要额外填充；DIB 在 Self 存活期间拥有整个缓冲。
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.original);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

unsafe fn rasterize(
    content: &Content,
    width: i32,
    height: i32,
    dpi: i32,
) -> Result<Surface, &'static str> {
    let mut surface = Surface::new(width, height)?;
    surface.pixels_mut().fill(0);
    let font = CreateFontW(
        -12 * dpi / 96,
        0,
        0,
        0,
        500,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        // ClearType 的子像素颜色不能作为 alpha，会在透明背景上留下彩边。
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        wide("Segoe UI").as_ptr(),
    );
    if font.is_null() {
        return Err("无法创建网速字体");
    }
    let old = SelectObject(surface.dc, font);
    if old.is_null() || old as isize == -1 {
        DeleteObject(font);
        return Err("无法选取网速字体");
    }
    SetBkMode(surface.dc, TRANSPARENT as i32);
    SetTextColor(surface.dc, 0x00ff_ffff);
    let line = (14 * dpi / 96).min(height / 2);
    let top = (height - line * 2) / 2;
    let mut drawn = true;
    for (i, (arrow, number)) in [("↓", &content.down), ("↑", &content.up)]
        .iter()
        .enumerate()
    {
        // 箭头的列宽和位置独立于数字；数字单独右对齐，单位切换也不会挤动箭头。
        for (text, left, right, align) in [
            (
                *arrow,
                ARROW_LEFT_DIP * dpi / 96,
                ARROW_RIGHT_DIP * dpi / 96,
                DT_CENTER,
            ),
            (
                number.trim(),
                NUMBER_LEFT_DIP * dpi / 96,
                width - RIGHT_PADDING_DIP * dpi / 96,
                DT_RIGHT,
            ),
        ] {
            let text = wide(text);
            let mut rect = RECT {
                left,
                right,
                top: top + i as i32 * line,
                bottom: top + (i as i32 + 1) * line,
            };
            drawn &= DrawTextW(
                surface.dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut rect,
                align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            ) != 0;
        }
    }
    SelectObject(surface.dc, old);
    DeleteObject(font);
    // 直接读取 DIB 前必须提交 GDI 批处理。
    let flushed = GdiFlush() != 0;
    if !drawn || !flushed {
        return Err("无法绘制任务栏网速文字");
    }
    for (y, row) in surface
        .pixels_mut()
        .chunks_exact_mut(width as usize)
        .enumerate()
    {
        let color = if content.colored {
            if y < height as usize / 2 {
                tm_core::color::download(!content.light)
            } else {
                tm_core::color::upload(!content.light)
            }
        } else if content.light {
            Rgb(24, 24, 28)
        } else {
            Rgb(238, 238, 242)
        };
        for pixel in row {
            let coverage = (*pixel & 0xff) as u8;
            *pixel = premultiply(coverage, color);
        }
    }
    Ok(surface)
}

fn premultiply(coverage: u8, color: Rgb) -> u32 {
    if coverage == 0 {
        return HIT_ALPHA << 24;
    }
    let alpha = coverage as u32;
    let channel = |c: u8| (c as u32 * alpha + 127) / 255;
    (alpha << 24) | (channel(color.0) << 16) | (channel(color.1) << 8) | channel(color.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_raster_keeps_arrows_fixed_across_rates_and_dpi() {
        for dpi in [96, 120, 144, 192] {
            for light in [false, true] {
                for colored in [false, true] {
                    let width = WIDTH_DIP * dpi / 96;
                    let height = 48 * dpi / 96;
                    let mut baseline = None;
                    for (down, up) in [("  0B", "  9B"), ("1.6K", "875B"), ("999M", "1.5G")] {
                        let content = Content {
                            down: down.into(),
                            up: up.into(),
                            light,
                            colored,
                        };
                        let mut surface =
                            unsafe { rasterize(&content, width, height, dpi).unwrap() };
                        let pixels = surface.pixels_mut();
                        // 背景仅有原生命中测试所需的最小 alpha，文字保留抗锯齿覆盖率。
                        assert_eq!(pixels[0], HIT_ALPHA << 24);
                        assert!(
                            pixels.iter().filter(|p| **p == HIT_ALPHA << 24).count()
                                > pixels.len() / 2
                        );
                        assert!(pixels.iter().any(|p| p >> 24 == 255));
                        assert!(pixels.iter().any(|p| (2..255).contains(&(p >> 24))));
                        assert!(pixels.iter().all(|p| [
                            p & 0xff,
                            (p >> 8) & 0xff,
                            (p >> 16) & 0xff
                        ]
                        .iter()
                        .all(|c| *c <= p >> 24)));
                        let arrows: Vec<_> = pixels
                            .chunks_exact(width as usize)
                            .flat_map(|row| {
                                row[..(NUMBER_LEFT_DIP * dpi / 96) as usize].iter().copied()
                            })
                            .collect();
                        assert!(arrows.iter().any(|p| p >> 24 > HIT_ALPHA));
                        if let Some(expected) = &baseline {
                            assert_eq!(&arrows, expected);
                        } else {
                            baseline = Some(arrows);
                        }
                    }
                }
            }
        }
    }
}
