//! Opt-in desktop overlay. No game hooks or process injection are used.
//! Own this object on the UI thread, whose message loop dispatches its messages.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Once;
use tm_core::color::Rgb;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "Tramoing.GameOverlay";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content {
    /// Physical desktop anchor selecting the monitor, including negative monitors.
    pub x: i32,
    pub y: i32,
    /// Position within the monitor's available travel span, in 0..=10000 basis points.
    pub x_percent: u16,
    pub y_percent: u16,
    pub locked: bool,
    /// Font height in device-independent pixels.
    pub font_size: u16,
    /// Background opacity, in percent. Glyphs retain their full brightness.
    pub opacity: u8,
    pub segments: Vec<Segment>,
}

/// One metric in the single horizontal row, with its own text color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub color: Rgb,
}

/// Persist all four fields after a drag, display change, or percentage adjustment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub x_percent: u16,
    pub y_percent: u16,
}

#[derive(Clone, Copy)]
struct Drag {
    cursor: (i32, i32),
    origin: (i32, i32),
}

struct State {
    hwnd: Cell<HWND>,
    locked: Cell<bool>,
    position: Cell<(i32, i32)>,
    size: Cell<(i32, i32)>,
    drag: Cell<Option<Drag>>,
    moved: Cell<Option<(i32, i32)>>,
    dirty: Cell<bool>,
    content: RefCell<Option<Content>>,
}

pub struct GameOverlay {
    state: Box<State>,
}

impl Default for GameOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl GameOverlay {
    pub fn new() -> Self {
        Self {
            state: Box::new(State {
                hwnd: Cell::new(null_mut()),
                locked: Cell::new(true),
                position: Cell::new((24, 24)),
                size: Cell::new((1, 1)),
                drag: Cell::new(None),
                moved: Cell::new(None),
                dirty: Cell::new(true),
                content: RefCell::new(None),
            }),
        }
    }

