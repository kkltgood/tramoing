//! 真正的任务栏子窗口：原生 GDI 绘制、普通鼠标消息，无置顶浮窗或全局鼠标轮询。
//!
//! Win10 缩小任务按钮宿主给控件留位，禁用/退出恢复原尺寸。Win11 的 XAML
//! 任务栏没有公开的任意文字扩展接口，仅在已知布局存在空位时挂接；不支持的
//! 布局返回状态给 UI。Explorer 重建后下一次 update 自动重新挂接。
use crate::screen::Rect;
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Once;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "Tramoing.TaskbarSpeed";

#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub down: String,
    pub up: String,
    pub light: bool,
    pub colored: bool,
}

struct WindowState {
    hwnd: Cell<HWND>,
    content: RefCell<Content>,
    click: fn(Option<Rect>),
    pressed: Cell<bool>,
}

struct Reservation {
    hwnd: HWND,
    original: RECT,
    applied: RECT,
}

pub struct TaskbarBadge {
    state: Box<WindowState>,
    shell: HWND,
    reservation: Option<Reservation>,
}

impl TaskbarBadge {
    pub fn new(click: fn(Option<Rect>)) -> Self {
        Self {
            state: Box::new(WindowState {
                hwnd: Cell::new(null_mut()),
                content: RefCell::new(Content {
                    down: "0".into(),
                    up: "0".into(),
                    light: false,
                    colored: false,
                }),
                click,
                pressed: Cell::new(false),
            }),
            shell: null_mut(),
            reservation: None,
        }
    }

