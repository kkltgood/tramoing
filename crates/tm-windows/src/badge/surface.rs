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

// Keep the network number column stable and measure the unit at its actual size.
const SPEED_UNIT_LEFT_DIP: i32 = 34;
const GROUP_GAP_DIP: i32 = 6;
const DETAIL_GAP_DIP: i32 = 3;
const ARROW_LEFT_DIP: i32 = 1;
const ARROW_RIGHT_DIP: i32 = 9;
const NUMBER_LEFT_DIP: i32 = 12;
const RIGHT_PADDING_DIP: i32 = 1;

fn content_gap_px(dpi: i32) -> i32 {
    // Include the padding on both sides of the inter-group gap.
    GROUP_GAP_DIP * dpi / 96 + 2 * (RIGHT_PADDING_DIP * dpi / 96)
}

fn speed_columns(content: &Content, dpi: i32) -> (i32, [i32; 2]) {
    let units = content.speed.as_ref().and_then(|speed| unsafe {
        let surface = Surface::new(1, 1).ok()?;
        let fonts = Fonts::new(dpi).ok()?;
        let down = measure_text(surface.dc, &fonts, speed_parts(&speed.down).1, Face::Label)?;
        let up = measure_text(surface.dc, &fonts, speed_parts(&speed.up).1, Face::Label)?;
        Some([down, up])
    });
    let units = units.unwrap_or([12 * dpi / 96; 2]);
    (
        SPEED_UNIT_LEFT_DIP * dpi / 96 + units[0].max(units[1]) + RIGHT_PADDING_DIP * dpi / 96,
        units,
    )
}

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
    speed_units: [i32; 2],
    performance: PerformanceColumns,
    gap: i32,
}

impl Widths {
    fn new(content: &Content, dpi: i32) -> Self {
        let (speed, speed_units) = speed_columns(content, dpi);
        Self {
            speed,
            speed_units,
            performance: PerformanceColumns::new(content, dpi),
            gap: GROUP_GAP_DIP * dpi / 96,
        }
    }

    fn group(&self, group: Group) -> i32 {
        match group {
            Group::Speed => self.speed,
            Group::Performance => self.performance.width,
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
    performance: PerformanceColumns,
    speed_unit_right: i32,
    speed_units: [i32; 2],
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
    // 绘制、窗口尺寸和命中区域共用实测列宽；容器有余量时也不拉大组间距。
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
                let left = offset;
                let item_width = widths.group(item);
                offset += item_width + widths.gap;
                RECT {
                    left,
                    right: left + item_width,
                    ..full
                }
            };
            (item, rect)
        })
        .collect();
    Layout {
        groups,
        stacked,
        performance: widths.performance,
        speed_unit_right: widths.speed - RIGHT_PADDING_DIP * dpi / 96,
        speed_units: widths.speed_units,
    }
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

#[derive(Clone, Copy)]
enum Face {
    Value,
    Label,
    Temperature,
}

unsafe fn create_font(dpi: i32) -> HFONT {
    CreateFontW(
        -12 * dpi / 96,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        // ClearType subpixel colors cannot be used as transparent glyph coverage.
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        wide("Segoe UI").as_ptr(),
    )
}

struct Fonts(HFONT);

impl Fonts {
    unsafe fn new(dpi: i32) -> Result<Self, &'static str> {
        let fonts = Self(create_font(dpi));
        if fonts.0.is_null() {
            return Err("无法创建任务栏字体");
        }
        Ok(fonts)
    }

    fn get(&self, _face: Face) -> HFONT {
        self.0
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

// Share the wider of the two current rows, rather than reserving 100% / 150°C
// on every refresh. This keeps CPU/GPU aligned without large empty columns.
struct PerformanceColumns {
    label_right: i32,
    number_left: i32,
    number_right: i32,
    unit_left: i32,
    unit_right: i32,
    temperature_left: i32,
    width: i32,
}

impl PerformanceColumns {
    fn new(content: &Content, dpi: i32) -> Self {
        let px = |dip| dip * dpi / 96;
        let sizes = content.performance.as_ref().and_then(|performance| unsafe {
            let surface = Surface::new(1, 1).ok()?;
            let fonts = Fonts::new(dpi).ok()?;
            let measure = |text, face| measure_text(surface.dc, &fonts, text, face);
            let label = measure("CPU", Face::Label)?.max(measure("GPU", Face::Label)?);
            let number = measure(usage_parts(&performance.cpu).0, Face::Value)?
                .max(measure(usage_parts(&performance.gpu).0, Face::Value)?);
            let unit = measure("%", Face::Label)?;
            let temperature = match &performance.temperatures {
                Some((cpu, gpu)) => measure(cpu.trim(), Face::Temperature)?
                    .max(measure(gpu.trim(), Face::Temperature)?),
                None => 0,
            };
            Some((label, number, unit, temperature))
        });
        // A failed measurement still leaves conservative room for every valid value.
        let (label, number, unit, temperature) = sizes.unwrap_or_else(|| {
            (
                px(26),
                px(24),
                px(12),
                if content
                    .performance
                    .as_ref()
                    .is_some_and(|p| p.temperatures.is_some())
                {
                    px(40)
                } else {
                    0
                },
            )
        });
        let label_right = px(1) + label;
        let number_left = label_right + px(DETAIL_GAP_DIP);
        let number_right = number_left + number;
        let unit_left = number_right + px(1);
        let unit_right = unit_left + unit;
        let temperature_left = unit_right + content_gap_px(dpi);
        Self {
            label_right,
            number_left,
            number_right,
            unit_left,
            unit_right,
            temperature_left,
            width: if temperature > 0 {
                temperature_left + temperature
            } else {
                unit_right
            } + px(RIGHT_PADDING_DIP),
        }
    }
}

unsafe fn measure_text(dc: HDC, fonts: &Fonts, text: &str, face: Face) -> Option<i32> {
    let old = SelectObject(dc, fonts.get(face));
    if old.is_null() || old as isize == -1 {
        return None;
    }
    let text = wide(text);
    let mut size = SIZE { cx: 0, cy: 0 };
    let measured = GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
    SelectObject(dc, old);
    (measured != 0).then_some(size.cx)
}

struct TextRun<'a> {
    text: &'a str,
    area: RECT,
    face: Face,
    color: Rgb,
    right: bool,
}

