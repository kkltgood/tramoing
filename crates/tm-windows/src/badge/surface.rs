//! 灰度字形覆盖率 → 预乘 BGRA，交给 DWM 与真实任务栏背景合成。
use super::{client_rect, wide, Content, Group};
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
pub(super) const WIDTH_DIP: i32 = 40;
const PERFORMANCE_WIDTH_DIP: i32 = 58;
const TEMPERATURE_WIDTH_DIP: i32 = 102;
const TEMPERATURE_GAP_DIP: i32 = 10;
// 相邻统计项在文字区域之外统一留白，不再叠加性能列的空余宽度。
const GROUP_GAP_DIP: i32 = 10;
const ARROW_LEFT_DIP: i32 = 1;
const ARROW_RIGHT_DIP: i32 = 8;
const NUMBER_LEFT_DIP: i32 = 8;
const PERFORMANCE_LABEL_RIGHT_DIP: i32 = 24;
const PERFORMANCE_NUMBER_LEFT_DIP: i32 = 27;
const RIGHT_PADDING_DIP: i32 = 1;

pub(super) fn width_px(content: &Content, dpi: i32) -> i32 {
    Widths::new(content, dpi).total(content)
}

pub(super) fn minimum_width_px(content: &Content, dpi: i32) -> i32 {
    let widths = Widths::new(content, dpi);
    content
        .visible_items()
        .map(|group| widths.group(group))
        .max()
        .unwrap_or(0)
}

struct Widths {
    speed: i32,
    performance: i32,
    gap: i32,
}

impl Widths {
    fn new(content: &Content, dpi: i32) -> Self {
        let performance = content
            .performance
            .as_ref()
            .filter(|p| p.temperatures.is_some())
            .map(|p| unsafe {
                measure_performance_width(p, dpi).unwrap_or(TEMPERATURE_WIDTH_DIP * dpi / 96)
            })
            .unwrap_or(PERFORMANCE_WIDTH_DIP * dpi / 96);
        Self {
            speed: WIDTH_DIP * dpi / 96,
            performance,
            gap: GROUP_GAP_DIP * dpi / 96,
        }
    }

    fn group(&self, group: Group) -> i32 {
        match group {
            Group::Speed => self.speed,
            Group::Performance => self.performance,
        }
    }

    fn total(&self, content: &Content) -> i32 {
        let count = content.visible_items().count();
        content
            .visible_items()
            .map(|group| self.group(group))
            .sum::<i32>()
            + count.saturating_sub(1) as i32 * self.gap
    }
}

pub(super) struct Layout {
    pub groups: Vec<(Group, RECT)>,
    stacked: bool,
}

impl Layout {
    pub fn rect(&self, group: Group) -> Option<RECT> {
        self.groups
            .iter()
            .find(|(item, _)| *item == group)
            .map(|(_, rect)| *rect)
    }

    pub fn group_at(&self, x: i32, y: i32) -> Option<Group> {
        self.groups
            .iter()
            .find(|(_, rect)| super::contains(*rect, x, y))
            .map(|(group, _)| *group)
    }

    pub fn stacked(&self) -> bool {
        self.stacked
    }
}

pub(super) fn layout(content: &Content, width: i32, height: i32, dpi: i32) -> Layout {
    let full = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    let items: Vec<_> = content.visible_items().collect();
    let count = items.len() as i32;
    let widths = Widths::new(content, dpi);
    let stacked = count > 1 && width < widths.total(content);
    // 绘制、窗口尺寸和命中区域使用相同的实际字体宽度，换序后间距保持一致。
    let item_width: i32 = items.iter().map(|item| widths.group(*item)).sum();
    let mut offset = 0;
    let groups = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let index = index as i32;
            let rect = if count == 1 {
                full
            } else if stacked {
                // Win10 竖向任务栏：按同一顺序上下排列，点击和绘制共用布局。
                RECT {
                    top: height * index / count,
                    bottom: height * (index + 1) / count,
                    ..full
                }
            } else {
                let left = offset + (width - item_width) * index / (count - 1);
                let item_width = widths.group(item);
                offset += item_width;
                RECT {
                    left,
                    right: left + item_width,
                    ..full
                }
            };
            (item, rect)
        })
        .collect();
    Layout { groups, stacked }
}

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

unsafe fn create_font(dpi: i32) -> HFONT {
    CreateFontW(
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
    )
}