    /// None 表示禁用。成功时窗口的 GetParent 必须是当前 Shell_TrayWnd。
    pub fn update(&mut self, content: Option<Content>) -> Result<(), &'static str> {
        let Some(content) = content else {
            self.detach();
            return Ok(());
        };
        let changed = *self.state.content.borrow() != content;
        *self.state.content.borrow_mut() = content;
        unsafe {
            let shell = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
            if shell.is_null() {
                self.detach();
                return Err("等待 Windows 任务栏恢复");
            }
            if self.shell != shell || IsWindow(self.state.hwnd.get()) == 0 {
                self.detach();
                register_class();
                let hwnd = CreateWindowExW(
                    // Win11 的任务栏由 DirectComposition 绘制，普通 GDI 子窗口
                    // 会被它的合成表面覆盖。常量 alpha=255 给子窗口独立表面，
                    // 无颜色键/点击穿透，父窗口仍然是任务栏。
                    WS_EX_NOACTIVATE | WS_EX_LAYERED,
                    wide(CLASS).as_ptr(),
                    wide("Tramoing Speed").as_ptr(),
                    WS_CHILD | WS_CLIPSIBLINGS,
                    0,
                    0,
                    0,
                    0,
                    shell,
                    null_mut(),
                    GetModuleHandleW(null()),
                    (&*self.state as *const WindowState).cast(),
                );
                if hwnd.is_null() {
                    return Err("无法挂接 Windows 任务栏");
                }
                self.shell = shell;
                self.state.hwnd.set(hwnd);
                if SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA) == 0 {
                    self.detach();
                    return Err("当前系统无法合成任务栏网速窗口");
                }
            }
            let result = self.layout();
            if result.is_err() {
                ShowWindow(self.state.hwnd.get(), SW_HIDE);
                self.restore();
            } else if changed {
                InvalidateRect(self.state.hwnd.get(), null(), 0);
            }
            result
        }
    }

    unsafe fn layout(&mut self) -> Result<(), &'static str> {
        let band = client_rect(self.shell).ok_or("无法读取任务栏布局")?;
        let dpi = GetDpiForWindow(self.shell).max(96) as i32;
        let width = 60 * dpi / 96;
        let gap = 4 * dpi / 96;
        let tray = FindWindowExW(
            self.shell,
            null_mut(),
            wide("TrayNotifyWnd").as_ptr(),
            null(),
        );
        let tray_rect = relative_rect(tray, self.shell).ok_or("当前任务栏布局不支持网速区域")?;
        let modern = !FindWindowExW(
            self.shell,
            null_mut(),
            wide("Windows.UI.Composition.DesktopWindowContentBridge").as_ptr(),
            null(),
        )
        .is_null();
        let vertical = band.bottom > band.right;
        let mut target = if vertical {
            RECT {
                left: 0,
                top: tray_rect.top - 36 * dpi / 96 - gap,
                right: band.right,
                bottom: tray_rect.top - gap,
            }
        } else {
            RECT {
                left: tray_rect.left - width - gap,
                top: 0,
                right: tray_rect.left - gap,
                bottom: band.bottom,
            }
        };
        // 避开任务栏上的第三方子窗口。XAML/系统窗口单独处理。
        let mut obstacles = Vec::new();
        let mut child = FindWindowExW(self.shell, null_mut(), null(), null());
        while !child.is_null() {
            let class = class_name(child);
            if child != self.state.hwnd.get()
                && IsWindowVisible(child) != 0
                && !matches!(
                    class.as_str(),
                    "Windows.UI.Composition.DesktopWindowContentBridge"
                        | "Windows.UI.Core.CoreWindow"
                        | "TrayNotifyWnd"
                        | "TrayDummySearchControl"
                        | "ReBarWindow32"
                        | "MSTaskSwWClass"
                        | "MSTaskListWClass"
                        | "Start"
                        | "tooltips_class32"
                )
            {
                if let Some(r) = relative_rect(child, self.shell) {
                    obstacles.push(r);
                }
            }
            child = FindWindowExW(self.shell, child, null(), null());
        }
        for _ in 0..obstacles.len() + 1 {
            let mut moved = false;
            for r in &obstacles {
                if intersects(target, *r) {
                    if vertical {
                        let h = target.bottom - target.top;
                        target.bottom = r.top - gap;
                        target.top = target.bottom - h;
                    } else {
                        target.right = r.left - gap;
                        target.left = target.right - width;
                    }
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        if target.left < gap
            || target.top < 0
            || target.right > band.right
            || target.bottom > band.bottom
        {
            // 纵向 left=0 是合法的。
            if !(vertical && target.top >= gap && target.bottom <= band.bottom) {
                return Err("任务栏空间不足，无法放入网速区域");
            }
        }
        let rebar = FindWindowExW(
            self.shell,
            null_mut(),
            wide("ReBarWindow32").as_ptr(),
            null(),
        );
        let task_rect = relative_rect(rebar, self.shell).ok_or("当前任务栏布局不支持网速区域")?;
        if modern {
            self.restore();
            // 新任务栏的按钮实际由 XAML 绘制，不能靠缩小旧 ReBar 假装预留成功。
            if vertical || task_rect.right + gap > target.left {
                return Err("任务栏按钮已占满网速区域，请减少固定按钮");
            }
        } else {
            if let Some(reservation) = &self.reservation {
                if reservation.hwnd != rebar || !same_rect(task_rect, reservation.applied) {
                    self.reservation = None; // Explorer 已重排，以它的新尺寸为准
                }
            }
            let original = self
                .reservation
                .as_ref()
                .map(|r| r.original)
                .unwrap_or(task_rect);
            let mut applied = original;
            if vertical {
                applied.bottom = target.top - gap;
            } else {
                applied.right = target.left - gap;
            }
            let (main_size, cross_size) = if vertical {
                (applied.bottom - applied.top, applied.right - applied.left)
            } else {
                (applied.right - applied.left, applied.bottom - applied.top)
            };
            if main_size < 100 * dpi / 96 || cross_size < 20 {
                return Err("任务栏空间不足，无法放入网速区域");
            }
            if !same_rect(task_rect, applied) && !move_child(rebar, applied) {
                return Err("无法为网速区域预留任务栏空间");
            }
            self.reservation = Some(Reservation {
                hwnd: rebar,
                original,
                applied,
            });
        }
        // HWND_TOP 仅调整兄弟子窗口顺序，不会进入顶层 TOPMOST 序列。
        if SetWindowPos(
            self.state.hwnd.get(),
            HWND_TOP,
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ) == 0
        {
            return Err("任务栏网速区域定位失败");
        }
        Ok(())
    }

    fn restore(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            unsafe {
                // 只恢复仍由我们修改的矩形；不覆盖系统/其他插件之后的新布局。
                if relative_rect(reservation.hwnd, self.shell)
                    .is_some_and(|r| same_rect(r, reservation.applied))
                {
                    move_child(reservation.hwnd, reservation.original);
                }
            }
        }
    }

    fn detach(&mut self) {
        self.restore();
        let hwnd = self.state.hwnd.replace(null_mut());
        unsafe {
            if !hwnd.is_null() && IsWindow(hwnd) != 0 {
                DestroyWindow(hwnd);
            }
        }
        self.shell = null_mut();
    }
}

impl Drop for TaskbarBadge {
    fn drop(&mut self) {
        self.detach();
    }
}