fn foreground(light: bool) -> Rgb {
    if light {
        Rgb(24, 24, 28)
    } else {
        Rgb(238, 238, 242)
    }
}

fn row_color(content: &Content, group: Group, row: usize) -> Rgb {
    let dark = !content.light;
    match group {
        Group::Speed if content.speed.as_ref().is_some_and(|s| s.colored) => {
            if row == 0 {
                content.colors.download(dark)
            } else {
                content.colors.upload(dark)
            }
        }
        Group::Performance if content.performance.as_ref().is_some_and(|p| p.colored) => {
            if row == 0 {
                content.colors.cpu(dark)
            } else {
                content.colors.gpu(dark)
            }
        }
        _ => foreground(content.light),
    }
}

fn rows(area: RECT, dpi: i32) -> [RECT; 2] {
    let height = area.bottom - area.top;
    let line = (18 * dpi / 96).min(height / 2);
    let top = area.top + (height - 2 * line) / 2;
    [0, 1].map(|row| RECT {
        top: top + row * line,
        bottom: top + (row + 1) * line,
        ..area
    })
}

fn column(area: RECT, left: i32, right: i32, dpi: i32) -> RECT {
    RECT {
        left: (area.left + left * dpi / 96).min(area.right),
        right: if right == 0 {
            area.right - RIGHT_PADDING_DIP * dpi / 96
        } else {
            (area.left + right * dpi / 96).min(area.right)
        },
        ..area
    }
}

fn speed_parts(value: &str) -> (&str, &str) {
    let value = value.trim();
    match value.as_bytes().last() {
        Some(b'B' | b'K' | b'M' | b'G' | b'T') => value.split_at(value.len() - 1),
        _ => (value, ""),
    }
}

fn usage_parts(value: &str) -> (&str, &str) {
    let value = value.trim();
    value
        .strip_suffix('%')
        .map_or((value, ""), |number| (number, "%"))
}