unsafe fn rasterize(
    content: &Content,
    width: i32,
    height: i32,
    dpi: i32,
) -> Result<Surface, &'static str> {
    let mut surface = Surface::new(width, height)?;
    surface.pixels_mut().fill(0);
    let font = create_font(dpi);
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
    let layout = layout(content, width, height, dpi);
    let mut drawn = true;
    if let (Some(speed), Some(rect)) = (&content.speed, layout.rect(Group::Speed)) {
        drawn &= draw_lines(
            surface.dc,
            rect,
            dpi,
            [("↓", &speed.down), ("↑", &speed.up)],
            false,
        );
    }
    if let (Some(performance), Some(rect)) = (&content.performance, layout.rect(Group::Performance))
    {
        drawn &= draw_lines(
            surface.dc,
            rect,
            dpi,
            [("CPU", &performance.cpu), ("GPU", &performance.gpu)],
            true,
        );
        if let Some((cpu, gpu)) = &performance.temperatures {
            // 温度紧跟两行中较宽的使用率，避免个位数百分比后留下三位数的空位。
            let temperature_left =
                temperature_left(surface.dc, rect, dpi, [&performance.cpu, &performance.gpu]);
            drawn &= temperature_left.is_some();
            let height = rect.bottom - rect.top;
            let line = (14 * dpi / 96).min(height / 2);
            let top = rect.top + (height - line * 2) / 2;
            for (index, value) in [cpu, gpu].into_iter().enumerate() {
                let text = wide(value.trim());
                let mut area = RECT {
                    left: temperature_left.unwrap_or(rect.right),
                    right: rect.right - RIGHT_PADDING_DIP * dpi / 96,
                    top: top + index as i32 * line,
                    bottom: top + (index as i32 + 1) * line,
                };
                drawn &= DrawTextW(
                    surface.dc,
                    text.as_ptr(),
                    (text.len() - 1) as i32,
                    &mut area,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                ) != 0;
            }
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
        for (x, pixel) in row.iter_mut().enumerate() {
            let color = text_color(content, &layout, x as i32, y as i32);
            let coverage = (*pixel & 0xff) as u8;
            *pixel = premultiply(coverage, color);
        }
    }
    Ok(surface)
}

unsafe fn widest_text(dc: HDC, values: [&str; 2]) -> Option<i32> {
    let mut width = 0;
    for value in values {
        let text = wide(value.trim());
        let mut size = SIZE { cx: 0, cy: 0 };
        if GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size) == 0 {
            return None;
        }
        width = width.max(size.cx);
    }
    Some(width)
}

unsafe fn temperature_left(dc: HDC, area: RECT, dpi: i32, usage: [&str; 2]) -> Option<i32> {
    Some(
        area.left
            + PERFORMANCE_NUMBER_LEFT_DIP * dpi / 96
            + widest_text(dc, usage)?
            + TEMPERATURE_GAP_DIP * dpi / 96,
    )
}

unsafe fn measure_performance_width(performance: &super::Performance, dpi: i32) -> Option<i32> {
    let (cpu, gpu) = performance.temperatures.as_ref()?;
    let surface = Surface::new(1, 1).ok()?;
    let font = create_font(dpi);
    if font.is_null() {
        return None;
    }
    let old = SelectObject(surface.dc, font);
    if old.is_null() || old as isize == -1 {
        DeleteObject(font);
        return None;
    }
    let width = widest_text(surface.dc, [&performance.cpu, &performance.gpu])
        .zip(widest_text(surface.dc, [cpu, gpu]))
        .map(|(usage, temperature)| {
            PERFORMANCE_NUMBER_LEFT_DIP * dpi / 96
                + usage
                + TEMPERATURE_GAP_DIP * dpi / 96
                + temperature
                + RIGHT_PADDING_DIP * dpi / 96
        });
    SelectObject(surface.dc, old);
    DeleteObject(font);
    width
}