fn register_class() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_HAND),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
    });
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lp as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState).as_ref();
    match (msg, state) {
        (WM_PAINT, Some(state)) => {
            paint(hwnd, &state.content.borrow());
            0
        }
        (WM_ERASEBKGND, _) => 1,
        (WM_MOUSEACTIVATE, _) => MA_NOACTIVATE as isize,
        (WM_LBUTTONDOWN, Some(state)) => {
            state.pressed.set(true);
            SetCapture(hwnd);
            0
        }
        (WM_LBUTTONUP, Some(state)) => {
            let pressed = state.pressed.replace(false);
            ReleaseCapture();
            let x = lp as i16 as i32;
            let y = (lp >> 16) as i16 as i32;
            if pressed
                && client_rect(hwnd)
                    .is_some_and(|r| x >= 0 && y >= 0 && x < r.right && y < r.bottom)
            {
                (state.click)(screen_rect(hwnd));
            }
            0
        }
        (WM_CAPTURECHANGED, Some(state)) => {
            state.pressed.set(false);
            0
        }
        (WM_NCDESTROY, Some(state)) => {
            state.hwnd.set(null_mut());
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        (WM_THEMECHANGED | WM_DPICHANGED | WM_SIZE, _) => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn paint(hwnd: HWND, data: &Content) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    let r = client_rect(hwnd).unwrap_or(RECT {
        left: 0,
        top: 0,
        right: 60,
        bottom: 48,
    });
    let bg = if data.light {
        rgb(243, 243, 243)
    } else {
        rgb(32, 32, 32)
    };
    let brush = CreateSolidBrush(bg);
    FillRect(dc, &r, brush);
    DeleteObject(brush);
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
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
        CLEARTYPE_QUALITY as u32,
        DEFAULT_PITCH as u32,
        wide("Segoe UI").as_ptr(),
    );
    let old = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    let line = (14 * dpi / 96).min(r.bottom / 2);
    let top = (r.bottom - line * 2) / 2;
    for (i, (arrow, number)) in [("↓", &data.down), ("↑", &data.up)].iter().enumerate() {
        let color = if data.colored {
            let c = if i == 0 {
                tm_core::color::download(!data.light)
            } else {
                tm_core::color::upload(!data.light)
            };
            rgb(c.0, c.1, c.2)
        } else if data.light {
            rgb(24, 24, 28)
        } else {
            rgb(238, 238, 242)
        };
        SetTextColor(dc, color);
        let text = wide(&format!("{arrow} {number}"));
        let mut line_rect = RECT {
            left: 3 * dpi / 96,
            right: r.right - 3 * dpi / 96,
            top: top + i as i32 * line,
            bottom: top + (i as i32 + 1) * line,
        };
        DrawTextW(
            dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut line_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    SelectObject(dc, old);
    DeleteObject(font);
    EndPaint(hwnd, &ps);
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn same_rect(a: RECT, b: RECT) -> bool {
    (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
}
fn intersects(a: RECT, b: RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}
unsafe fn move_child(hwnd: HWND, r: RECT) -> bool {
    SetWindowPos(
        hwnd,
        null_mut(),
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        SWP_NOACTIVATE | SWP_NOZORDER,
    ) != 0
}
unsafe fn client_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = std::mem::zeroed();
    (GetClientRect(hwnd, &mut r) != 0).then_some(r)
}
unsafe fn relative_rect(hwnd: HWND, parent: HWND) -> Option<RECT> {
    if hwnd.is_null() {
        return None;
    }
    let mut r: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut r) == 0 {
        return None;
    }
    let mut point = POINT {
        x: r.left,
        y: r.top,
    };
    if ScreenToClient(parent, &mut point) == 0 {
        return None;
    }
    Some(RECT {
        left: point.x,
        top: point.y,
        right: point.x + r.right - r.left,
        bottom: point.y + r.bottom - r.top,
    })
}
unsafe fn screen_rect(hwnd: HWND) -> Option<Rect> {
    let mut r: RECT = std::mem::zeroed();
    (GetWindowRect(hwnd, &mut r) != 0).then_some(Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    })
}
unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CLICKS: AtomicUsize = AtomicUsize::new(0);

    /// 需要交互式 Windows 桌面；不在无桌面的 CI 中修改 Explorer 布局。
    #[test]
    #[ignore = "requires interactive Windows taskbar"]
    fn native_child_lifecycle_and_clicks() {
        let mut badge = TaskbarBadge::new(|rect| {
            assert!(rect.is_some());
            CLICKS.fetch_add(1, Ordering::Relaxed);
        });
        let content = Content {
            down: "12M".into(),
            up: "3.4K".into(),
            light: false,
            colored: true,
        };
        badge.update(Some(content.clone())).unwrap();
        unsafe {
            let hwnd = badge.state.hwnd.get();
            assert!(!hwnd.is_null());
            assert_eq!(GetParent(hwnd), badge.shell);
            assert_ne!(GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CHILD, 0);
            assert_eq!(GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_POPUP, 0);
            assert_eq!(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST, 0);
            assert_ne!(IsWindowVisible(hwnd), 0);
            let r = relative_rect(hwnd, badge.shell).unwrap();
            let band = client_rect(badge.shell).unwrap();
            assert!(r.left >= 0 && r.top >= 0 && r.right <= band.right && r.bottom <= band.bottom);
            UpdateWindow(hwnd);
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, 4 | (4 << 16));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, 4 | (4 << 16));
            assert_eq!(CLICKS.load(Ordering::Relaxed), 1);
            // 模拟原生子窗口丢失，验证下一帧恢复，不重启用户的 Explorer。
            DestroyWindow(hwnd);
            assert!(badge.state.hwnd.get().is_null());
            badge.update(Some(content)).unwrap();
            let recovered = badge.state.hwnd.get();
            assert_eq!(GetParent(recovered), badge.shell);
            badge.update(None).unwrap();
            assert_eq!(IsWindow(recovered), 0);
            assert!(badge.reservation.is_none());
        }
    }
}