fn runs<'a>(content: &'a Content, layout: &Layout, dpi: i32) -> Vec<TextRun<'a>> {
    let mut runs = Vec::new();
    if let (Some(speed), Some(area)) = (&content.speed, layout.rect(Group::Speed)) {
        for (row, (area, value)) in rows(area, dpi)
            .into_iter()
            .zip([&speed.down, &speed.up])
            .enumerate()
        {
            let (number, unit) = speed_parts(value);
            // Right-align the complete reading, keeping its number and unit together.
            let unit_left = area.left + layout.speed_unit_right - layout.speed_units[row];
            runs.push(TextRun {
                text: number,
                area: RECT {
                    right: unit_left.min(area.right),
                    ..column(area, NUMBER_LEFT_DIP, 0, dpi)
                },
                face: Face::Value,
                color: row_color(content, Group::Speed, row),
                right: true,
            });
            runs.push(TextRun {
                text: unit,
                area: RECT {
                    left: unit_left.min(area.right),
                    right: (area.left + layout.speed_unit_right).min(area.right),
                    ..area
                },
                face: Face::Label,
                color: row_color(content, Group::Speed, row),
                right: true,
            });
        }
    }
    if let (Some(performance), Some(area)) = (&content.performance, layout.rect(Group::Performance))
    {
        let columns = &layout.performance;
        let column_px = |area: RECT, left: i32, right: i32| RECT {
            left: (area.left + left).min(area.right),
            right: (area.left + right).min(area.right),
            ..area
        };
        for (row, (area, (label, usage))) in rows(area, dpi)
            .into_iter()
            .zip([("CPU", &performance.cpu), ("GPU", &performance.gpu)])
            .enumerate()
        {
            runs.push(TextRun {
                text: label,
                area: column_px(area, dpi / 96, columns.label_right),
                face: Face::Label,
                color: row_color(content, Group::Performance, row),
                right: false,
            });
            let (number, unit) = usage_parts(usage);
            runs.push(TextRun {
                text: number,
                area: column_px(area, columns.number_left, columns.number_right),
                face: Face::Value,
                color: row_color(content, Group::Performance, row),
                right: true,
            });
            runs.push(TextRun {
                text: unit,
                area: column_px(area, columns.unit_left, columns.unit_right),
                face: Face::Label,
                color: row_color(content, Group::Performance, row),
                right: false,
            });
            if let Some((cpu, gpu)) = &performance.temperatures {
                runs.push(TextRun {
                    text: if row == 0 { cpu.trim() } else { gpu.trim() },
                    area: column_px(area, columns.temperature_left, columns.width - dpi / 96),
                    face: Face::Temperature,
                    color: row_color(content, Group::Performance, row),
                    right: false,
                });
            }
        }
    }
    runs
}

unsafe fn draw_text(dc: HDC, fonts: &Fonts, run: &TextRun<'_>) -> bool {
    if run.text.is_empty() {
        return true;
    }
    let old = SelectObject(dc, fonts.get(run.face));
    if old.is_null() || old as isize == -1 {
        return false;
    }
    let mut area = run.area;
    let text = wide(run.text);
    let ok = DrawTextW(
        dc,
        text.as_ptr(),
        (text.len() - 1) as i32,
        &mut area,
        if run.right { DT_RIGHT } else { DT_LEFT } | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    ) != 0;
    SelectObject(dc, old);
    ok
}

unsafe fn draw_arrow(dc: HDC, area: RECT, dpi: i32, down: bool) -> bool {
    let pen = CreatePen(PS_SOLID, (dpi / 96).max(1), 0x00ff_ffff);
    if pen.is_null() {
        return false;
    }
    let old = SelectObject(dc, pen);
    if old.is_null() || old as isize == -1 {
        DeleteObject(pen);
        return false;
    }
    let x = (area.left + area.right) / 2;
    let y = (area.top + area.bottom) / 2;
    let radius = 3 * dpi / 96;
    let direction = if down { 1 } else { -1 };
    let tip = y + direction * radius;
    let mut ok = MoveToEx(dc, x, y - direction * radius, null_mut()) != 0;
    ok &= LineTo(dc, x, tip) != 0;
    ok &= MoveToEx(dc, x - radius, y, null_mut()) != 0;
    ok &= LineTo(dc, x, tip) != 0;
    ok &= LineTo(dc, x + radius, y) != 0;
    SelectObject(dc, old);
    DeleteObject(pen);
    ok
}

unsafe fn rasterize(
    content: &Content,
    width: i32,
    height: i32,
    dpi: i32,
) -> Result<Surface, &'static str> {
    let mut surface = Surface::new(width, height)?;
    surface.pixels_mut().fill(0);
    let fonts = Fonts::new(dpi)?;
    SetBkMode(surface.dc, TRANSPARENT as i32);
    SetTextColor(surface.dc, 0x00ff_ffff);
    let layout = layout(content, width, height, dpi);
    let text = runs(content, &layout, dpi);
    let mut ink = Vec::new();
    let mut drawn = true;
    for run in &text {
        drawn &= draw_text(surface.dc, &fonts, run);
        ink.push((run.area, run.color));
    }
    if let Some(area) = layout.rect(Group::Speed) {
        for (row, area) in rows(area, dpi).into_iter().enumerate() {
            let area = column(area, ARROW_LEFT_DIP, ARROW_RIGHT_DIP, dpi);
            drawn &= draw_arrow(surface.dc, area, dpi, row == 0);
            ink.push((area, row_color(content, Group::Speed, row)));
        }
    }
    // Flush before reading a DIB; all fonts and pens have already been deselected.
    if !drawn || GdiFlush() == 0 {
        return Err("无法绘制任务栏文字");
    }
    for (y, row) in surface
        .pixels_mut()
        .chunks_exact_mut(width as usize)
        .enumerate()
    {
        for (x, pixel) in row.iter_mut().enumerate() {
            let color = ink
                .iter()
                .find(|(area, _)| super::contains(*area, x as i32, y as i32))
                .map_or(foreground(content.light), |(_, color)| *color);
            *pixel = premultiply((*pixel & 0xff) as u8, color);
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
mod tests;