unsafe fn draw_lines(
    dc: HDC,
    area: RECT,
    dpi: i32,
    lines: [(&str, &str); 2],
    performance: bool,
) -> bool {
    let height = area.bottom - area.top;
    let line = (14 * dpi / 96).min(height / 2);
    let top = area.top + (height - line * 2) / 2;
    let mut drawn = true;
    for (i, (label, number)) in lines.into_iter().enumerate() {
        // 固定标签列；百分比靠左贴近 CPU/GPU，速率右对齐以保持单位对齐。
        for (text, left, right, align) in [
            (
                label,
                ARROW_LEFT_DIP,
                if performance {
                    PERFORMANCE_LABEL_RIGHT_DIP
                } else {
                    ARROW_RIGHT_DIP
                },
                if performance { DT_LEFT } else { DT_CENTER },
            ),
            (
                number.trim(),
                if performance {
                    PERFORMANCE_NUMBER_LEFT_DIP
                } else {
                    NUMBER_LEFT_DIP
                },
                0,
                if performance { DT_LEFT } else { DT_RIGHT },
            ),
        ] {
            let text = wide(text);
            let mut rect = RECT {
                left: area.left + left * dpi / 96,
                right: if right == 0 {
                    area.right - RIGHT_PADDING_DIP * dpi / 96
                } else {
                    area.left + right * dpi / 96
                },
                top: top + i as i32 * line,
                bottom: top + (i as i32 + 1) * line,
            };
            drawn &= DrawTextW(
                dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut rect,
                align | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            ) != 0;
        }
    }
    drawn
}

