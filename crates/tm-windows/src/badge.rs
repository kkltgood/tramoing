//! 真正的任务栏子窗口：原生 GDI 绘制、普通鼠标消息，无置顶浮窗或全局鼠标轮询。
//!
//! Win10 缩小任务按钮宿主给控件留位，禁用/退出恢复原尺寸。Win11 的 XAML
//! 任务栏没有公开的任意文字扩展接口，仅在已知布局存在空位时挂接；不支持的
//! 布局返回状态给 UI。Explorer 重建后下一次 update 自动重新挂接。
mod surface;

use crate::screen::Rect;
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Once;
use tm_core::taskbar::{TaskbarItem as Group, TaskbarOrder};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "Tramoing.TaskbarSpeed";

#[derive(Clone, Debug, PartialEq)]
pub struct Content {
    pub speed: Option<Speed>,
    pub performance: Option<Performance>,
    pub light: bool,
    pub colors: tm_core::color::DisplayColors,
    pub order: TaskbarOrder,
}

impl Content {
    fn visible_items(&self) -> impl Iterator<Item = Group> + '_ {
        self.order
            .items()
            .iter()
            .copied()
            .filter(|item| match item {
                Group::Speed => self.speed.is_some(),
                Group::Performance => self.performance.is_some(),
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Speed {
    pub down: String,
    pub up: String,
    pub colored: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Performance {
    pub cpu: String,
    pub gpu: String,
    pub colored: bool,
}

#[derive(Clone)]
struct Drag {
    group: Group,
    start: (i32, i32),
    original_order: TaskbarOrder,
    preview_order: TaskbarOrder,
    moved: bool,
}

impl Drag {
    fn new(group: Group, x: i32, y: i32, order: TaskbarOrder) -> Self {
        Self {
            group,
            start: (x, y),
            preview_order: order.clone(),
            original_order: order,
            moved: false,
        }
    }

    fn track(
        &mut self,
        x: i32,
        y: i32,
        area: RECT,
        layout: &surface::Layout,
        threshold: (i32, i32),
    ) {
        self.moved |=
            (x - self.start.0).abs() > threshold.0 || (y - self.start.1).abs() > threshold.1;
        self.preview_order = self.original_order.clone();
        if !self.moved || !contains(area, x, y) {
            return;
        }
        // 按原始布局中其他项目的中点确定插入位置，支持任意数量的统计项。
        // 不使用预览布局判定，避免宽度不同的项目换位后反复跳动。
        let target = layout
            .groups
            .iter()
            .filter(|(group, _)| *group != self.group)
            .take_while(|(_, rect)| {
                if layout.stacked() {
                    y >= (rect.top + rect.bottom) / 2
                } else {
                    x >= (rect.left + rect.right) / 2
                }
            })
            .count();
        let visible: Vec<_> = layout.groups.iter().map(|(group, _)| *group).collect();
        self.preview_order
            .move_visible(self.group, &visible, target);
    }
}

struct WindowState {
    hwnd: Cell<HWND>,
    content: RefCell<Content>,
    click: fn(Option<Rect>, bool),
    reordered: fn(TaskbarOrder),
    drag: RefCell<Option<Drag>>,
    needs_paint: Cell<bool>,
}

impl WindowState {
    fn display_content(&self) -> Content {
        let mut content = self.content.borrow().clone();
        if let Some(drag) = self.drag.borrow().as_ref() {
            content.order = drag.preview_order.clone();
        }
        content
    }

    unsafe fn repaint(&self) -> Result<(), &'static str> {
        surface::present(self.hwnd.get(), &self.display_content())?;
        self.needs_paint.set(false);
        Ok(())
    }

    unsafe fn cancel_drag(&self) {
        let drag = self.drag.borrow_mut().take();
        if drag.is_some() {
            self.needs_paint.set(true);
            if GetCapture() == self.hwnd.get() {
                ReleaseCapture();
            }
            let _ = self.repaint();
        }
    }

    unsafe fn track_drag(&self, x: i32, y: i32) {
        let (Some(mut drag), Some(rect)) =
            (self.drag.borrow().clone(), client_rect(self.hwnd.get()))
        else {
            return;
        };
        let dpi = GetDpiForWindow(self.hwnd.get()).max(96);
        let layout = surface::layout(&self.content.borrow(), rect.right, rect.bottom, dpi as i32);
        let previous = drag.preview_order.clone();
        drag.track(
            x,
            y,
            rect,
            &layout,
            (
                GetSystemMetricsForDpi(SM_CXDRAG, dpi).max(1),
                GetSystemMetricsForDpi(SM_CYDRAG, dpi).max(1),
            ),
        );
        let changed = previous != drag.preview_order;
        let moved = drag.moved;
        self.drag.replace(Some(drag));
        if changed {
            self.needs_paint.set(true);
            let _ = self.repaint();
        }
        if moved && layout.groups.len() > 1 {
            SetCursor(LoadCursorW(
                null_mut(),
                if layout.stacked() {
                    IDC_SIZENS
                } else {
                    IDC_SIZEWE
                },
            ));
        }
    }
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
    pub fn new(click: fn(Option<Rect>, bool), reordered: fn(TaskbarOrder)) -> Self {
        Self {
            state: Box::new(WindowState {
                hwnd: Cell::new(null_mut()),
                content: RefCell::new(Content {
                    speed: None,
                    performance: None,
                    light: false,
                    colors: Default::default(),
                    order: TaskbarOrder::default(),
                }),
                click,
                reordered,
                drag: RefCell::new(None),
                needs_paint: Cell::new(true),
            }),
            shell: null_mut(),
            reservation: None,
        }
    }

    /// None 表示禁用。成功时窗口的 GetParent 必须是当前 Shell_TrayWnd。
    pub fn update(&mut self, content: Option<Content>) -> Result<(), &'static str> {
        let Some(content) = content.filter(|c| c.visible_items().next().is_some()) else {
            self.detach();
            return Ok(());
        };
        let previous = self.state.content.borrow();
        let changed = *previous != content;
        let layout_changed =
            previous.visible_items().ne(content.visible_items()) || previous.order != content.order;
        drop(previous);
        if layout_changed {
            unsafe {
                self.state.cancel_drag();
            }
        }
        *self.state.content.borrow_mut() = content;
        if changed {
            self.state.needs_paint.set(true);
        }
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
                    // 会被它的合成表面覆盖。使用逐像素 alpha 合成，
                    // 背景透出任务栏，父窗口仍然是任务栏。
                    WS_EX_NOACTIVATE | WS_EX_LAYERED,
                    wide(CLASS).as_ptr(),
                    wide("Tramoing Monitor").as_ptr(),
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
                self.state.needs_paint.set(true);
            }
            let result = self.layout().and_then(|()| {
                if self.state.needs_paint.get() {
                    self.state.repaint()?;
                }
                Ok(())
            });
            if result.is_err() {
                self.state.cancel_drag();
                ShowWindow(self.state.hwnd.get(), SW_HIDE);
                self.restore();
            }
            result
        }
    }