    /// Disable with `None` to destroy the window immediately. Persist a returned
    /// placement after a drag or monitor recovery. Ordinary percentage placement
    /// and metric width changes keep the saved monitor anchor unchanged.
    pub fn update(&mut self, content: Option<Content>) -> Result<Option<Placement>, &'static str> {
        let Some(mut content) = content.filter(|content| !content.segments.is_empty()) else {
            self.destroy();
            return Ok(None);
        };
        // Bound allocations even if the caller supplies corrupted settings/text.
        content.font_size = content.font_size.clamp(12, 32);
        content.opacity = content.opacity.clamp(20, 100);
        let normalized_percent = content.x_percent > 10000 || content.y_percent > 10000;
        content.x_percent = content.x_percent.min(10000);
        content.y_percent = content.y_percent.min(10000);
        content.segments.truncate(8);
        for segment in &mut content.segments {
            if let Some((boundary, _)) = segment.text.char_indices().nth(160) {
                segment.text.truncate(boundary);
            }
        }
        let mut dragged = self.state.moved.take();
        unsafe {
            let mut hwnd = self.state.hwnd.get();
            if hwnd.is_null() || IsWindow(hwnd) == 0 {
                register_class();
                hwnd = CreateWindowExW(
                    window_style(content.locked),
                    wide(CLASS).as_ptr(),
                    wide("Tramoing Game Overlay").as_ptr(),
                    WS_POPUP,
                    content.x,
                    content.y,
                    1,
                    1,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    (&*self.state as *const State).cast(),
                );
                if hwnd.is_null() {
                    return Err("无法创建游戏悬浮窗");
                }
                self.state.hwnd.set(hwnd);
                self.state.position.set((content.x, content.y));
                self.state.dirty.set(true);
            }
            if self.state.locked.replace(content.locked) != content.locked {
                self.state.end_drag();
                SetWindowLongW(hwnd, GWL_EXSTYLE, window_style(content.locked) as i32);
            }
            // Locking while a drag is active also completes it; preserve that
            // final position instead of applying the pre-drag settings again.
            if let Some(position) = self.state.moved.take() {
                dragged = Some(position);
            }
            if self.state.drag.get().is_some() {
                // Preserve the native drag position and dimensions until release.
                return Ok(None);
            }
            if let Some(position) = dragged {
                let placement = placement_from_drag(position, self.state.size.get());
                content.x = placement.x;
                content.y = placement.y;
                content.x_percent = placement.x_percent;
                content.y_percent = placement.y_percent;
            }
            let recovered_monitor = monitor_at((content.x, content.y)).is_some_and(|monitor| {
                content.x < monitor.left
                    || content.x >= monitor.right
                    || content.y < monitor.top
                    || content.y >= monitor.bottom
            });
            // Placement and mouse passthrough do not change the pixels. Keep the
            // existing layered bitmap while moving/locking; WM_DPICHANGED still
            // invalidates it when the window enters a differently scaled screen.
            let pixels_changed = self.state.content.borrow().as_ref().is_none_or(|previous| {
                previous.font_size != content.font_size
                    || previous.opacity != content.opacity
                    || previous.segments != content.segments
            });
            if pixels_changed {
                self.state.dirty.set(true);
            }
            // Move before choosing the font so crossing monitors uses the new DPI.
            let initial = configured_placement(&content, self.state.size.get());
            if (initial.x, initial.y) != self.state.position.get() {
                if SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    initial.x,
                    initial.y,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOSIZE,
                ) == 0
                {
                    return Err("无法定位游戏悬浮窗");
                }
                self.state.position.set((initial.x, initial.y));
            }
            if self.state.dirty.get() {
                let dpi = GetDpiForWindow(hwnd).clamp(96, 768);
                let surface = rasterize(&content, dpi)?;
                self.state.size.set((surface.width, surface.height));
                let placement = configured_placement(&content, self.state.size.get());
                let position = (placement.x, placement.y);
                self.state.position.set(position);
                surface.present(hwnd, position)?;
                // A recovered offscreen position can enter a monitor with a
                // different DPI during UpdateLayeredWindow itself.
                self.state
                    .dirty
                    .set(GetDpiForWindow(hwnd).clamp(96, 768) != dpi);
            } else {
                // Display topology can change without a new sample or DPI change.
                self.state.position.set((initial.x, initial.y));
            }
            let position = self.state.position.get();
            let placement = Placement {
                x: position.0,
                y: position.1,
                x_percent: content.x_percent,
                y_percent: content.y_percent,
            };
            let persist = dragged.is_some() || recovered_monitor || normalized_percent;
            if persist {
                content.x = position.0;
                content.y = position.1;
            }
            *self.state.content.borrow_mut() = Some(content);
            if SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                position.0,
                position.1,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW,
            ) == 0
            {
                return Err("无法置顶游戏悬浮窗");
            }
            Ok(persist.then_some(placement))
        }
    }

    fn destroy(&mut self) {
        unsafe {
            self.state.end_drag();
            let hwnd = self.state.hwnd.replace(null_mut());
            if !hwnd.is_null() && IsWindow(hwnd) != 0 {
                DestroyWindow(hwnd);
            }
        }
        self.state.moved.set(None);
        self.state.content.replace(None);
        self.state.dirty.set(true);
    }
}

impl Drop for GameOverlay {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl State {
    unsafe fn end_drag(&self) {
        if self.drag.take().is_some() {
            let position = visible_position(self.position.get(), self.size.get());
            self.position.set(position);
            self.moved.set(Some(position));
            if GetCapture() == self.hwnd.get() {
                ReleaseCapture();
            }
        }
    }
}

fn window_style(locked: bool) -> u32 {
    WS_EX_LAYERED
        | WS_EX_TOPMOST
        | WS_EX_TOOLWINDOW
        | WS_EX_NOACTIVATE
        | if locked { WS_EX_TRANSPARENT } else { 0 }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let name = wide(CLASS);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_SIZEALL),
            lpszClassName: name.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&class);
    });
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = &*pointer;
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as isize,
        WM_NCHITTEST if state.locked.get() => return HTTRANSPARENT as isize,
        WM_LBUTTONDOWN if !state.locked.get() => {
            let mut point: POINT = std::mem::zeroed();
            if GetCursorPos(&mut point) != 0 {
                state.drag.set(Some(Drag {
                    cursor: (point.x, point.y),
                    origin: state.position.get(),
                }));
                SetCapture(hwnd);
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            if let Some(drag) = state.drag.get() {
                let mut point: POINT = std::mem::zeroed();
                if GetCursorPos(&mut point) != 0 {
                    let position = (
                        drag.origin
                            .0
                            .saturating_add(point.x.saturating_sub(drag.cursor.0)),
                        drag.origin
                            .1
                            .saturating_add(point.y.saturating_sub(drag.cursor.1)),
                    );
                    if SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        position.0,
                        position.1,
                        0,
                        0,
                        SWP_NOACTIVATE | SWP_NOSIZE,
                    ) != 0
                    {
                        state.position.set(position);
                    }
                }
            }
            return 0;
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED | WM_CANCELMODE => {
            state.end_drag();
            return 0;
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            state.dirty.set(true);
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            ValidateRect(hwnd, null());
            return 0;
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            state.hwnd.set(null_mut());
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn clamp_to_monitor(position: (i32, i32), size: (i32, i32), monitor: RECT) -> (i32, i32) {
    let right = monitor.right.saturating_sub(size.0).max(monitor.left);
    let bottom = monitor.bottom.saturating_sub(size.1).max(monitor.top);
    (
        position.0.clamp(monitor.left, right),
        position.1.clamp(monitor.top, bottom),
    )
}

fn visible_position(position: (i32, i32), size: (i32, i32)) -> (i32, i32) {
    monitor_at(position)
        .map(|monitor| clamp_to_monitor(position, size, monitor))
        .unwrap_or(position)
}

fn monitor_at(position: (i32, i32)) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: position.0,
                y: position.1,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
            // Full monitor bounds deliberately allow placement over the taskbar.
            Some(info.rcMonitor)
        } else {
            None
        }
    }
}