fn text_color(content: &Content, layout: &Layout, x: i32, y: i32) -> Rgb {
    let contains = |r: RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
    if let (Some(performance), Some(rect)) = (&content.performance, layout.rect(Group::Performance))
    {
        if performance.colored && contains(rect) {
            return if y < (rect.top + rect.bottom) / 2 {
                content.colors.cpu(!content.light)
            } else {
                content.colors.gpu(!content.light)
            };
        }
    }
    if let (Some(speed), Some(rect)) = (&content.speed, layout.rect(Group::Speed)) {
        if speed.colored && contains(rect) {
            return if y < (rect.top + rect.bottom) / 2 {
                content.colors.download(!content.light)
            } else {
                content.colors.upload(!content.light)
            };
        }
    }
    if content.light {
        Rgb(24, 24, 28)
    } else {
        Rgb(238, 238, 242)
    }
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
    use crate::badge::{tests::order, Performance, Speed};

    fn content(light: bool, colored: bool) -> Content {
        Content {
            speed: Some(Speed {
                down: "999M".into(),
                up: "1.5G".into(),
                colored: !colored,
            }),
            performance: Some(Performance {
                temperatures: None,
                cpu: "100%".into(),
                gpu: " --%".into(),
                colored,
            }),
            light,
            colors: Default::default(),
            order: order(false),
        }
    }

    #[test]
    fn temperature_columns_fit_and_hiding_them_restores_compact_layout() {
        for dpi in [96, 120, 144, 168, 192] {
            let mut content = content(false, true);
            let compact = width_px(&content, dpi);
            content.performance.as_mut().unwrap().temperatures =
                Some(("150°C".into(), " --°C".into()));
            let width = width_px(&content, dpi);
            assert!(width > compact);
            let height = 48 * dpi / 96;
            let layout = layout(&content, width, height, dpi);
            let area = layout.rect(Group::Performance).unwrap();
            let mut surface = unsafe { rasterize(&content, width, height, dpi).unwrap() };
            let font = unsafe { create_font(dpi) };
            let old = unsafe { SelectObject(surface.dc, font) };
            let performance = content.performance.as_ref().unwrap();
            let start = unsafe {
                temperature_left(surface.dc, area, dpi, [&performance.cpu, &performance.gpu])
                    .unwrap()
            };
            for text in ["150°C", "--°C"] {
                let text = wide(text);
                let mut size = SIZE { cx: 0, cy: 0 };
                assert_ne!(
                    unsafe {
                        GetTextExtentPoint32W(
                            surface.dc,
                            text.as_ptr(),
                            (text.len() - 1) as i32,
                            &mut size,
                        )
                    },
                    0
                );
                assert!(size.cx <= area.right - RIGHT_PADDING_DIP * dpi / 96 - start);
            }
            unsafe {
                SelectObject(surface.dc, old);
                DeleteObject(font);
            }
            for (top, bottom) in [(0, height / 2), (height / 2, height)] {
                assert!((top..bottom).any(|y| surface.pixels_mut()
                    [(y * width + start) as usize..(y * width + area.right) as usize]
                    .iter()
                    .any(|p| p >> 24 == 255)));
            }
            content.performance.as_mut().unwrap().temperatures = None;
            assert_eq!(width_px(&content, dpi), compact);
        }
    }

    #[test]
    fn compact_columns_fit_the_widest_values_at_each_dpi() {
        for dpi in [96, 120, 144, 168, 192] {
            let surface = unsafe { Surface::new(1, 1).unwrap() };
            let font = unsafe { create_font(dpi) };
            assert!(!font.is_null());
            let old = unsafe { SelectObject(surface.dc, font) };
            let texts = [
                "CPU", "GPU", "100%", "99%", "--%", "999M", "9.9M", "999B", "999K", "↓", "↑",
            ];
            let widths: Vec<_> = texts
                .iter()
                .map(|text| {
                    let text = wide(text);
                    let mut size = SIZE { cx: 0, cy: 0 };
                    assert_ne!(
                        unsafe {
                            GetTextExtentPoint32W(
                                surface.dc,
                                text.as_ptr(),
                                (text.len() - 1) as i32,
                                &mut size,
                            )
                        },
                        0
                    );
                    size.cx
                })
                .collect();
            unsafe {
                SelectObject(surface.dc, old);
                DeleteObject(font);
            }
            for (i, width) in widths.into_iter().enumerate() {
                let available = match i {
                    0 | 1 => PERFORMANCE_LABEL_RIGHT_DIP * dpi / 96 - ARROW_LEFT_DIP * dpi / 96,
                    2..=4 => {
                        PERFORMANCE_WIDTH_DIP * dpi / 96
                            - RIGHT_PADDING_DIP * dpi / 96
                            - PERFORMANCE_NUMBER_LEFT_DIP * dpi / 96
                    }
                    5..=8 => {
                        WIDTH_DIP * dpi / 96
                            - RIGHT_PADDING_DIP * dpi / 96
                            - NUMBER_LEFT_DIP * dpi / 96
                    }
                    _ => ARROW_RIGHT_DIP * dpi / 96 - ARROW_LEFT_DIP * dpi / 96,
                };
                assert!(
                    width <= available,
                    "{} at {dpi} DPI needs {width}px, available {available}px",
                    texts[i]
                );
            }
        }
    }

    #[test]
    fn configured_colors_reach_each_row_and_mono_ignores_them() {
        use tm_core::color::DisplayColors;
        for light in [false, true] {
            let mut content = content(light, true);
            content.speed.as_mut().unwrap().colored = true;
            content.colors = DisplayColors {
                download: Some(Rgb(255, 128, 0)),
                upload: Some(Rgb(0, 192, 255)),
                cpu: Some(Rgb(255, 80, 80)),
                gpu: Some(Rgb(160, 240, 32)),
            };
            let width = width_px(&content, 96);
            let mut surface = unsafe { rasterize(&content, width, 48, 96).unwrap() };
            for color in [
                content.colors.download,
                content.colors.upload,
                content.colors.cpu,
                content.colors.gpu,
            ] {
                assert!(surface
                    .pixels_mut()
                    .contains(&premultiply(255, color.unwrap())));
            }
            content.speed.as_mut().unwrap().colored = false;
            content.performance.as_mut().unwrap().colored = false;
            let mut mono = unsafe { rasterize(&content, width, 48, 96).unwrap() };
            let expected = premultiply(
                255,
                if light {
                    Rgb(24, 24, 28)
                } else {
                    Rgb(238, 238, 242)
                },
            );
            assert!(mono.pixels_mut().contains(&expected));
            assert!(mono
                .pixels_mut()
                .iter()
                .filter(|p| *p >> 24 == 255)
                .all(|p| *p == expected));
        }
    }

    #[test]
    fn performance_layout_supports_independent_switches_and_vertical_taskbars() {
        for (dpi, performance_first) in [96, 120, 144, 168, 192]
            .into_iter()
            .flat_map(|dpi| [false, true].map(|first| (dpi, first)))
        {
            let mut content = content(false, true);
            content.order = order(performance_first);
            let width = width_px(&content, dpi);
            let horizontal = layout(&content, width, 48 * dpi / 96, dpi);
            let speed = horizontal.rect(Group::Speed).unwrap();
            let perf = horizontal.rect(Group::Performance).unwrap();
            let (first, second) = if performance_first {
                (perf, speed)
            } else {
                (speed, perf)
            };
            assert_eq!(first.left, 0);
            assert!(second.left - first.right >= GROUP_GAP_DIP * dpi / 96);
            assert_eq!(second.right, width);
            assert_eq!(speed.right - speed.left, WIDTH_DIP * dpi / 96);
            assert_eq!(perf.right - perf.left, PERFORMANCE_WIDTH_DIP * dpi / 96);
            assert_eq!(horizontal.group_at(speed.left, 0), Some(Group::Speed));
            assert_eq!(horizontal.group_at(perf.left, 0), Some(Group::Performance));
            assert_eq!(horizontal.group_at(first.right, 0), None);
            let vertical = layout(&content, 80 * dpi / 96, 72 * dpi / 96, dpi);
            let speed = vertical.rect(Group::Speed).unwrap();
            let perf = vertical.rect(Group::Performance).unwrap();
            let (first, second) = if performance_first {
                (perf, speed)
            } else {
                (speed, perf)
            };
            assert_eq!(first.bottom, second.top);
            assert_eq!(vertical.group_at(0, speed.top), Some(Group::Speed));
            assert_eq!(vertical.group_at(0, perf.top), Some(Group::Performance));
            content.speed = None;
            assert_eq!(width_px(&content, dpi), PERFORMANCE_WIDTH_DIP * dpi / 96);
            let only = layout(
                &content,
                PERFORMANCE_WIDTH_DIP * dpi / 96,
                48 * dpi / 96,
                dpi,
            );
            assert!(only.rect(Group::Speed).is_none());
            assert_eq!(only.rect(Group::Performance).unwrap().left, 0);
            content.performance = None;
            assert_eq!(width_px(&content, dpi), 0);
        }
    }

    #[test]
    fn performance_raster_keeps_labels_fixed_and_colors_independent() {
        for (dpi, performance_first) in [96, 120, 144, 168, 192]
            .into_iter()
            .flat_map(|dpi| [false, true].map(|first| (dpi, first)))
        {
            for light in [false, true] {
                for colored in [false, true] {
                    let mut content = content(light, colored);
                    content.order = order(performance_first);
                    for (width, height) in [
                        (width_px(&content, dpi), 48 * dpi / 96),
                        (80 * dpi / 96, 72 * dpi / 96),
                    ] {
                        let layout = layout(&content, width, height, dpi);
                        let perf = layout.rect(Group::Performance).unwrap();
                        let mut labels = None;
                        for (cpu, gpu) in [("100%", " --%"), ("  0%", "  9%"), (" --%", "100%")] {
                            content.performance.as_mut().unwrap().cpu = cpu.into();
                            content.performance.as_mut().unwrap().gpu = gpu.into();
                            let mut surface =
                                unsafe { rasterize(&content, width, height, dpi).unwrap() };
                            let pixels = surface.pixels_mut();
                            assert_eq!(pixels[0], HIT_ALPHA << 24);
                            let label_pixels: Vec<_> = (perf.top..perf.bottom)
                                .flat_map(|y| {
                                    let start = (y * width + perf.left) as usize;
                                    pixels[start
                                        ..start + (PERFORMANCE_NUMBER_LEFT_DIP * dpi / 96) as usize]
                                        .iter()
                                        .copied()
                                })
                                .collect();
                            assert!(label_pixels.iter().any(|p| p >> 24 == 255));
                            if let Some(expected) = &labels {
                                assert_eq!(&label_pixels, expected);
                            } else {
                                labels = Some(label_pixels);
                            }
                            // 每个区块的上下两排都必须有完整不透明的字形，且配色不串列。
                            for area in [layout.rect(Group::Speed).unwrap(), perf] {
                                for (top, bottom) in [
                                    (area.top, (area.top + area.bottom) / 2),
                                    ((area.top + area.bottom) / 2, area.bottom),
                                ] {
                                    let expected = premultiply(
                                        255,
                                        text_color(&content, &layout, area.left, top),
                                    );
                                    assert!((top..bottom).any(|y| {
                                        let row = &pixels[(y * width + area.left) as usize
                                            ..(y * width + area.right) as usize];
                                        row.contains(&expected)
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn short_percentages_stay_by_labels_and_groups_have_visible_separation() {
        for dpi in [96, 120, 144, 168, 192] {
            for performance_first in [false, true] {
                for (case, down, up, cpu, gpu, temperatures) in [
                    ("reported", "118B", "144B", "  3%", "  1%", None),
                    ("reordered", "344K", " 18K", "  2%", "  5%", None),
                    ("widest", "999M", "1.5G", "100%", " 99%", None),
                    ("unavailable", "  0B", "  9B", " --%", "100%", None),
                    (
                        "temperature-reordered",
                        "2.4K",
                        "1.1K",
                        "  2%",
                        "  2%",
                        Some((" 48°C", " 41°C")),
                    ),
                    (
                        "temperature-widest",
                        "999M",
                        "1.5G",
                        "100%",
                        " 99%",
                        Some(("150°C", " --°C")),
                    ),
                    (
                        "temperature-unavailable",
                        "  0B",
                        "  9B",
                        " --%",
                        " --%",
                        Some((" --°C", " --°C")),
                    ),
                ] {
                    let mut content = content(false, true);
                    content.order = order(performance_first);
                    content.speed = Some(Speed {
                        down: down.into(),
                        up: up.into(),
                        colored: true,
                    });
                    content.performance = Some(Performance {
                        temperatures: temperatures.map(|(cpu, gpu)| (cpu.into(), gpu.into())),
                        cpu: cpu.into(),
                        gpu: gpu.into(),
                        colored: true,
                    });
                    let width = width_px(&content, dpi);
                    let height = 36 * dpi / 96;
                    let layout = layout(&content, width, height, dpi);
                    let speed = layout.rect(Group::Speed).unwrap();
                    let perf = layout.rect(Group::Performance).unwrap();
                    assert_eq!(layout.group_at(speed.left, 0), Some(Group::Speed));
                    assert_eq!(layout.group_at(perf.right - 1, 0), Some(Group::Performance));
                    let mut reversed = content.clone();
                    reversed.order = order(!performance_first);
                    assert_eq!(width_px(&reversed, dpi), width);
                    let mut surface = unsafe { rasterize(&content, width, height, dpi).unwrap() };
                    let pixels = surface.pixels_mut();
                    let bounds = |left, right, top, bottom| {
                        let occupied: Vec<_> = (left..right)
                            .filter(|x| {
                                (top..bottom)
                                    .any(|y| pixels[(y * width + x) as usize] >> 24 > HIT_ALPHA)
                            })
                            .collect();
                        (*occupied.first().unwrap(), *occupied.last().unwrap())
                    };
                    for (top, bottom) in [(0, height / 2), (height / 2, height)] {
                        let value_left = perf.left + PERFORMANCE_NUMBER_LEFT_DIP * dpi / 96;
                        let (_, label_end) = bounds(perf.left, value_left, top, bottom);
                        let (number_start, _) = bounds(value_left, perf.right, top, bottom);
                        let gap = number_start - label_end - 1;
                        assert!(
                            gap >= 3 * dpi / 96 && gap <= 7 * dpi / 96,
                            "{case} at {dpi} DPI: label/value gap {gap}px is too large or missing"
                        );
                        let (first, second) = if performance_first {
                            (perf, speed)
                        } else {
                            (speed, perf)
                        };
                        let (_, first_end) = bounds(first.left, first.right, top, bottom);
                        let (second_start, _) = bounds(second.left, second.right, top, bottom);
                        assert!(
                            second_start - first_end > GROUP_GAP_DIP * dpi / 96,
                            "{case} at {dpi} DPI: groups touch"
                        );
                    }
                    // 可选输出直接来自生产 GDI 渲染，用截图中的数值复核真实字形。
                    if let Some(dir) = std::env::var_os("TM_BADGE_REVIEW_DIR") {
                        use std::io::Write;
                        let dir = std::path::PathBuf::from(dir);
                        std::fs::create_dir_all(&dir).unwrap();
                        let mut file = std::io::BufWriter::new(
                            std::fs::File::create(dir.join(format!(
                                "{case}-{dpi}-{}.ppm",
                                if performance_first {
                                    "performance"
                                } else {
                                    "network"
                                }
                            )))
                            .unwrap(),
                        );
                        write!(file, "P6\n{width} {height}\n255\n").unwrap();
                        for &pixel in pixels.iter() {
                            let alpha = pixel.to_be_bytes()[0] as u32;
                            for (shift, background) in [(16, 26), (8, 32), (0, 44)] {
                                let c = ((pixel >> shift) & 0xff)
                                    + (background * (255 - alpha) + 127) / 255;
                                file.write_all(&[c.min(255) as u8]).unwrap();
                            }
                        }
                        file.flush().unwrap();
                    }
                    if temperatures.is_some() {
                        let (first, second) = if performance_first {
                            (perf, speed)
                        } else {
                            (speed, perf)
                        };
                        let (_, first_end) = bounds(first.left, first.right, 0, height);
                        let (second_start, _) = bounds(second.left, second.right, 0, height);
                        let gap = second_start - first_end - 1;
                        assert!(
                            gap <= (GROUP_GAP_DIP + 5) * dpi / 96,
                            "{case} at {dpi} DPI, performance first={performance_first}: visible group gap {gap}px is too wide"
                        );
                    }
                }
            }
        }
    }

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
                            speed: Some(Speed {
                                down: down.into(),
                                up: up.into(),
                                colored,
                            }),
                            performance: None,
                            light,
                            colors: Default::default(),
                            order: order(false),
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
