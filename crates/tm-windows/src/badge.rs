//! 给自绘的贴片窗口补窗口样式，并量出任务栏的位置。
//!
//! egui 建出来的是**普通一级窗口**：会出现在 Alt+Tab 列表里、被点到会抢走
//! 焦点。这两件事得在窗口建好之后按 Win32 的方式补上，`ViewportBuilder`
//! 没有对应的开关：
//!
//! - `WS_EX_TOOLWINDOW`：不进 Alt+Tab、不上任务栏；
//! - `WS_EX_NOACTIVATE`：点了也不激活（贴片不该抢焦点）。
//!
//! ## 分层是为了颜色键，不是为了穿透
//!
//! `WS_EX_LAYERED | WS_EX_TRANSPARENT` 是「点击穿透」最顺手的写法，但那样
//! 这个窗口**连画面都不显示**——egui 的 `with_mouse_passthrough(true)` 走的
//! 就是这条路（winit 的 `set_cursor_hittest(false)` 加的正好是这两个位），
//! 贴片一开始怎么都不显示就是它造成的。这里的 `WS_EX_LAYERED` 只用来挂
//! 颜色键（把纯黑抠成透明），不能带 `WS_EX_TRANSPARENT`。
//!
//! ## 样式必须反复补
//!
//! egui 每次给这个窗口下发 `Visible` / `WindowLevel` 之类的命令，winit 都会
//! 按自己记的 `WindowFlags` 重算一遍 `WS_EX_*`，把这里加的位覆盖掉（收一次
//! 面板再打开，贴片就变回会抢焦点的普通窗口）。所以 [`tune`] 是幂等的，
//! 调用方每帧都可以调：内部先读现有值再比，已经对了就只花一次 FindWindow。
//!
//! ## 点击
//!
//! 贴片是**颜色键**分层窗口：抠掉的像素在系统的命中测试里根本不属于它，
//! 鼠标会直接漏给下面的任务栏。`WM_NCHITTEST` 也救不回来——那一步在窗口
//! 过程之前就做完了（实测返回 `HTCLIENT` 也没用），所以只有笔画本身能点，
//! 中间大片留白点不到。
//!
//! 与其把整块贴片糊成不透明（任务栏有亚克力，任何纯色底都会露出来一块），
//! 不如自己看鼠标：一个后台线程每 12ms 读一次光标位置和左键状态，按下和
//! 抬起都落在贴片矩形里就算一次点击。这比全局鼠标钩子便宜得多（两次
//! `GetCursorPos`/`GetAsyncKeyState`，一次都不会进到系统输入链路里），
//! 也不会拖慢别处的鼠标响应。
//!
//! 顺带：颜色键抠掉的区域点下去仍然是任务栏的，所以「点贴片空白处」在
//! 视觉上依然会落到任务栏上——只是我们同时也收到了一次点击。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetClassNameW, GetCursorPos, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, LWA_COLORKEY, SetLayeredWindowAttributes, SetWindowLongW,
    SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::screen::Rect;

/// 任务栏和托盘区的位置。
pub struct Taskbar {
    /// 任务栏整条（`Shell_TrayWnd`）
    pub band: Rect,
    /// 托盘区（`TrayNotifyWnd`）。贴片贴在它**左边**——那是任务栏里
    /// 正常空着的区域，托盘图标本身是壳按顺序排的，插不进去。
    pub tray: Rect,
    /// 任务栏上**别的程序**插进来的窗口（TrafficMonitor 之类）。
    /// 这些窗口占着的横向区间不能让我们的贴片盖上去，否则两边文字叠在一起。
    pub plugins: Vec<Rect>,
}

/// 任务栏自己的子窗口类名。除此之外的子窗口都算「别的程序插进来的」。
const SYSTEM_CLASSES: &[&str] = &[
    "Windows.UI.Composition.DesktopWindowContentBridge",
    "Windows.UI.Core.CoreWindow",
    "TrayNotifyWnd",
    "TrayDummySearchControl",
    "ReBarWindow32",
    "MSTaskSwWClass",
    "MSTaskListWClass",
    "Start",
    "Shell_TrayWnd",
    "tooltips_class32",
];