fn position_for_percent(size: (i32, i32), percent: (u16, u16), monitor: RECT) -> (i32, i32) {
    let coordinate = |start: i32, end: i32, length: i32, percent: u16| {
        let span = (i64::from(end) - i64::from(start) - i64::from(length)).max(0);
        (i64::from(start) + (span * i64::from(percent.min(10000)) + 5000) / 10000) as i32
    };
    (
        coordinate(monitor.left, monitor.right, size.0, percent.0),
        coordinate(monitor.top, monitor.bottom, size.1, percent.1),
    )
}

fn percent_for_position(position: (i32, i32), size: (i32, i32), monitor: RECT) -> (u16, u16) {
    let percent = |coordinate: i32, start: i32, end: i32, length: i32| {
        let span = (i64::from(end) - i64::from(start) - i64::from(length)).max(0);
        if span == 0 {
            return 0;
        }
        let offset = (i64::from(coordinate) - i64::from(start)).clamp(0, span);
        ((offset * 10000 + span / 2) / span) as u16
    };
    (
        percent(position.0, monitor.left, monitor.right, size.0),
        percent(position.1, monitor.top, monitor.bottom, size.1),
    )
}

fn configured_placement(content: &Content, size: (i32, i32)) -> Placement {
    let anchor = (content.x, content.y);
    let position = monitor_at(anchor)
        .map(|monitor| position_for_percent(size, (content.x_percent, content.y_percent), monitor))
        .unwrap_or(anchor);
    Placement {
        x: position.0,
        y: position.1,
        x_percent: content.x_percent,
        y_percent: content.y_percent,
    }
}

fn placement_from_drag(position: (i32, i32), size: (i32, i32)) -> Placement {
    let (position, percent) = monitor_at(position)
        .map(|monitor| {
            let position = clamp_to_monitor(position, size, monitor);
            (position, percent_for_position(position, size, monitor))
        })
        .unwrap_or((position, (0, 0)));
    Placement {
        x: position.0,
        y: position.1,
        x_percent: percent.0,
        y_percent: percent.1,
    }
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Surface {
    unsafe fn new(width: i32, height: i32) -> Result<Self, &'static str> {
        if width <= 0 || height <= 0 || width > 4096 || height > 4096 {
            return Err("游戏悬浮窗尺寸超出范围");
        }
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return Err("无法创建游戏悬浮窗绘图上下文");
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
            return Err("无法创建游戏悬浮窗位图");
        }
        let original = SelectObject(dc, bitmap);
        if original.is_null() || original as isize == -1 {
            DeleteObject(bitmap);
            DeleteDC(dc);
            return Err("无法选取游戏悬浮窗位图");
        }
        Ok(Self {
            dc,
            bitmap,
            original,
            bits: bits.cast(),
            width,
            height,
        })
    }

    unsafe fn pixels_mut(&mut self) -> &mut [u32] {
        std::slice::from_raw_parts_mut(self.bits, self.width as usize * self.height as usize)
    }

    unsafe fn present(&self, hwnd: HWND, position: (i32, i32)) -> Result<(), &'static str> {
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        if UpdateLayeredWindow(
            hwnd,
            null_mut(),
            &POINT {
                x: position.0,
                y: position.1,
            },
            &SIZE {
                cx: self.width,
                cy: self.height,
            },
            self.dc,
            &POINT { x: 0, y: 0 },
            0,
            &blend,
            ULW_ALPHA,
        ) == 0
        {
            return Err("无法绘制游戏悬浮窗");
        }
        Ok(())
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