    unsafe fn layout(&mut self) -> Result<(), &'static str> {
        let band = client_rect(self.shell).ok_or("无法读取任务栏布局")?;
        let dpi = GetDpiForWindow(self.shell).max(96) as i32;
        let width = surface::width_dip(&self.state.content.borrow()) * dpi / 96;
        let gap = 4 * dpi / 96;
        let tray = FindWindowExW(
            self.shell,
            null_mut(),
            wide("TrayNotifyWnd").as_ptr(),
            null(),
        );
        let tray_rect = relative_rect(tray, self.shell).ok_or("当前任务栏布局不支持监控区域")?;
        let modern = !FindWindowExW(
            self.shell,
            null_mut(),
            wide("Windows.UI.Composition.DesktopWindowContentBridge").as_ptr(),
            null(),
        )
        .is_null();
        let vertical = band.bottom > band.right;
        let stacked = vertical && band.right < width;
        let height = if stacked {
            let content = self.state.content.borrow();
            if band.right < surface::minimum_width_dip(&content) * dpi / 96 {
                return Err("任务栏太窄，无法放入统计项");
            }
            36 * content.visible_items().count() as i32
        } else {
            36
        } * dpi
            / 96;
        let mut target = if vertical {
            RECT {
                left: 0,
                top: tray_rect.top - height - gap,
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
                return Err("任务栏空间不足，无法放入监控区域");
            }
        }
        let rebar = FindWindowExW(
            self.shell,
            null_mut(),
            wide("ReBarWindow32").as_ptr(),
            null(),
        );
        let task_rect = relative_rect(rebar, self.shell).ok_or("当前任务栏布局不支持监控区域")?;
        if modern {
            self.restore();
            // 新任务栏的按钮实际由 XAML 绘制，不能靠缩小旧 ReBar 假装预留成功。
            if vertical || task_rect.right + gap > target.left {
                return Err("任务栏按钮已占满监控区域，请减少固定按钮");
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
                return Err("任务栏空间不足，无法放入监控区域");
            }
            if !same_rect(task_rect, applied) && !move_child(rebar, applied) {
                return Err("无法为监控区域预留任务栏空间");
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
            return Err("任务栏监控区域定位失败");
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
        unsafe {
            self.state.cancel_drag();
        }
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
            // UpdateLayeredWindow 管理合成表面；验证重绘区，下一次 update 再提交。
            let mut ps = std::mem::zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            state.needs_paint.set(true);
            0
        }
        (WM_ERASEBKGND, _) => 1,
        (WM_MOUSEACTIVATE, _) => MA_NOACTIVATE as isize,
        (WM_LBUTTONDOWN, Some(state)) => {
            if let Some(rect) = client_rect(hwnd) {
                let (x, y) = mouse_point(lp);
                let content = state.content.borrow();
                let layout = surface::layout(
                    &content,
                    rect.right,
                    rect.bottom,
                    GetDpiForWindow(hwnd).max(96) as i32,
                );
                if let Some(group) = layout.group_at(x, y) {
                    state
                        .drag
                        .replace(Some(Drag::new(group, x, y, content.order.clone())));
                    drop(content);
                    SetCapture(hwnd);
                }
            }
            0
        }
        (WM_MOUSEMOVE, Some(state)) => {
            let (x, y) = mouse_point(lp);
            state.track_drag(x, y);
            0
        }
        (WM_LBUTTONUP, Some(state)) => {
            let (x, y) = mouse_point(lp);
            state.track_drag(x, y);
            let drag = state.drag.borrow_mut().take();
            if GetCapture() == hwnd {
                ReleaseCapture();
            }
            if let Some(drag) = drag {
                if let Some(rect) = client_rect(hwnd).filter(|r| contains(*r, x, y)) {
                    if drag.moved {
                        if drag.preview_order != drag.original_order {
                            state.content.borrow_mut().order = drag.preview_order.clone();
                            (state.reordered)(drag.preview_order);
                        }
                    } else {
                        let layout = surface::layout(
                            &state.content.borrow(),
                            rect.right,
                            rect.bottom,
                            GetDpiForWindow(hwnd).max(96) as i32,
                        );
                        if layout.group_at(x, y) == Some(drag.group) {
                            (state.click)(screen_rect(hwnd), drag.group == Group::Performance);
                        }
                    }
                }
                state.needs_paint.set(true);
                let _ = state.repaint();
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
            }
            0
        }
        (WM_CAPTURECHANGED | WM_CANCELMODE, Some(state)) => {
            state.cancel_drag();
            0
        }
        (WM_NCDESTROY, Some(state)) => {
            state.drag.replace(None);
            state.hwnd.set(null_mut());
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        (WM_DPICHANGED | WM_SIZE, Some(state)) => {
            state.cancel_drag();
            state.needs_paint.set(true);
            0
        }
        (WM_THEMECHANGED, Some(state)) => {
            state.needs_paint.set(true);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn mouse_point(lp: LPARAM) -> (i32, i32) {
    (lp as i16 as i32, (lp >> 16) as i16 as i32)
}
fn contains(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
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
    static REORDERS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn order(performance_first: bool) -> TaskbarOrder {
        TaskbarOrder::new([if performance_first {
            Group::Performance
        } else {
            Group::Speed
        }])
    }

    fn combined_content(performance_first: bool) -> Content {
        Content {
            speed: Some(Speed {
                down: "118B".into(),
                up: "144B".into(),
                colored: true,
            }),
            performance: Some(Performance {
                cpu: "3%".into(),
                gpu: "1%".into(),
                colored: true,
            }),
            light: false,
            colors: Default::default(),
            order: order(performance_first),
        }
    }

    #[test]
    fn drag_reorders_both_groups_in_both_orientations_and_can_return_or_cancel() {
        for dpi in [96, 120, 144, 168, 192] {
            for performance_first in [false, true] {
                let content = combined_content(performance_first);
                for (width, height) in [(surface::width_dip(&content), 48), (80, 72)] {
                    let area = RECT {
                        left: 0,
                        top: 0,
                        right: width * dpi / 96,
                        bottom: height * dpi / 96,
                    };
                    let layout = surface::layout(&content, area.right, area.bottom, dpi);
                    for (group, source, destination) in [
                        (
                            Group::Speed,
                            layout.rect(Group::Speed).unwrap(),
                            layout.rect(Group::Performance).unwrap(),
                        ),
                        (
                            Group::Performance,
                            layout.rect(Group::Performance).unwrap(),
                            layout.rect(Group::Speed).unwrap(),
                        ),
                    ] {
                        let start = (source.left + 3, source.top + 3);
                        let mut drag = Drag::new(group, start.0, start.1, content.order.clone());
                        let threshold = (4 * dpi / 96, 4 * dpi / 96);
                        drag.track(start.0 + 1, start.1 + 1, area, &layout, threshold);
                        assert!(!drag.moved, "minor pointer jitter must remain a click");
                        assert_eq!(drag.preview_order, content.order);
                        // 拖到另一组远端，跨过该组中点才换序。
                        let target = if (group == Group::Performance) == performance_first {
                            (destination.right - 2, destination.bottom - 2)
                        } else {
                            (destination.left + 1, destination.top + 1)
                        };
                        drag.track(target.0, target.1, area, &layout, threshold);
                        assert!(drag.moved);
                        assert_ne!(drag.preview_order, content.order);
                        // 预览期间采样数值可以刷新；排序判定始终使用原始布局，避免来回抖动。
                        drag.track(target.0, target.1, area, &layout, threshold);
                        assert_ne!(drag.preview_order, content.order);
                        drag.track(start.0, start.1, area, &layout, threshold);
                        assert_eq!(drag.preview_order, content.order);
                        assert!(drag.moved, "dragging back must not open the panel");
                        drag.track(-20, -20, area, &layout, threshold);
                        assert_eq!(drag.preview_order, content.order);
                    }
                }
            }
        }
    }

    #[test]
    fn dragging_a_single_visible_item_preserves_the_hidden_items_position() {
        for performance_first in [false, true] {
            for group in [Group::Speed, Group::Performance] {
                let mut content = combined_content(performance_first);
                match group {
                    Group::Speed => content.performance = None,
                    Group::Performance => content.speed = None,
                }
                let area = RECT {
                    left: 0,
                    top: 0,
                    right: surface::width_dip(&content),
                    bottom: 48,
                };
                let layout = surface::layout(&content, area.right, area.bottom, 96);
                let mut drag = Drag::new(group, 3, 3, content.order.clone());
                drag.track(area.right - 2, 20, area, &layout, (4, 4));
                assert!(drag.moved);
                assert_eq!(drag.preview_order, content.order);
            }
        }
    }

    /// 需要交互式 Windows 桌面；不在无桌面的 CI 中修改 Explorer 布局。
    #[test]
    #[ignore = "requires interactive Windows taskbar"]
    fn native_child_lifecycle_and_clicks() {
        CLICKS.store(0, Ordering::Relaxed);
        REORDERS.store(0, Ordering::Relaxed);
        let mut badge = TaskbarBadge::new(
            |rect, performance| {
                assert!(rect.is_some());
                CLICKS.fetch_add(if performance { 10 } else { 1 }, Ordering::Relaxed);
            },
            |order| {
                REORDERS.fetch_add(
                    if order.items()[0] == Group::Performance {
                        1
                    } else {
                        10
                    },
                    Ordering::Relaxed,
                );
            },
        );
        let content = Content {
            speed: Some(Speed {
                down: "12M".into(),
                up: "3.4K".into(),
                colored: true,
            }),
            performance: None,
            light: false,
            colors: Default::default(),
            order: TaskbarOrder::default(),
        };
        badge.update(Some(content.clone())).unwrap();
        unsafe {
            let hwnd = badge.state.hwnd.get();
            assert!(!hwnd.is_null());
            assert_eq!(GetParent(hwnd), badge.shell);
            assert_ne!(GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_CHILD, 0);
            assert_eq!(GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_POPUP, 0);
            assert_eq!(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST, 0);
            assert_ne!(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_LAYERED, 0);
            assert_eq!(
                GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT,
                0
            );
            assert_ne!(IsWindowVisible(hwnd), 0);
            let r = relative_rect(hwnd, badge.shell).unwrap();
            let band = client_rect(badge.shell).unwrap();
            assert!(r.left >= 0 && r.top >= 0 && r.right <= band.right && r.bottom <= band.bottom);
            UpdateWindow(hwnd);
            let screen = screen_rect(hwnd).unwrap();
            assert_eq!(
                WindowFromPoint(POINT {
                    x: screen.x + 3,
                    y: screen.y + 3
                }),
                hwnd
            );
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, 4 | (4 << 16));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, 4 | (4 << 16));
            assert_eq!(CLICKS.load(Ordering::Relaxed), 1);
            let mut combined = content.clone();
            combined.performance = Some(Performance {
                cpu: "100%".into(),
                gpu: " --%".into(),
                colored: true,
            });
            badge.update(Some(combined.clone())).unwrap();
            let combined_rect = client_rect(hwnd).unwrap();
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let perf = surface::layout(&combined, combined_rect.right, combined_rect.bottom, dpi)
                .rect(Group::Performance)
                .unwrap();
            let point = (perf.left + 3) as isize | (((perf.top + 3) as isize) << 16);
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, point);
            SendMessageW(hwnd, WM_LBUTTONUP, 0, point);
            assert_eq!(CLICKS.load(Ordering::Relaxed), 11);
            let coord = |x: i32, y: i32| x as u16 as isize | ((y as u16 as isize) << 16);
            // 从性能区域拖向左侧：原生窗口即时预览，松开只发送排序事件。
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, point);
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, coord(2, 4));
            assert_eq!(badge.state.display_content().order, order(true));
            combined.performance.as_mut().unwrap().cpu = "3%".into();
            badge.update(Some(combined.clone())).unwrap(); // 模拟采样刷新
            assert_eq!(badge.state.display_content().order, order(true));
            assert_eq!(badge.state.display_content().performance.unwrap().cpu, "3%");
            SendMessageW(hwnd, WM_LBUTTONUP, 0, coord(2, 4));
            assert_eq!(badge.state.content.borrow().order, order(true));
            assert_eq!(REORDERS.load(Ordering::Relaxed), 1);
            assert_eq!(CLICKS.load(Ordering::Relaxed), 11);
            combined.order = order(true);
            badge.update(Some(combined.clone())).unwrap();
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, coord(4, 4));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, coord(4, 4));
            assert_eq!(
                CLICKS.load(Ordering::Relaxed),
                21,
                "click follows the reordered content"
            );
            // 交换回来前被系统取消：不能保存预览、不能误触发点击。
            let end = coord(combined_rect.right - 2, 4);
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, coord(4, 4));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, end);
            assert_eq!(badge.state.display_content().order, order(false));
            SendMessageW(hwnd, WM_CANCELMODE, 0, 0);
            assert_eq!(badge.state.display_content().order, order(true));
            SendMessageW(hwnd, WM_LBUTTONUP, 0, end);
            assert_eq!(REORDERS.load(Ordering::Relaxed), 1);
            // 松开于窗口外亦取消。
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, coord(4, 4));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, end);
            SendMessageW(hwnd, WM_LBUTTONUP, 0, coord(-10, -10));
            assert_eq!(badge.state.display_content().order, order(true));
            assert_eq!(REORDERS.load(Ordering::Relaxed), 1);
            // 捕获丢失恢复原序。
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, coord(4, 4));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, end);
            ReleaseCapture();
            assert!(badge.state.drag.borrow().is_none());
            assert_eq!(badge.state.display_content().order, order(true));
            // 真正拖回右侧。
            SendMessageW(hwnd, WM_LBUTTONDOWN, 0, coord(4, 4));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, end);
            SendMessageW(hwnd, WM_LBUTTONUP, 0, end);
            assert_eq!(badge.state.content.borrow().order, order(false));
            assert_eq!(REORDERS.load(Ordering::Relaxed), 11);
            assert_eq!(CLICKS.load(Ordering::Relaxed), 21);
            combined.speed = None;
            badge.update(Some(combined)).unwrap();
            let only_perf = client_rect(hwnd).unwrap();
            assert!(
                only_perf.right < combined_rect.right || only_perf.bottom < combined_rect.bottom
            );
            badge.update(Some(content.clone())).unwrap();
            assert!(same_rect(
                client_rect(hwnd).unwrap(),
                RECT {
                    left: 0,
                    top: 0,
                    right: r.right - r.left,
                    bottom: r.bottom - r.top,
                }
            ));
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