/// 点一下贴片时调用的函数。参数是贴片自身的屏幕矩形（物理像素），
/// 给面板落位兜底——托盘图标被折进 Win11 的溢出区时拿不到图标位置，
/// 那时至少还能按贴片摆。
///
/// 只存裸函数指针：调用方是 `tray-app` 里的一个普通 fn（事件最终进它自己
/// 的 channel），不需要捕获环境，也就不必装箱。
static ON_CLICK: Mutex<Option<fn(Option<Rect>)>> = Mutex::new(None);

/// 贴片的窗口句柄（`0` = 还不知道）。[`tune`] 每帧刷新。
static BADGE_HWND: AtomicIsize = AtomicIsize::new(0);


/// 注册贴片的点击回调。重复调用以最后一次为准。
pub fn on_click(f: fn(Option<Rect>)) {
    if let Ok(mut slot) = ON_CLICK.lock() {
        *slot = Some(f);
    }
}

/// 光标在贴片里时的轮询间隔。12ms 比一个 120Hz 的帧还短，正常点击
/// 不会漏；80 次/秒的 `GetCursorPos` 只是两次 syscall，开销可以忽略。
const TICK_FAST: Duration = Duration::from_millis(12);
/// 光标不在贴片附近：慢一点，只求及时发现它过来
const TICK_IDLE: Duration = Duration::from_millis(40);
/// 贴片周围这一圈也算「快速区」。
///
/// 鼠标是**移动**进来的：等光标真进了矩形才提速就晚了 —— 一次 30ms 的
/// 快速点击可能整个落在两次慢轮询之间，按下和抬起都没看见。留一圈余量，
/// 手往贴片这边挪的时候已经在用 12ms 采样了。
const NEAR: i32 = 40;
/// 按住超过这么久就不算点击（按住拖窗口/框选时路过贴片不该被当成点它）
const LONG_PRESS: Duration = Duration::from_millis(900);

/// 起一个线程专门判断「鼠标在贴片里点了一下」。幂等。
///
/// 为什么不用鼠标消息：贴片是颜色键分层窗口，抠掉的像素在系统命中测试里
/// 不属于它（见模块注释「点击」），`WM_LBUTTONDOWN` 根本不会来。
pub fn start_click_poller() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let mut was_down = false;
            // 按下时记一笔（时间 + 当时的矩形）：抬起要还在这块地方才算
            let mut armed: Option<(Instant, Rect)> = None;

            loop {
                // 位置和「该不该响应」都问窗口自己：子 viewport 的落位是它
                // 自校正的，`update()` 空闲时只有 1Hz，从主循环传状态会慢
                // 一整拍（实测刚启动那一下点不中）。不可见 = 用户关了贴片，
                // 或者这一帧本来就没该显示。
                let rect = {
                    let h = BADGE_HWND.load(Ordering::Relaxed) as HWND;
                    if h.is_null() || unsafe { IsWindowVisible(h) } == 0 {
                        None
                    } else {
                        unsafe { rect_of(h) }
                    }
                };
                let (x, y) = cursor_pos();
                let down = left_down();
                let inside = rect.is_some_and(|r| {
                    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
                });
                let near = rect.is_some_and(|r| {
                    x >= r.x - NEAR
                        && x < r.x + r.w + NEAR
                        && y >= r.y - NEAR
                        && y < r.y + r.h + NEAR
                });

                if down && !was_down {
                    armed = if inside {
                        rect.map(|r| (Instant::now(), r))
                    } else {
                        None
                    };
                }
                // 按住之后挪出贴片（拖选、拖窗口）就当没点过
                if down && !inside {
                    armed = None;
                }
                if !down && was_down {
                    if let Some((at, r)) = armed.take() {
                        if inside && at.elapsed() < LONG_PRESS {
                            if let Ok(slot) = ON_CLICK.lock() {
                                if let Some(f) = *slot {
                                    f(Some(r));
                                }
                            }
                        }
                    }
                }
                was_down = down;

                std::thread::sleep(if near || down { TICK_FAST } else { TICK_IDLE });
            }
        });
    });
}

fn cursor_pos() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

fn left_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000 != 0 }
}