struct Font(HFONT);
impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}

unsafe fn rasterize(content: &Content, dpi: u32) -> Result<Surface, &'static str> {
    let px = |dip: i32| (dip * dpi as i32 / 96).max(1);
    let font = Font(CreateFontW(
        -px(i32::from(content.font_size)),
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
        ANTIALIASED_QUALITY as u32,
        DEFAULT_PITCH as u32,
        wide("Segoe UI").as_ptr(),
    ));
    if font.0.is_null() {
        return Err("无法创建游戏悬浮窗字体");
    }
    let measure = Surface::new(1, 1)?;
    let old_font = SelectObject(measure.dc, font.0);
    let mut widths = Vec::new();
    let mut line_height = px(i32::from(content.font_size));
    for segment in &content.segments {
        let text = wide(&segment.text);
        let mut size: SIZE = std::mem::zeroed();
        if GetTextExtentPoint32W(
            measure.dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut size,
        ) == 0
        {
            SelectObject(measure.dc, old_font);
            return Err("无法测量游戏悬浮窗文字");
        }
        widths.push(size.cx);
        line_height = line_height.max(size.cy);
    }
    SelectObject(measure.dc, old_font);
    let horizontal_padding = px(10);
    let vertical_padding = px(7);
    let gap = px(14);
    let width = (widths.iter().sum::<i32>()
        + gap * content.segments.len().saturating_sub(1) as i32
        + horizontal_padding * 2)
        .clamp(px(80), 4096);
    let height = line_height + vertical_padding * 2;
    let mut surface = Surface::new(width, height)?;
    surface.pixels_mut().fill(0);
    let old_font = SelectObject(surface.dc, font.0);
    SetBkMode(surface.dc, TRANSPARENT as i32);
    SetTextColor(surface.dc, 0x00ff_ffff);
    let mut left = horizontal_padding;
    let mut runs = Vec::with_capacity(content.segments.len());
    for (segment, segment_width) in content.segments.iter().zip(widths) {
        let text = wide(&segment.text);
        let mut rect = RECT {
            left,
            top: vertical_padding,
            right: (left + segment_width).min(width - horizontal_padding),
            bottom: vertical_padding + line_height,
        };
        if rect.left < rect.right {
            DrawTextW(
                surface.dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            runs.push((rect.left, rect.right, segment.color));
        }
        left += segment_width + gap;
    }
    SelectObject(surface.dc, old_font);
    GdiFlush();
    let background = u32::from(content.opacity) * 255 / 100;
    let radius = px(7);
    for (index, pixel) in surface.pixels_mut().iter_mut().enumerate() {
        let x = index as i32 % width;
        let y = index as i32 / width;
        let dx = (radius - x).max(x - (width - radius - 1)).max(0);
        let dy = (radius - y).max(y - (height - radius - 1)).max(0);
        if dx * dx + dy * dy > radius * radius {
            *pixel = 0;
        } else {
            // GDI drew grayscale coverage onto black. Color each metric without
            // reducing glyph opacity along with the translucent background.
            let coverage = (*pixel & 255)
                .max((*pixel >> 8) & 255)
                .max((*pixel >> 16) & 255);
            let alpha = coverage + background * (255 - coverage) / 255;
            let Rgb(red, green, blue) = runs
                .iter()
                .find(|(left, right, _)| x >= *left && x < *right)
                .map(|(_, _, color)| *color)
                .unwrap_or(Rgb(240, 240, 240));
            let channel = |color: u8| {
                coverage * u32::from(color) / 255 + 18 * background * (255 - coverage) / 65025
            };
            *pixel = (alpha << 24) | (channel(red) << 16) | (channel(green) << 8) | channel(blue);
        }
    }
    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_content() -> Content {
        Content {
            x: 24,
            y: 24,
            x_percent: 200,
            y_percent: 200,
            locked: true,
            font_size: 16,
            opacity: 20,
            segments: vec![
                Segment {
                    text: "↓ 12.4 MB/s".into(),
                    color: Rgb(67, 211, 148),
                },
                Segment {
                    text: "↑ 256 KB/s".into(),
                    color: Rgb(58, 183, 255),
                },
                Segment {
                    text: "CPU 42%".into(),
                    color: Rgb(96, 180, 255),
                },
                Segment {
                    text: "CPU 58°C".into(),
                    color: Rgb(255, 179, 88),
                },
                Segment {
                    text: "GPU 81%".into(),
                    color: Rgb(185, 157, 255),
                },
                Segment {
                    text: "GPU 64°C".into(),
                    color: Rgb(255, 138, 167),
                },
                Segment {
                    text: "FPS 144".into(),
                    color: Rgb(245, 221, 99),
                },
            ],
        }
    }

    #[test]
    fn placement_keeps_negative_monitors_and_recovers_offscreen_coordinates() {
        let monitor = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        assert_eq!(
            clamp_to_monitor((-1800, -100), (300, 120), monitor),
            (-1800, -100)
        );
        assert_eq!(
            clamp_to_monitor((-10, 850), (300, 120), monitor),
            (-300, 760)
        );
        assert_eq!(
            clamp_to_monitor((i32::MIN, i32::MAX), (300, 120), monitor),
            (-1920, 760)
        );
        assert_eq!(
            clamp_to_monitor((-100, 20), (3000, 2000), monitor),
            (-1920, -200)
        );
    }

    #[test]
    fn percentages_follow_monitor_edges_and_resize_including_negative_displays() {
        let monitor = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        assert_eq!(
            position_for_percent((600, 44), (0, 0), monitor),
            (-1920, -200)
        );
        assert_eq!(
            position_for_percent((600, 44), (10000, 10000), monitor),
            (-600, 836)
        );
        assert_eq!(
            position_for_percent((800, 60), (10000, 10000), monitor),
            (-800, 820)
        );
        assert_eq!(
            position_for_percent((600, 44), (5000, 5000), monitor),
            (-1260, 318)
        );
        assert_eq!(
            position_for_percent((3000, 1200), (10000, 10000), monitor),
            (-1920, -200)
        );
        assert_eq!(
            percent_for_position((-600, 836), (600, 44), monitor),
            (10000, 10000)
        );
        assert_eq!(
            percent_for_position((-1260, 318), (600, 44), monitor),
            (5000, 5000)
        );
        for position in [(-1900, -180), (-1000, 400), (-605, 830)] {
            let percent = percent_for_position(position, (600, 44), monitor);
            assert_eq!(
                position_for_percent((600, 44), percent, monitor),
                position,
                "dragged positions round-trip without snapping back"
            );
        }
    }

    #[test]
    fn lock_changes_only_mouse_passthrough() {
        assert_eq!(window_style(true) ^ window_style(false), WS_EX_TRANSPARENT);
        for locked in [true, false] {
            let style = window_style(locked);
            assert_ne!(style & WS_EX_NOACTIVATE, 0);
            assert_ne!(style & WS_EX_TOPMOST, 0);
            assert_ne!(style & WS_EX_TOOLWINDOW, 0);
        }
    }

    #[test]
    fn raster_is_bounded_premultiplied_and_keeps_text_opaque() {
        let content = demo_content();
        for dpi in [96, 144, 192] {
            let mut surface = unsafe { rasterize(&content, dpi).unwrap() };
            let mut single_metric = content.clone();
            single_metric.segments.truncate(1);
            let single = unsafe { rasterize(&single_metric, dpi).unwrap() };
            assert_eq!(
                surface.height, single.height,
                "all metrics must remain on one row"
            );
            assert!(surface.width > single.width * 4);
            assert!(
                surface.height >= (16 + 7 * 2) * dpi as i32 / 96 - 1,
                "16 DIP text retains 7 DIP of padding above and below"
            );
            export_raster(&mut surface, &format!("overlay-{dpi}"));
            let pixels = unsafe { surface.pixels_mut() };
            assert!(pixels.iter().any(|pixel| pixel >> 24 > 230));
            assert!(pixels.iter().any(|pixel| pixel >> 24 == 51));
            for segment in &content.segments {
                let Rgb(red, green, blue) = segment.color;
                let solid =
                    0xff00_0000 | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32;
                assert!(
                    pixels.contains(&solid),
                    "each metric keeps its configured color"
                );
            }
            for pixel in pixels {
                let alpha = *pixel >> 24;
                assert!(*pixel & 255 <= alpha);
                assert!((*pixel >> 8) & 255 <= alpha);
                assert!((*pixel >> 16) & 255 <= alpha);
            }
            // Keep Chinese, accents, descenders and both Celsius forms inside
            // the chosen 7 DIP vertical / 10 DIP horizontal inset at each scale.
            let mut accents = content.clone();
            accents.segments = vec![Segment {
                text: "FPS -- 中文 Ágjpqy 58°C 64℃".into(),
                color: Rgb(255, 255, 255),
            }];
            let mut sample = unsafe { rasterize(&accents, dpi).unwrap() };
            let width = sample.width as usize;
            let height = sample.height as usize;
            export_raster(&mut sample, &format!("glyphs-{dpi}"));
            let horizontal_padding = (10 * dpi / 96) as usize;
            let vertical_padding = (7 * dpi / 96) as usize;
            for (index, pixel) in unsafe { sample.pixels_mut() }.iter().enumerate() {
                if pixel >> 24 > 51 {
                    let row = index / width;
                    let column = index % width;
                    assert!(
                        row >= vertical_padding
                            && row < height - vertical_padding
                            && column >= horizontal_padding
                            && column < width - horizontal_padding,
                        "glyphs stay inside the configured padding on every side"
                    );
                }
            }
        }
    }

    fn export_raster(surface: &mut Surface, name: &str) {
        if let Some(directory) = std::env::var_os("TM_OVERLAY_REVIEW_DIR") {
            use std::io::Write;
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let mut output = std::fs::File::create(directory.join(format!("{name}.ppm"))).unwrap();
            writeln!(output, "P6\n{} {}\n255", surface.width, surface.height).unwrap();
            for pixel in unsafe { surface.pixels_mut() } {
                let alpha = *pixel >> 24;
                let background = 72 * (255 - alpha) / 255;
                output
                    .write_all(&[
                        (((*pixel >> 16) & 255) + background) as u8,
                        (((*pixel >> 8) & 255) + background) as u8,
                        ((*pixel & 255) + background) as u8,
                    ])
                    .unwrap();
            }
        }
    }

    #[test]
    #[ignore = "requires interactive Windows desktop"]
    fn native_lifecycle_preserves_foreground_and_releases_window() {
        let mut overlay = GameOverlay::new();
        let mut content = demo_content();
        unsafe {
            let foreground = GetForegroundWindow();
            if let Some(placement) = overlay.update(Some(content.clone())).unwrap() {
                content.x = placement.x;
                content.y = placement.y;
                content.x_percent = placement.x_percent;
                content.y_percent = placement.y_percent;
            }
            assert_eq!(
                overlay.update(Some(content.clone())).unwrap(),
                None,
                "persisted percentage placement must settle without repeated saves"
            );
            let mut shorter = content.clone();
            shorter.segments.truncate(2);
            assert_eq!(
                overlay.update(Some(shorter)).unwrap(),
                None,
                "metric width changes must not request settings writes"
            );
            assert_eq!(
                overlay.update(Some(content.clone())).unwrap(),
                None,
                "saved monitor anchor stays valid across width changes"
            );
            let hwnd = overlay.state.hwnd.get();
            assert_ne!(IsWindowVisible(hwnd), 0);
            assert_eq!(foreground, GetForegroundWindow());
            assert_ne!(
                GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT,
                0
            );
            overlay
                .update(Some(Content {
                    locked: false,
                    ..content.clone()
                }))
                .unwrap();
            assert_eq!(foreground, GetForegroundWindow());
            assert_eq!(
                GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT,
                0
            );
            assert_eq!(
                SendMessageW(hwnd, WM_MOUSEACTIVATE, 0, 0),
                MA_NOACTIVATE as isize
            );
            // Inject only the position that an already-completed drag would report;
            // this avoids moving the user's pointer or stealing foreground focus.
            let drag_position = (content.x + 80, content.y + 60);
            overlay.state.position.set(drag_position);
            overlay.state.moved.set(Some(drag_position));
            let placement = overlay.update(Some(content.clone())).unwrap().unwrap();
            content.x = placement.x;
            content.y = placement.y;
            content.x_percent = placement.x_percent;
            content.y_percent = placement.y_percent;
            assert_eq!(
                overlay.update(Some(content)).unwrap(),
                None,
                "drag percentage rounding must settle after persistence"
            );
            overlay.update(None).unwrap();
            assert_eq!(IsWindow(hwnd), 0);
        }
    }
}