/// 量任务栏和托盘区。拿不到（explorer 正在重启、任务栏被换成第三方实现）
/// 就返回 `None`，调用方退回「贴着托盘图标」的位置。
pub fn taskbar() -> Option<Taskbar> {
    unsafe {
        let class = wide("Shell_TrayWnd");
        let shell = FindWindowW(class.as_ptr(), std::ptr::null());
        if shell.is_null() {
            return None;
        }
        let band = rect_of(shell)?;
        // Win10 和 Win11 都有这个子窗口，名字一直没变
        let tray_class = wide("TrayNotifyWnd");
        let tray = FindWindowExW(
            shell,
            std::ptr::null_mut(),
            tray_class.as_ptr(),
            std::ptr::null(),
        );
        let tray = if tray.is_null() { band } else { rect_of(tray)? };

        let mut plugins = Vec::new();
        let mut child = FindWindowExW(shell, std::ptr::null_mut(), std::ptr::null(), std::ptr::null());
        let mut guard = 0;
        while !child.is_null() && guard < 40 {
            guard += 1;
            let mut name = [0u16; 96];
            let n = GetClassNameW(child, name.as_mut_ptr(), name.len() as i32);
            let class = String::from_utf16_lossy(&name[..n.max(0) as usize]);
            if !SYSTEM_CLASSES.contains(&class.as_str()) {
                if let Some(r) = rect_of(child) {
                    if r.w > 8 && r.h > 8 {
                        plugins.push(r);
                    }
                }
            }
            child = FindWindowExW(shell, child, std::ptr::null(), std::ptr::null());
        }

        Some(Taskbar {
            band,
            tray,
            plugins,
        })
    }
}

unsafe fn rect_of(hwnd: HWND) -> Option<Rect> {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut r) } == 0 {
        return None;
    }
    Some(Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    })
}

/// 把指定标题的窗口改成「工具窗口 + 不可激活」。
///
/// 找不到窗口（还没建出来）或那个窗口不属于本进程时返回 `false`，
/// 调用方下一帧再试——贴片窗口是子 viewport 第一次渲染时才建的，
/// 起手那几帧本来就找不到。
pub fn tune(title: &str) -> bool {
    let name = wide(title);
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), name.as_ptr());
        if hwnd.is_null() {
            return false;
        }
        // 标题是全局可见的：别的程序完全可能也叫这个名字。只动自己的窗口。
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != GetCurrentProcessId() {
            return false;
        }

        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let want = ex
            | WS_EX_TOOLWINDOW as i32
            | WS_EX_NOACTIVATE as i32
            | WS_EX_LAYERED as i32;
        let mut flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        if want != ex {
            SetWindowLongW(hwnd, GWL_EXSTYLE, want);
            // 扩展样式改完要让系统重算一次非客户区，否则新样式要等下次
            // 尺寸变化才生效（贴片尺寸一直不变，等于永远不生效）。
            flags |= SWP_FRAMECHANGED;
        }
        // **每次都重新申明 TOPMOST**。
        //
        // 任务栏自己也是 TOPMOST，而且它每次重绘（时间跳一分钟、托盘图标变化）
        // 都会把自己重新提到最前，我们就被压到它下面 —— 表现是贴片毫无征兆地
        // 消失，过一会儿又出现。SWP_NOACTIVATE 保证这一下不会抢焦点。
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);

        // 记下句柄，让轮询线程自己去查矩形（它 12ms 一次，比这里的
        // 1Hz 心跳跟得紧）
        BADGE_HWND.store(hwnd as isize, Ordering::Relaxed);

        // 纯黑抠成透明。
        //
        // 贴片窗口是 GL 画的，而 eframe 拿到的 GL 配置**没有 alpha 通道**
        // （启动日志里那句 `Cannot create transparent window: the GL config
        // does not support it`），所以 glClear 的 alpha 被丢掉，窗口底色
        // 永远是黑 —— 不处理的话它在任务栏上就是一块黑方块。
        //
        // 逐像素 alpha（`UpdateLayeredWindow`）轮不到我们用，能用的只有
        // 颜色键：把纯黑整片抠掉，画在上面的字反而是原样。抗锯齿的过渡像素
        // 是「文字色×覆盖率」，不是纯黑，所以不会被误抠，只是描边会偏深一档
        // —— 在深色任务栏上看不出来，浅色任务栏上我们的字也是深色，同样不显。
        //
        // 顺带：颜色键抠掉的区域鼠标也会漏下去，贴片不挡住任务栏的点击。
        // 落在这块地方的点击由 [`start_click_poller`] 另外认（系统的命中
        // 测试不认，只能自己看鼠标）。
        SetLayeredWindowAttributes(hwnd, 0, 0, LWA_COLORKEY);
        true
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
