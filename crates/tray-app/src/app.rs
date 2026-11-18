//! 托盘弹窗主体：状态栏图标 + 无边框弹出面板。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};
use tm_core::{format_bytes, format_speed, pretty_name, AppRate};
// Windows 仍然用 tray-icon；macOS 换成原生 NSStatusItem（见 tm_macos::StatusItem 的注释）
#[cfg(not(target_os = "macos"))]
use tray_icon::{Icon, TrayIconBuilder};

/// 状态栏项的平台类型
#[cfg(target_os = "macos")]
type TrayHandle = tm_macos::StatusItem;
#[cfg(not(target_os = "macos"))]
type TrayHandle = tray_icon::TrayIcon;

use crate::autostart;
use crate::engine::{self, Snapshot};
use crate::icon;
use crate::icons::{self, IconSet};
use crate::settings::{LangMode, Settings, ThemeMode};
#[cfg(target_os = "macos")]
use crate::settings::TrayMode;
use crate::theme::{self, Lang, Palette};

pub const WINDOW_W: f32 = 352.0;
/// 面板高度，**固定值**。
///
/// 以前高度是逐帧量内容算出来的，结果：切到设置页会缩一截、应用数一变窗口就抽动。
/// 一个菜单栏弹窗本来就不该自己改尺寸 —— 固定之后，列表装不下就滚动，
/// 设置页底部留白，切页/换数据时窗口一动不动。
///
/// 411 是照着「列表正好 7 行」调的：上面 chrome 约 138pt、页脚带 33pt、
/// 列表 7×31−5 = 212pt、面板内边距 28pt。
///
/// 再矮下去设置页就装不下了（7 个设置行 + 开机自启约 402pt），所以 7 行
/// 是「列表尽量少占、设置页又几乎不留白」的交点。列表行数仍然是运行时按可用高度算的
/// （并向下取整到整行），所以以后哪块高度变了也只是少显示一行，不会露出半截。
pub const PANEL_H: f32 = 411.0;

/// 「收起」时窗口停靠的位置（逻辑点）。
///
/// Windows 上不能用 `Visible(false)` 收面板（会让 eframe 的重绘循环停摆，
/// 托盘点击就再也唤不醒了，见 [`App::park_window`]），只能把窗口挪到屏幕外。
/// 取 -32000 是 Win32 的老办法：超出任何虚拟桌面边界，系统不会绘制它。
#[cfg(windows)]
const PARKED_X: f32 = -32000.0;
#[cfg(windows)]
const PARKED_Y: f32 = -32000.0;

/// 列表下方固定区的高度（点）。
///
/// 这里**只含页脚**：提醒行是可选的（真的超阈值才有），提示则画在页脚里。
/// 以前把 22+18 两段也预留在下面，于是没有提醒时列表底下永远空着 40pt，
/// 看着像滚动区没铺满——现在列表一路铺到页脚线。
const FOOTER_H: f32 = 20.0;
/// 页脚带 = 空隙 + 分隔线 + 空隙 + 页脚
const FOOTER_BAND: f32 = 8.0 + 1.0 + 4.0 + FOOTER_H;
/// 近 10 分钟超量提醒行的高度（只在真的有提醒时占位）
const ALERT_H: f32 = 24.0;
/// 设置页里的临时提示行高度
const TOAST_H: f32 = 18.0;

/// 每一行的「盒子」高度（含 1px 描边）。
///
/// 26 是 18pt 图标的上下各留 4pt：再矮描边就会贴到图标，再高整列就发虚。
const ROW_BOX_H: f32 = 26.0;
/// 行与行之间的外边距。盒子本身有描边，间距不能太小，否则像一条表格。
const ROW_GAP: f32 = 5.0;
/// 列表里每一行占的高度 = 盒子 + 外边距
const ROW_H: f32 = ROW_BOX_H + ROW_GAP;
/// 行盒的圆角。比面板的 14 小一档，形成「面板 ⊃ 行」的层级
const ROW_RADIUS: u8 = 7;
/// 行盒左右内边距：让内容离描边有一个稳定的距离
const ROW_PAD_X: f32 = 8.0;
/// 行内「下行速率」列的固定宽度。数字列必须是定宽的：不定宽时一行显示
/// `0`、另一行显示 `1.2 MB/s`，整列数字会左右跳，扫一眼对不齐。
const NUM_W_RX: f32 = 74.0;
/// 行内「上行速率」列的固定宽度（字小一档，所以窄一些）
const NUM_W_TX: f32 = 62.0;
/// 单位（`KB/s`、`MB/s`）占的固定宽度。
///
/// 必须定宽：单位宽度随 K/M 变化，若让数值贴着自己的单位右对齐，各行的
/// 数字就会差一两像素，竖着扫时小数点对不齐。
const NUM_UNIT_W: f32 = 23.0;
/// 两个数字列之间的空隙
const NUM_GAP: f32 = 6.0;
/// 右侧给滚动指示条留的过道。行盒不占这一条，指示条才不会压在描边上。
const SCROLL_GUTTER: f32 = 9.0;
/// 指示条宽度（自绘，不用 egui 自带的浮动把手）
const SCROLL_BAR_W: f32 = 3.0;
/// 应用名最多占用多少显示宽度单位（中日韩字符算 2，字母数字算 1）
const NAME_UNITS: usize = 22;
/// 顶部两个大数字之间留给竖分隔线的宽度
const METRIC_DIVIDER_W: f32 = 22.0;
/// 时间窗口分段控件的宽度。四档等宽，"1分钟" 也放得下。
const SEG_W: f32 = 196.0;
/// 分段控件高度
const SEG_H: f32 = 22.0;
/// 失焦后多久才真正收起。托盘点击会先让窗口失焦再派发点击事件，
/// 没有这个去抖就会「刚打开又立刻关上」。
const HIDE_DEBOUNCE: Duration = Duration::from_millis(320);
/// 底部提示条显示多久
const TOAST_TTL: Duration = Duration::from_secs(6);

/// 时间窗口。界面上的分段控件就是它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Live,
    Min1,
    Min5,
    Hour1,
}

/// 面板当前显示哪一页。设置做成同一个窗口里的第二页，
/// 而不是另开窗口：另开一个 NSPanel 要重新处理锚定、失焦收起、层级，
/// 而这几个坑我们已经在主面板上踩过一遍了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Main,
    Settings,
}

impl Window {
    pub fn all() -> [Window; 4] {
        [Window::Live, Window::Min1, Window::Min5, Window::Hour1]
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Window::Live => lang.t("实时", "Live"),
            Window::Min1 => lang.t("1分钟", "1m"),
            Window::Min5 => lang.t("5分钟", "5m"),
            Window::Hour1 => lang.t("1小时", "1h"),
        }
    }
}

/// 状态栏图标在屏幕上的位置（物理像素）。
#[derive(Debug, Clone, Copy)]
pub struct IconRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug)]
enum TrayEvent {
    /// 用户点了状态栏图标。位置在收到事件后再查——原生 NSStatusItem
    /// 的点击回调不带 rect，而且每次展开前重新取一次更准（状态栏会重排）。
    ///
    /// `fallback` 是事件自带的矩形，只有 Windows 有。Win11 把新图标收进
    /// `«` 溢出区时，`Shell_NotifyIconGetRect` 会失败，那时就用点击事件
    /// 里带的位置兜底，否则点了完全没反应。
    Click { fallback: Option<IconRect> },
    /// 用户点了任务栏上的网速贴片（Windows 专属）。
    ///
    /// 和托盘点击分开记，是因为**锚点不一样**：贴片在任务栏条里，和托盘
    /// 图标之间可能隔着一整个第三方插件（TrafficMonitor 那种）。按托盘
    /// 图标摆的话，点最左边的数字、面板却从右边托盘上方冒出来，看着像点
    /// 错了东西。这里直接以被点的贴片为锚点——点哪儿弹哪儿。
    #[cfg(windows)]
    BadgeClick { at: IconRect },
}

// tray-icon 只允许注册一个全局回调，所以用全局 channel 把事件转回 App。
static TRAY_TX: OnceLock<Sender<TrayEvent>> = OnceLock::new();
static CTX: OnceLock<egui::Context> = OnceLock::new();

pub struct App {
    rx: Receiver<Snapshot>,
    tray_rx: Receiver<TrayEvent>,
    latest: Option<Snapshot>,
    visible: bool,
    shown_at: Instant,
    auto_hidden_at: Option<Instant>,
    tray: Option<TrayHandle>,
    /// 上一次写进状态栏的「模式 + 文字」。只比文字不够：切到「仅图标」时
    /// 文字本来就是空串，会被当成没变化而跳过一次必要的重排。
    tray_title: String,
    /// 上一次写进悬停提示的文字。两个平台共用 —— Windows 没有菜单栏文字，
    /// 悬停提示是那里唯一能常看到实时速率的地方（不带面板）。
    tray_tip: String,
    lang: Lang,
    /// 状态栏图标延迟到事件循环转起来之后再建。
    ///
    /// 依据：原生 `NSStatusBar.statusItem` 刚创建时 `button.frame` 已经是 22pt 高，
    /// 但它所在窗口的 frame 高度是 0，要等状态栏布局后才正常。早建虽然不报错，
    /// 但拿到的 rect 是无效的（弹窗会定错位置），所以等一会儿再建更稳。
    tray_build_at: Instant,
    /// 当前选中的时间窗口
    window: Window,
    /// 应用图标纹理缓存（后台加载，见 [`IconSet`]）
    icons: IconSet,
    /// 是否已设置开机自启
    launch_at_login: bool,
    /// 用户设置（`settings.conf`）
    settings: Settings,
    /// 采样线程读取的保留天数，改设置后即时生效
    retention: Arc<AtomicI64>,
    /// 只由字体可用性决定的界面语言。`settings.lang` 为 Auto 时用它。
    font_lang: Lang,
    /// 当前是主面板还是设置页
    view: View,
    /// 底部临时提示（导出结果、自启切换结果），`(文案, 起始时间)`
    toast: Option<(String, Instant)>,
    /// 列表滚动位置（点）。自己存一份是为了在滚动停下后吸附到整行：
    /// 触控板松手时偏移量常常停在半行上，视口上下就会各露半截行。
    list_scroll: f32,
    /// 调试用：启动后立刻把面板摆出来（`--show` / `TM_SHOW_ON_START`）
    show_on_start: bool,
    /// 展开时刻，用于开头那段淡入 + 逐行浮现。
    ///
    /// 为什么不用 `Context::animate_bool_with_time`：那个按归一化进度推进，
    /// 做逐行错峰时最后几行永远到不了 1，会一直半透明。这里直接用真实经过时间算。
    opened_at: Option<Instant>,
    /// 本次展开后窗口是否真的拿到过焦点。
    ///
    /// Windows 的 `SetForegroundWindow` 受前台窗口规则限制，托盘点击之后
    /// 未必能把焦点抢过来。这时不能按「失焦就收起」处理，否则面板一打开
    /// 就自己关掉，看起来像"点了没反应"。
    had_focus: bool,
    /// 状态栏图标当前是否挂着。切到「仅网速」时要摘掉，摘/挂都得有状态，
    /// 否则每帧都重建一次 NSImage。（Windows 没有这个档位）
    #[cfg(target_os = "macos")]
    tray_icon_on: bool,
    /// 任务栏是不是浅色。Windows 的托盘图标不参与 macOS 那种模板反色，
    /// 主题一变就得重画一张，否则深色任务栏上的黑图标等于隐形。
    #[cfg(windows)]
    taskbar_light: bool,
    /// 上次检查系统主题的时间。注册表读一次是微秒级，但也没必要每帧读。
    #[cfg(windows)]
    theme_check_at: Instant,
    /// 任务栏上方的网速贴片（Windows 专属，见 [`crate::badge_window`]）
    #[cfg(windows)]
    badge: crate::badge_window::Badge,
    /// 第几帧（只给 `TM_TRACE_FRAMES` 诊断用）
    frame_no: u64,
    /// 首帧是否已经做过「可见但停在屏幕外」的兜底设置。
    ///
    /// Windows 上窗口不能真隐藏（隐藏后 eframe 的 update 会停摆），
    /// 所以启动时必须显式把它摆到屏幕外去。
    #[cfg(windows)]
    startup_parked: bool,
    /// 还需要强制隐藏几帧。
    ///
    /// eframe 在首帧画完后会**无条件**调 `window.set_visible(true)`
    /// （`eframe::native::epi_integration::post_rendering`），把
    /// `ViewportBuilder::with_visible(false)` 直接覆盖掉 —— 表现就是
    /// 「一启动面板自己弹出来」。它这么做是为了避免启动白闪，但没提供开关。
    ///
    /// 这里在前几帧再明确隐藏一次：ViewportCommand 的处理（handle_viewport_output）
    /// 与 post_rendering 在同一帧内先后执行，所以窗口不会被合成到屏幕上。
    /// 留 3 帧是冗余，防止第一帧的命令赶上首帧初始化还没完成。
    ///
    /// 只有 macOS 用得上：Windows 走「挪到屏幕外」，不需要也不允许隐藏窗口。
    #[cfg(not(windows))]
    startup_hide: u8,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let _ = CTX.set(ctx.clone());

        let settings = Settings::load();
        ctx.set_theme(theme_pref(settings.theme));

        // 滚动条改成悬浮细条：不占内容宽度，列表右侧的速率数字不会在
        // 滚动条出现/消失时横向跳动。
        let mut style = (*ctx.style()).clone();
        style.spacing.scroll.floating = true;
        style.spacing.scroll.floating_width = 6.0;
        style.spacing.scroll.floating_allocated_width = 0.0;
        // 默认的浮动滚动条在空闲时完全不透明度过 0，等于看不见 ——
        // 那样「下面还有内容」这个信息就丢了。留一个浅色的常驻把手。
        // 把手默认用 widgets.inactive.bg_fill，那是深灰，在深色面板上等于隐形。
        // 换成前景色：深色主题是浅灰、浅色主题是深灰，两边都看得见。
        style.spacing.scroll.foreground_color = true;
        style.spacing.scroll.dormant_handle_opacity = 0.55;
        style.spacing.scroll.active_handle_opacity = 0.75;
        style.spacing.scroll.interact_handle_opacity = 1.0;
        style.spacing.scroll.dormant_background_opacity = 0.0;
        ctx.set_style(style);

        let font_lang = match theme::install_cjk(&ctx) {
            Some(path) => {
                eprintln!("[ui] 已加载中文字体：{path}");
                Lang::Zh
            }
            None => {
                eprintln!("[ui] 没找到可用的中文字体，界面退回英文");
                Lang::En
            }
        };
        let lang = resolve_lang(settings.lang, font_lang);

        // 保留天数从设置里来，改完立刻对采样线程生效
        let retention = Arc::new(AtomicI64::new(settings.retention_days));

        // 采样线程每出一帧就唤醒一次界面
        let rx = engine::spawn(
            {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            },
            retention.clone(),
        );

        let (tray_tx, tray_rx) = mpsc::channel();
        let _ = TRAY_TX.set(tray_tx);
        // macOS 用原生状态项，点击回调在创建时就注册好了；只有走 tray-icon 的平台需要全局 handler
        #[cfg(not(target_os = "macos"))]
        install_tray_handler();

        let icons = IconSet::new(ctx.clone());

        let mut app = Self {
            rx,
            tray_rx,
            latest: None,
            visible: false,
            shown_at: Instant::now(),
            auto_hidden_at: None,
            tray: None,
            tray_title: String::new(),
            tray_tip: String::new(),
            lang,
            // 调试时可以固定某个窗口，方便截图核对
            window: match std::env::var("TM_WINDOW").unwrap_or_default().as_str() {
                "1m" => Window::Min1,
                "5m" => Window::Min5,
                "1h" => Window::Hour1,
                _ => Window::Live,
            },
            // macOS 要等状态栏布局好再建（见字段注释）；Windows 的 Shell_NotifyIcon 可以立刻建
            #[cfg(target_os = "macos")]
            tray_build_at: Instant::now() + Duration::from_millis(600),
            #[cfg(not(target_os = "macos"))]
            tray_build_at: Instant::now(),
            icons,
            launch_at_login: autostart::is_enabled(),
            font_lang,
            view: if std::env::var("TM_VIEW").map(|v| v == "settings").unwrap_or(false) {
                View::Settings
            } else {
                View::Main
            },
            settings,
            retention,
            toast: None,
            // 调试用：TM_SCROLL=140 给列表一个初始位置，方便截图核对滚动后的样子
            list_scroll: std::env::var("TM_SCROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            show_on_start: std::env::var_os("TM_SHOW_ON_START").is_some(),
            // macOS 靠「前几帧再隐藏」抵消 eframe 的强制显示；Windows 改成摆到屏幕外
            #[cfg(not(windows))]
            startup_hide: 3,
            frame_no: 0,
            #[cfg(windows)]
            startup_parked: false,
            opened_at: None,
            had_focus: false,
            // build_tray 里已经挂了图标，这里按「已挂」起步
            #[cfg(target_os = "macos")]
            tray_icon_on: true,
            #[cfg(windows)]
            taskbar_light: tm_windows::theme::taskbar_is_light(),
            #[cfg(windows)]
            theme_check_at: Instant::now(),
            #[cfg(windows)]
            badge: crate::badge_window::Badge::new(),
        };

        // 隐藏窗口不会驱动 eframe 的 update()，托盘图标必须在这里同步建好：
        // 否则用户根本看不到图标，也就无从点开面板。
        #[cfg(windows)]
        app.ensure_tray();

        app
    }

    /// 延迟创建状态栏图标，失败则过几秒重试。
    fn ensure_tray(&mut self) {
        if self.tray.is_some() || Instant::now() < self.tray_build_at {
            return;
        }
        match build_tray() {
            Ok(t) => self.tray = Some(t),
            Err(e) => {
                eprintln!("[ui] 状态栏图标创建失败：{e}");
                self.tray_build_at = Instant::now() + Duration::from_secs(5);
            }
        }
    }

    /// 任务栏明暗切换后重画托盘图标。
    ///
    /// Windows 不会替我们把图标反色（那是 macOS 模板图的行为），所以用户
    /// 白天切到浅色主题、晚上切回深色之后，原来的白线会变成一块看不见的白斑。
    /// 每 3 秒读一次注册表，开销可以忽略。
    #[cfg(windows)]
    fn sync_tray_theme(&mut self) {
        if self.tray.is_none() || self.theme_check_at.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.theme_check_at = Instant::now();

        let light = tm_windows::theme::taskbar_is_light();
        if light == self.taskbar_light {
            return;
        }
        self.taskbar_light = light;

        let (rgba, w, h) = icon::waveform_tinted(TRAY_ICON_PX, tray_tint());
        if let Ok(img) = Icon::from_rgba(rgba, w, h) {
            if let Some(tray) = &self.tray {
                let _ = tray.set_icon(Some(img));
            }
        }
    }

    pub fn open(&mut self, ctx: &egui::Context, rect: IconRect) {
        // 高度固定，所以落位只算一次：先摆位置再显示，两个命令在同一帧里
        // 按顺序执行，面板不会先在默认位置闪一下再跳过去。
        let place = self.placement_for(ctx, rect);
        let pos = place.top_left(PANEL_H, ctx.pixels_per_point());
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));

        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.visible = true;
        self.shown_at = Instant::now();
        self.auto_hidden_at = None;
        self.opened_at = Some(Instant::now());
        // 焦点是「拿到过」才作数，见字段注释
        self.had_focus = false;
    }

    /// 算出这次该把面板摆哪儿。
    ///
    /// 三条规则：
    /// 1. 方向看图标在屏幕上的位置——图标在**下半屏**说明任务栏在底部，
    ///    面板要往上长（Windows 常态）；在上半屏说明是菜单栏，往下长（macOS）。
    /// 2. 工作区取**图标所在那块显示器**的，不能用主屏尺寸：副屏、竖屏、
    ///    高 DPI 屏下按主屏算会整体偏出去。
    /// 3. 横向在屏幕内居中于图标，贴边时朝里夹，不让面板出屏。
    fn placement_for(&self, ctx: &egui::Context, icon: IconRect) -> Placement {
        let ppp = ctx.pixels_per_point().max(1.0);

        // 兜底：拿不到显示器信息时给一个足够大的范围，等于「按方向摆、不夹边」
        let huge = ScreenBox {
            x: -1.0e6,
            y: 0.0,
            w: 2.0e6,
            h: 2.0e6,
        };
        let screen = ctx
            .input(|i| i.viewport().monitor_size)
            .map(|m| ScreenBox {
                x: 0.0,
                y: 0.0,
                w: m.x * ppp,
                h: m.y * ppp,
            })
            .unwrap_or(huge);

        #[cfg(windows)]
        let work = {
            let cx = (icon.x + icon.w / 2.0).round() as i32;
            let cy = (icon.y + icon.h / 2.0).round() as i32;
            tm_windows::screen::work_area_at(cx, cy)
                .map(|r| ScreenBox {
                    x: r.x as f32,
                    y: r.y as f32,
                    w: r.w as f32,
                    h: r.h as f32,
                })
                .unwrap_or(screen)
        };
        #[cfg(not(windows))]
        let work = screen;

        compute_placement(icon, work, ppp)
    }

    pub fn close(&mut self, ctx: &egui::Context) {
        self.park_window(ctx);
    }

    /// 收起面板。
    ///
    /// Windows 上**不能**用 `ViewportCommand::Visible(false)`：winit 收到之后
    /// 就不再派发 `RedrawRequested`，eframe 的 `update()` 随之停摆 —— 托盘点击
    /// 虽然被 handler 塞进了 channel（还顺手 `request_repaint()`），但再也没人
    /// 消费，表现就是「点图标完全没反应」，只能重启程序。
    ///
    /// 所以改成把窗口挪到屏幕外：`WS_VISIBLE` 保持为真，事件循环照常转，
    /// 面板状态、悬停提示也能继续刷新。逻辑上仍然是「收起」（`visible = false`），
    /// 展开时再把窗口搬回来。
    #[cfg(windows)]
    fn park_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            PARKED_X, PARKED_Y,
        )));
        self.visible = false;
    }

    /// 收起面板。
    ///
    /// macOS 的状态项窗口本来就不参与普通窗口管理，直接隐藏最干净，
    /// 原生状态栏的点击回调也不依赖 eframe 的重绘循环。
    #[cfg(not(windows))]
    fn park_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.visible = false;
    }

    fn toggle(&mut self, ctx: &egui::Context, rect: IconRect) {
        // 这一次点击本身可能刚刚触发过失焦收起，那就不要再打开
        if let Some(t) = self.auto_hidden_at.take() {
            if t.elapsed() < HIDE_DEBOUNCE {
                return;
            }
        }
        if self.visible {
            self.close(ctx);
        } else {
            self.open(ctx, rect);
        }
    }

    fn drain_engine(&mut self) {
        while let Ok(snap) = self.rx.try_recv() {
            self.latest = Some(snap);
        }
    }

    /// 状态栏上的内容：悬停提示两个平台都有，文字标题只有 macOS 有。
    fn sync_tray_title(&mut self) {
        let Some(tray) = &self.tray else { return };

        // 悬停提示必须每秒跟着数据走。Windows 的托盘没有文字标题，
        // 这是面板之外唯一能看到实时速率的地方 —— 以前它被 macOS 专用的
        // 那段逻辑一起跳过了，悬停永远只有一句产品名。
        let tip = match &self.latest {
            Some(s) => format!(
                "↓ {}    ↑ {}",
                format_speed(s.total_rx),
                format_speed(s.total_tx)
            ),
            None => "Tramoing".to_owned(),
        };
        if tip != self.tray_tip {
            let _ = tray.set_tooltip(Some(tip.as_str()));
            self.tray_tip = tip;
        }

        #[cfg(target_os = "macos")]
        {
            let mode = self.settings.tray_mode;

            // 图标：只有「仅网速」这档要摘掉。切换才动 AppKit，
            // 否则每秒重建一次 NSImage 纯属浪费。
            let want_icon = !matches!(mode, TrayMode::SpeedOnly);
            if want_icon != self.tray_icon_on {
                if want_icon {
                    let (rgba, w, h) = icon::waveform();
                    tray.set_icon(&rgba, w, h);
                } else {
                    tray.clear_icon();
                }
                self.tray_icon_on = want_icon;
            }

            let (rx, tx) = self
                .latest
                .as_ref()
                .map(|s| (s.total_rx, s.total_tx))
                .unwrap_or((0.0, 0.0));
            let (up, down) = (menu_speed(tx), menu_speed(rx));

            // 缓存键里必须带模式：同样的数字在「仅图标」和「都展示」下宽度天差地别，
            // 只比文字的话「切到仅图标」这一下会被当成没变化而跳过。
            let text = match mode {
                TrayMode::IconOnly => String::new(),
                _ => format!("↑{up}\n↓{down}"),
            };
            let key = format!("{mode:?}|{text}");
            if key != self.tray_title {
                if text.is_empty() {
                    tray.clear_title();
                } else {
                    tray.set_speed_lines(&tm_macos::SpeedLines {
                        up: &up,
                        down: &down,
                        colored: self.settings.tray_color,
                    });
                }
                self.tray_title = key;

                // 怀疑「状态栏没出现」时开 TM_TRACE：这里能看到它到底被摆在哪、
                // 是否已经被系统收进 `«` 折叠区（rect 为 None 或 x 越界）。
                if std::env::var_os("TM_TRACE").is_some() {
                    eprintln!(
                        "[tray] rect={:?} visible={} {} title={:?}",
                        tray.icon_rect(),
                        tray.is_visible(),
                        tray.debug_state(),
                        text
                    );
                    // TM_TRAY_PNG=/tmp/x.png 把按钮离屏渲染出来，折叠区里也能核对排版
                    if let Ok(path) = std::env::var("TM_TRAY_PNG") {
                        eprintln!("[tray] dump={}", tray.dump_png(&path));
                    }
                }

                // 悬停提示在上面统一处理了，这里只管文字
            }
        }
    }

    /// 把最新的总速率推给任务栏贴片。
    ///
    /// 贴片现在嵌在任务栏条里，和面板（从任务栏上方弹出来）不重叠，
    /// 所以面板展开时不必让它让位——一直显示反而更像任务栏自己的东西。
    #[cfg(windows)]
    fn sync_badge(&mut self, ctx: &egui::Context) {
        use crate::badge_window::BadgeState;

        let state = match (&self.latest, self.tray_icon_rect(ctx)) {
            (Some(snap), Some(icon))
                if self.settings.tray_badge && icon.w >= 1.0 && icon.h >= 1.0 =>
            {
                Some(BadgeState {
                    icon,
                    rx: snap.total_rx,
                    tx: snap.total_tx,
                    light: self.taskbar_light,
                    colored: self.settings.tray_color,
                })
            }
            // 还没第一帧数据、图标被 Win11 收进溢出区、用户关了贴片
            _ => None,
        };
        self.badge.show(ctx, state);
    }

    /// 状态栏图标在屏幕上的位置，单位是物理像素（`compute_placement` 的约定）。
    fn tray_icon_rect(&self, ctx: &egui::Context) -> Option<IconRect> {
        let tray = self.tray.as_ref()?;

        #[cfg(target_os = "macos")]
        {
            // AppKit 给的是「点」，乘上缩放比才是物理像素
            let (x, y, w, h) = tray.icon_rect()?;
            let ppp = ctx.pixels_per_point().max(1.0);
            Some(IconRect {
                x: x as f32 * ppp,
                y: y as f32 * ppp,
                w: w as f32 * ppp,
                h: h as f32 * ppp,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            let r = tray.rect()?;
            Some(IconRect {
                x: r.position.x as f32,
                y: r.position.y as f32,
                w: r.size.width as f32,
                h: r.size.height as f32,
            })
        }
    }

    fn session(&self) -> (u64, u64) {
        self.latest
            .as_ref()
            .map(|s| (s.session_rx, s.session_tx))
            .unwrap_or((0, 0))
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 全透明，圆角之外的区域才不会出现一块底色
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 排查「点了托盘没反应」用的：确认收起之后 update 还在跑。
        // 收缩面板如果把窗口真隐藏了，这个计数就会停在收起的那一刻。
        if std::env::var_os("TM_TRACE_FRAMES").is_some() {
            self.frame_no = self.frame_no.wrapping_add(1);
            if self.frame_no % 30 == 0 {
                eprintln!(
                    "[ui] frame={} visible={} pos={:?}",
                    self.frame_no,
                    self.visible,
                    ctx.input(|i| i.viewport().outer_rect).map(|r| r.min)
                );
            }
        }
        self.ensure_tray();
        #[cfg(windows)]
        self.sync_tray_theme();
        self.drain_engine();
        self.sync_tray_title();
        #[cfg(windows)]
        self.sync_badge(ctx);
        self.icons.poll();

        // 调试模式：摆到屏幕右上角，方便截图核对排版
        if self.show_on_start && !self.visible {
            let ppp = ctx.pixels_per_point().max(1.0);
            let (w, _h) = ctx
                .input(|i| i.viewport().monitor_size)
                .map(|m| (m.x, m.y))
                .unwrap_or((1440.0, 900.0));
            // 系统通知横幅会盖住顶部，截图时可以用 TM_PANEL_Y 把面板往下挪
            let y = std::env::var("TM_PANEL_Y")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0);
            // open() 内部会把物理像素除以 ppp，这里先乘回去，两边单位才能对上
            self.open(
                ctx,
                IconRect {
                    x: (w - 90.0) * ppp,
                    y: y * ppp,
                    w: 40.0 * ppp,
                    h: 24.0 * ppp,
                },
            );
        }

        // 抵消 eframe 首帧的强制显示（见字段注释）。
        // 必须在 show_on_start 之后判断，否则 --show 会被自己关掉。
        //
        // 只在 macOS 走这条路：Windows 的窗口一直保持 WS_VISIBLE、靠挪到屏幕外
        // 来「收起」，一旦这里发 Visible(false) 就会把 update() 弄停摆，
        // 之后点托盘图标不会有任何反应。
        #[cfg(not(windows))]
        if !self.visible && self.startup_hide > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.startup_hide -= 1;
        }
        #[cfg(windows)]
        {
            // 首帧保险：确保窗口是「可见但停在屏幕外」的状态。
            // 之所以要主动发一次，是因为窗口一旦真的被隐藏，update() 就停摆了，
            // 之后再点托盘也唤不回来（见 park_window 的注释）。
            if !self.startup_parked {
                self.startup_parked = true;
                if !self.visible {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                        PARKED_X, PARKED_Y,
                    )));
                }
            }
        }

        while let Ok(ev) = self.tray_rx.try_recv() {
            match ev {
                TrayEvent::Click { fallback } => match self.tray_icon_rect(ctx).or(fallback) {
                    Some(rect) => self.toggle(ctx, rect),
                    // 状态项还没被布局（刚创建的头几帧）就拿不到位置，这次点击直接忽略
                    None => eprintln!("[ui] 状态栏项还没有位置，忽略这次点击"),
                },
                #[cfg(windows)]
                TrayEvent::BadgeClick { at } => self.toggle(ctx, at),
            }
        }

        // 失焦自动收起，模拟原生 popover。
        // 前提是这次展开**真的拿到过焦点**：Windows 的前台窗口规则可能让
        // SetForegroundWindow 失败，那种情况下焦点一直为 false，
        // 按「失焦」处理会让面板刚打开就自己关掉。没焦点时改由再点一次图标收起。
        if self.visible {
            let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
            if focused {
                self.had_focus = true;
            } else if self.had_focus
                // `--show` 是给调试/截图用的：那种情况下窗口本来就不是用户点开的，
                // 一失焦就收起会导致「刚起来就没了」，根本没法核对界面。
                && !self.show_on_start
                && self.shown_at.elapsed() > HIDE_DEBOUNCE
            {
                self.close(ctx);
                self.auto_hidden_at = Some(Instant::now());
            }
        }

        self.draw(ctx);

        // 展开动画（约 0.3s）期间要连续重绘，之后回到 1s 心跳
        if self.reveal_secs() < 0.4 {
            ctx.request_repaint();
        }

        // 心跳：即使窗口隐藏也保证每秒有一次 update，状态栏文字不会停更
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    fn draw(&mut self, ctx: &egui::Context) {
        let palette = theme::palette(ctx.style().visuals.dark_mode);
        let lang = self.lang;
        let alpha = self.open_alpha();

        let frame = egui::Frame::new()
            .inner_margin(egui::Margin::same(14))
            // 面板本身也要参与淡入，否则会出现「背景先出现、内容后浮现」
            .fill(palette.panel.gamma_multiply(alpha))
            .stroke(Stroke::new(0.5, palette.border.gamma_multiply(alpha)))
            .corner_radius(CornerRadius::same(14));

        egui::CentralPanel::default()
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                ui.set_width(WINDOW_W - 28.0);
                match self.view {
                    View::Main => self.main_body(ui, &palette, lang),
                    View::Settings => self.settings_body(ui, &palette, lang),
                }
            });
    }

    /// 展开动画的进度 0..1（已缓动）。窗口没显示时恒为 0。
    fn open_alpha(&self) -> f32 {
        if !self.visible {
            return 0.0;
        }
        let elapsed = self.opened_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(1.0);
        ease_out_cubic((elapsed / 0.13).clamp(0.0, 1.0))
    }

    /// 距展开已经过去多久（秒）。逐行浮现按它算延迟。
    fn reveal_secs(&self) -> f32 {
        self.opened_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(1.0)
    }

    /// 主面板。
    ///
    /// 版面从上到下只有四段：标题 / 两个大数字 / 列表（含时间窗口）/ 页脚。
    /// 段与段之间用一条 1px 淡线分开，不再用满宽实线——面板只有 350px 宽，
    /// 线一重就变成「被切成三块」。
    ///
    /// 高度是固定的（[`PANEL_H`]），所以列表吃掉「剩下多少」，页脚靠底。
    fn main_body(&mut self, ui: &mut egui::Ui, palette: &Palette, lang: Lang) {
        let p = palette;
        let reveal = self.reveal_secs();

        self.header(ui, p, lang);
        ui.add_space(12.0);
        self.totals(ui, p, lang);
        ui.add_space(12.0);

        hairline(ui, p);
        ui.add_space(9.0);

        // 时间窗口紧贴列表：它只决定列表按哪段时间排序，控件离内容越近越好。
        self.list_header(ui, p, lang);
        ui.add_space(6.0);
        // 提醒行有没有内容要在排版前就知道：它占一行高度，列表得先让出来。
        // 以前是无条件预留 22pt，结果没提醒时列表底下永远空一截。
        let alert = self.alert_text(lang);
        self.list(ui, p, lang, reveal, alert.is_some());
        if let Some(text) = &alert {
            self.alert_line(ui, p, text);
        }

        ui.add_space(8.0);
        hairline(ui, p);
        ui.add_space(4.0);
        self.footer(ui, p, lang);
    }

    /// 设置页。
    fn settings_body(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let zh = matches!(lang, Lang::Zh);
        let mut changed = false;

        ui.horizontal(|ui| {
                let back = ui.add(
                    egui::Button::new(
                        RichText::new(lang.t("‹ 返回", "‹ Back"))
                            .size(12.0)
                            .color(p.subtext),
                    )
                    .frame(false),
                );
                if back.clicked() {
                    self.view = View::Main;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(lang.t("设置", "Settings"))
                            .size(13.0)
                            .color(p.text),
                    );
                });
            });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
        // 设置行之间留 5pt：行本身 32pt，合起来 37pt 一行。
        // 面板高度是按「列表 7 行」定的（411pt），这个密度刚好把剩下的高度
        // 匀进行距里——比在页面底部堆一块空白好看，也不会把「开机自启」挤出面板。
        // 再松 1pt（6pt 行距）就会溢出 1pt，把开关行顶到面板边上。
        ui.spacing_mut().item_spacing.y = 5.0;

        // ---- 菜单栏（只有 macOS 有这回事）----
        //
        // Windows 的任务栏托盘没有文字标题，这两项在那边是纯装饰：
        // 调了不会有任何变化。宁可少两行，也不能给用户一个假的开关。
        #[cfg(target_os = "macos")]
        {
            if option_row(
                ui,
                p,
                lang.t("菜单栏", "Menu bar"),
                &mut self.settings.tray_mode,
                &[
                    (TrayMode::Both, lang.t("全部", "Icon+Speed")),
                    (TrayMode::SpeedOnly, lang.t("仅网速", "Speed")),
                    (TrayMode::IconOnly, lang.t("仅图标", "Icon")),
                ],
            ) {
                changed = true;
            }

            // ---- 菜单栏配色 ----
            // 彩色时下行蓝、上行绿；单色时全部跟随系统标签色，菜单栏更素
            if option_row(
                ui,
                p,
                lang.t("菜单栏配色", "Menu bar color"),
                &mut self.settings.tray_color,
                &[
                    (true, lang.t("彩色", "Color")),
                    (false, lang.t("单色", "Mono")),
                ],
            ) {
                changed = true;
            }
        }

        // ---- 任务栏（只有 Windows 有这回事）----
        //
        // Windows 的托盘图标是定死的方形槽，塞不进 `↓1.2M`；数字是画在图标
        // 上方的独立贴片里的（`badge_window`）。所以这里的选项和 macOS 不同：
        // 那边调的是「状态栏显示什么」，这边只有「贴片开不开」。
        #[cfg(windows)]
        {
            if option_row(
                ui,
                p,
                lang.t("任务栏网速", "Tray speed"),
                &mut self.settings.tray_badge,
                &[
                    (true, lang.t("显示", "Show")),
                    (false, lang.t("隐藏", "Hide")),
                ],
            ) {
                changed = true;
            }

            if option_row(
                ui,
                p,
                lang.t("网速配色", "Speed color"),
                &mut self.settings.tray_color,
                &[
                    (true, lang.t("彩色", "Color")),
                    (false, lang.t("单色", "Mono")),
                ],
            ) {
                changed = true;
            }
        }

        // ---- 外观 ----
        if option_row(
            ui,
            p,
            lang.t("外观", "Appearance"),
            &mut self.settings.theme,
            &[
                (ThemeMode::System, lang.t("跟随系统", "System")),
                (ThemeMode::Light, lang.t("浅色", "Light")),
                (ThemeMode::Dark, lang.t("深色", "Dark")),
            ],
        ) {
            changed = true;
        }

        // ---- 语言 ----
        if option_row(
            ui,
            p,
            lang.t("语言", "Language"),
            &mut self.settings.lang,
            &[
                (LangMode::Auto, lang.t("自动", "Auto")),
                (LangMode::Zh, "中文"),
                (LangMode::En, "English"),
            ],
        ) {
            changed = true;
        }

        // ---- 列表条数 ----
        // 面板高度固定，所以这一项不再管「窗口多高」，而是「最多列几个应用」：
        // 超出的部分不展示，多到装不下可视行数时靠滚动查看。
        if option_row(
            ui,
            p,
            lang.t("最多展示", "Max apps"),
            &mut self.settings.max_apps,
            &[(5, "5"), (10, "10"), (20, "20")],
        ) {
            changed = true;
        }

        // ---- 历史保留 ----
        if option_row(
            ui,
            p,
            lang.t("历史保留", "History"),
            &mut self.settings.retention_days,
            &[
                (7, if zh { "7 天" } else { "7d" }),
                (30, if zh { "30 天" } else { "30d" }),
                (90, if zh { "90 天" } else { "90d" }),
            ],
        ) {
            changed = true;
        }

        // ---- 流量提醒 ----
        if option_row(
            ui,
            p,
            lang.t("流量提醒", "Traffic alert"),
            &mut self.settings.alert_mb,
            &[
                (0, lang.t("关闭", "Off")),
                (100, "100M"),
                (500, "500M"),
                (1024, "1G"),
            ],
        ) {
            changed = true;
        }

        // 提示行排在「底部块」上方：它的出现/消失只挤压中间的留白，
        // 不会把「开机自启」推上推下（toast_line 本身就是定高的）。
        ui.add_space(5.0);
        self.toast_line(ui, p);

        // 面板高度固定，设置页内容比面板矮，剩下的那截全部塞在这里：
        // 于是「开机自启」贴着面板底边，空白看着像刻意留的，而不是没排完。
        let block_h = 1.0 + 5.0 + 32.0 + 5.0; // 分隔线 + 间距 + 开关行 + 行距
        let pad = (ui.available_height() - block_h).max(0.0);
        if std::env::var_os("TM_TRACE_LIST").is_some() {
            eprintln!("[settings] avail={:.1} pad={pad:.1}", ui.available_height());
        }
        ui.add_space(pad);

        ui.separator();
        ui.add_space(5.0);

        // ---- 开机自启（状态来自系统，不是配置文件）----
        let mut on = self.launch_at_login;
        if toggle_row(ui, p, lang.t("开机自启", "Launch at login"), &mut on) {
            match autostart::set_enabled(on) {
                Ok(()) => {
                    self.launch_at_login = on;
                    self.toast = Some((
                        match (zh, on) {
                            (true, true) => "已开启开机自启".to_owned(),
                            (true, false) => "已关闭开机自启".to_owned(),
                            (false, true) => "Launch at login enabled".to_owned(),
                            (false, false) => "Launch at login disabled".to_owned(),
                        },
                        Instant::now(),
                    ));
                }
                Err(e) => self.toast = Some((e, Instant::now())),
            }
        }

        if changed {
            self.apply_settings(ui.ctx());
        }
    }

    /// 设置改动后立刻生效并落盘。
    fn apply_settings(&mut self, ctx: &egui::Context) {
        self.settings.save();
        ctx.set_theme(theme_pref(self.settings.theme));
        self.lang = resolve_lang(self.settings.lang, self.font_lang);
        self.retention
            .store(self.settings.retention_days, Ordering::Relaxed);
        // 清掉缓存，逼 `sync_tray_title` 下一帧重画：只改了配色时文字没变，
        // 不清缓存就不会重新上色。
        self.tray_title.clear();
    }


    fn header(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        // 优先显示历史库里的 24 小时合计；库还没写进数据时退回本次运行
        let (day_rx, day_tx) = self
            .latest
            .as_ref()
            .map(|s| (s.day_rx, s.day_tx))
            .unwrap_or((0, 0));
        let (srx, stx) = self.session();
        let (rx, tx) = if day_rx + day_tx > 0 {
            (day_rx, day_tx)
        } else {
            (srx, stx)
        };
        let mut open_settings = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.t("网络流量", "Network"))
                    .size(14.0)
                    .color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if sliders_button(ui, p).clicked() {
                    open_settings = true;
                }
                ui.add_space(4.0);
                // 第一帧数据还没到时整块省略：写「本次 0 B」既不准确也没意义
                if self.latest.is_some() {
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            if day_rx + day_tx > 0 {
                                lang.t("24小时", "24h")
                            } else {
                                lang.t("本次", "Session")
                            },
                            format_bytes(rx.saturating_add(tx))
                        ))
                        .size(11.0)
                        .color(p.subtext),
                    );
                }
            });
        });
        if open_settings {
            self.view = View::Settings;
        }
    }

    /// 顶部两个大数字：左右等分的两栏，中间一条竖线。
    ///
    /// 之前是两个块挤在左边（中间 22px 空隙），右侧留一大片空白，
    /// 面板看起来重心偏左。等分之后两栏各自有落点，扫视线时不需要横移。
    fn totals(&self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        // 还没拿到第一帧时不显示 0：0 读起来像「没有流量」，而真实状态是
        // 「还在测」。这里给一个占位条，和下面列表的骨架是同一套语言。
        let (rx_text, tx_text) = match &self.latest {
            None => (None, None),
            Some(s) => (
                Some(format_speed(s.total_rx)),
                Some(format_speed(s.total_tx)),
            ),
        };

        const H: f32 = 44.0;
        let col = ((ui.available_width() - METRIC_DIVIDER_W) / 2.0).max(60.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            metric(ui, col, H, "↓", lang.t("下载", "Down"), rx_text, p.accent, p.subtext);

            let (r, _) = ui.allocate_exact_size(
                Vec2::new(METRIC_DIVIDER_W, H),
                egui::Sense::hover(),
            );
            let line = egui::Rect::from_center_size(
                r.center(),
                Vec2::new(1.0, H - 18.0),
            );
            ui.painter().rect_filled(line, 0, p.border);

            metric(ui, col, H, "↑", lang.t("上传", "Up"), tx_text, p.upload, p.subtext);
        });
    }

    /// 列表上方的一行：左边「应用 · N」，右边时间窗口分段控件。
    fn list_header(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let count = match (&self.latest, self.window) {
            (Some(s), Window::Live) => s.rates.len(),
            (Some(s), Window::Min1) => s.windowed.min1.len(),
            (Some(s), Window::Min5) => s.windowed.min5.len(),
            (Some(s), Window::Hour1) => s.windowed.hour1.len(),
            (None, _) => 0,
        };
        // 只列前 N 个，标题里的计数也要跟着截断，否则会出现「应用 · 9」却只有 5 行
        let count = count.min(self.settings.max_apps.max(1));

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(lang.t("应用", "Apps"))
                    .size(11.0)
                    .color(p.subtext),
            );
            if count > 0 {
                ui.label(
                    RichText::new(format!("· {count}"))
                        .size(11.0)
                        .color(p.subtext.gamma_multiply(0.75)),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(SEG_W, SEG_H),
                    egui::Sense::hover(),
                );
                segmented_at(ui, rect, &mut self.window, p, lang);
            });
        });
    }

    fn list(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        reveal: f32,
        has_alert: bool,
    ) {
        // 每一行自己是一张「小卡」：1px 描边 + 内边距，行与行之间留 5pt 外边距。
        //
        // 以前行是直接摊在面板上的，只能靠行距和数字列对齐撑结构，看着像一张
        // 没画完的表格。现在每项有自己的边界，行与行的分界、条目的归属都不
        // 用猜；描边很淡（`p.border`），所以密集排列也不会显得吵。
        let reserve = FOOTER_BAND + if has_alert { ALERT_H } else { 0.0 };
        let avail = (ui.available_height() - reserve).max(ROW_H);
        let cap = (((avail + ROW_GAP) / ROW_H).floor() as usize).max(1);

        // 先把这一帧的内容定下来（都要 owned，下面要 &mut self 取图标）
        let mut rates: Vec<AppRate> = match (self.latest.as_ref(), self.window) {
            (None, _) => Vec::new(),
            (Some(s), Window::Live) => s.rates.clone(),
            (Some(s), Window::Min1) => s.windowed.min1.clone(),
            (Some(s), Window::Min5) => s.windowed.min5.clone(),
            (Some(s), Window::Hour1) => s.windowed.hour1.clone(),
        };
        let loading = self.latest.is_none();
        let note: Option<&'static str> = if loading {
            None
        } else if rates.is_empty() {
            // 实时没有，其它窗口必然也没有；区分文案只是为了让人知道
            // 「不是采集中断了，是这段时间确实没流量」
            Some(if self.window == Window::Live {
                lang.t("当前没有流量", "No traffic right now")
            } else {
                lang.t("这段时间没有流量", "No traffic in this window")
            })
        } else {
            // 只列前 N 个：几十个后台进程全列出来没人看，按流量降序截断，
            // 超出的部分靠滚动查看。N 来自设置里的「最多展示」。
            rates.truncate(self.settings.max_apps.max(1));
            None
        };

        // 空状态不给一整屏的空盒子，三行的高度足够放一句说明
        let body_rows = if loading {
            cap
        } else if note.is_some() {
            cap.min(3)
        } else {
            rates.len().min(cap)
        };
        let list_h = body_rows as f32 * ROW_H - ROW_GAP;
        if std::env::var_os("TM_TRACE_LIST").is_some() {
            eprintln!("[list] avail={avail:.1} cap={cap} rows={body_rows} list_h={list_h:.1}");
        }

        if loading {
            // 采样器要两个采样点才能差分出速率，第一个可用数据大约在 1–2 秒后。
            // 这段时间画骨架而不是留白：空面板看起来像坏了。
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), list_h),
                Layout::top_down(Align::Min),
                |ui| self.skeleton(ui, p, body_rows),
            );
        } else if let Some(text) = note {
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), list_h),
                Layout::top_down(Align::Min),
                |ui| empty_note(ui, text, list_h, p),
            );
        } else {
            // 每帧把「上一帧定下来的位置」写回去。不写回去的话，停止滚动之后
            // 就没法把半行偏移吸回整行（ScrollArea 只在构造时接受偏移）。
            let prev = self.list_scroll;
            let out = egui::ScrollArea::vertical()
                .id_salt("app-list")
                .vertical_scroll_offset(prev)
                .max_height(list_h)
                // 关掉自动收缩，否则内容少时窗口会再缩一圈
                .auto_shrink([false, false])
                // 自带的浮动把手会压在行盒的右描边上（它画在内容矩形之内），
                // 所以关掉，改用下面自绘的细指示条
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    // 让出右侧过道：行盒的描边不能和指示条叠在一起
                    ui.set_width(ui.available_width() - SCROLL_GUTTER);
                    // 行距自己控制，不用默认 item_spacing，行高才是确定的 31
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let n = rates.len();
                    for (i, r) in rates.iter().enumerate() {
                        // 逐行错峰浮现：每行晚 14ms 开始，100ms 淡入
                        let t = ((reveal - i as f32 * 0.014) / 0.10).clamp(0.0, 1.0);
                        ui.scope(|ui| {
                            // 用 multiply 而不是 set：面板整体淡入时，
                            // set 会把这层透明度覆盖掉，行会比面板更实
                            ui.multiply_opacity(ease_out_cubic(t));
                            self.row(ui, r, p);
                        });
                        if i + 1 < n {
                            ui.add_space(ROW_GAP);
                        }
                    }
                });

            // 滚动停下后吸附到整行。触控板松手时偏移量经常停在 12.3
            // 这种半行值，视口上下就各露半截行。
            let now = out.state.offset.y;
            let max_off = (out.content_size.y - out.inner_rect.height()).max(0.0);
            self.list_scroll = snap_scroll(prev, now, max_off);

            // 自绘滚动指示条：3pt 宽，贴在行盒右侧的过道里。
            // 用 egui 自带的浮动把手会压住行盒的描边，在「每行一张小卡」
            // 的版式里格外明显；自己画一条细的最省事，也不抢视线。
            if max_off > 1.0 {
                let track = out.inner_rect;
                let frac = (track.height() / out.content_size.y).clamp(0.06, 1.0);
                let h = (track.height() * frac).max(24.0);
                let y = track.top()
                    + (track.height() - h)
                        * (self.list_scroll / max_off).clamp(0.0, 1.0);
                let x = ui.max_rect().right() - SCROLL_BAR_W;
                let color = if p.panel.r() < 128 {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 70)
                } else {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 60)
                };
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(x, y),
                        Vec2::new(SCROLL_BAR_W, h),
                    ),
                    CornerRadius::same(2),
                    color,
                );
            }
        }

        // 行数不满一屏时把余量吃掉，页脚才会留在面板底边
        let rest = avail - list_h;
        if rest > 0.0 {
            ui.add_space(rest);
        }
    }

    /// 数据还没到时的占位骨架。
    ///
    /// 面板高度固定，所以骨架要**铺满整个列表区**（有几行可视就画几行）——
    /// 只画三行的话下面留一大片空白，加载态看着像列表塌了。
    /// 越往下越淡：读起来是「正在往下填」，而不是一堵灰墙。
    fn skeleton(&self, ui: &mut egui::Ui, p: &Palette, rows: usize) {
        let t = ui.ctx().input(|i| i.time) as f32;
        // 0.22..0.5 之间呼吸，太亮会喧宾夺主
        let pulse = 0.36 + 0.14 * (t * 2.6).sin();
        let width = ui.available_width();
        let rows = rows.max(1);
        // 行距自己控制，和真实列表一样是 24：否则骨架每行会比数据行高 2pt，
        // 八行下来最后一行会压到提醒区里去
        ui.spacing_mut().item_spacing.y = 0.0;

        for i in 0..rows {
            // 最后几行淡到 45%，再低就看不见了
            let fade = 1.0 - (i as f32 / rows as f32) * 0.55;
            let color = p.subtext.gamma_multiply(pulse * fade);

            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(width, ROW_BOX_H), egui::Sense::hover());
            let painter = ui.painter();
            // 骨架也画同样的行盒，数据到达时不会「凭空长出一圈框」
            painter.rect(
                rect,
                CornerRadius::same(ROW_RADIUS),
                p.chip.gamma_multiply(0.35),
                Stroke::new(1.0, p.border.gamma_multiply(pulse * fade)),
                egui::StrokeKind::Inside,
            );
            let bar = |x: f32, w: f32| {
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.center().y - 4.5),
                    Vec2::new(w, 9.0),
                )
            };
            // 图标位：和真实行的 18pt 方块对齐
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left() + ROW_PAD_X, rect.center().y - 9.0),
                    Vec2::splat(18.0),
                ),
                CornerRadius::same(6),
                color,
            );
            let right = rect.right() - ROW_PAD_X;
            let tx_x = right - 1.0 - NUM_W_TX;
            let rx_x = tx_x - NUM_GAP - NUM_W_RX;
            // 名字长度按行号错开（走一个固定序列，不是随机），
            // 否则每帧变一次，骨架会自己抖
            let name_left = rect.left() + ROW_PAD_X + 25.0;
            let name_max = (rx_x - 8.0 - name_left).max(24.0);
            let name_w = (name_max * (0.45 + ((i * 37) % 13) as f32 / 40.0)).min(name_max);
            painter.rect_filled(bar(name_left, name_w), CornerRadius::same(3), color);
            // 两根灰条的位置和真实行里的两个速率列对齐，
            // 骨架换成本体时不会有东西横着挪一下。
            // 一根条代表「数值 + 单位」整格，别拆成两截——四根小方块一行的
            // 骨架看着像乱码，反而不像在加载
            painter.rect_filled(
                bar(rx_x + 6.0, NUM_W_RX - 12.0),
                CornerRadius::same(3),
                color,
            );
            painter.rect_filled(
                bar(tx_x + 6.0, NUM_W_TX - 14.0),
                CornerRadius::same(3),
                color,
            );
            if i + 1 < rows {
                ui.add_space(ROW_GAP);
            }
        }
        // 呼吸动画需要持续重绘，否则骨架是静止的
        ui.ctx().request_repaint();
    }

    /// 一行：图标 + 名字 + 下行速率 + 上行速率。
    ///
    /// 两个速率用**定宽列 + 右对齐**画，而不是靠布局自然排。这样同一列数字
    /// 的小数点能竖着对齐，扫一眼就能比出谁吃得多；否则上行是 `0`、下行是
    /// `1.2 MB/s` 时，整列会左右错开十几像素。
    /// 一行：图标 + 名字 + 下行速率 + 上行速率，脚下一根流量占比条。
    ///
    /// 全部用 painter 画而不是拼一堆小控件：
    /// - 数字列必须绝对对齐（见下面 `rate_cell`），靠布局会随名字长度漂；
    /// - 行底那根占比条要垫在文字**下面**，只有先拿到整行 rect 才好画。
    fn row(&mut self, ui: &mut egui::Ui, r: &AppRate, p: &Palette) {
        let name = pretty_name(r.app.as_str());
        let tex = self.icons.get(r.app.as_str());
        let dark = p.panel.r() < 128;

        let width = ui.available_width();
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(width, ROW_BOX_H), egui::Sense::hover());

        // 悬停：底色抬一档 + 描边转向强调色，120ms 过渡。
        //
        // 每一项本来就有描边，所以悬停不需要再加色块——只把这一格的描边点亮，
        // 视线就知道「操作的是这个」。渐变是为了不闪。
        let t = ui.ctx().animate_bool_with_time(
            ui.id().with(("row-hover", r.app.as_str())),
            resp.hovered(),
            0.12,
        );
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(ROW_RADIUS),
            p.chip.gamma_multiply(0.5 + 0.5 * t),
            Stroke::new(1.0, mix(p.border, p.accent, t * 0.6)),
            egui::StrokeKind::Inside,
        );

        // 数字不贴到描边上
        let right = rect.right() - ROW_PAD_X;

        // 图标：真实图标直接贴；拿不到就用低饱和的字母块，比一坨纯色块轻
        let icon = egui::Rect::from_min_size(
            egui::pos2(rect.left() + ROW_PAD_X, rect.center().y - icons::ICON_PT / 2.0),
            Vec2::splat(icons::ICON_PT),
        );
        match tex {
            Some(t) => {
                painter.image(
                    t.id(),
                    icon,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => badge(painter, icon, &name, dark),
        }

        // 两列速率的右边界：先定数字，名字再去吃剩下的宽度
        let tx_right = right - 1.0;
        let rx_right = tx_right - NUM_W_TX - NUM_GAP;
        let name_left = icon.right() + 7.0;

        painter.text(
            egui::pos2(name_left, rect.center().y),
            egui::Align2::LEFT_CENTER,
            fit_name(&name, NAME_UNITS),
            egui::FontId::proportional(12.5),
            p.text,
        );

        rate_cell(
            painter,
            rx_right,
            rect.center().y,
            r.rx_bps,
            12.0,
            p.accent,
            p.subtext.gamma_multiply(0.8),
        );
        rate_cell(
            painter,
            tx_right,
            rect.center().y,
            r.tx_bps,
            10.5,
            p.subtext,
            p.subtext.gamma_multiply(0.7),
        );
    }

    /// 近 10 分钟单应用超过阈值时给一行提醒。阈值没配就什么都不画。
    ///
    /// 用「窗口平均速率 × 600 秒」算字节数，而不是另存一份计数器：
    /// 时间窗口本来就是这么用的，省掉一套状态。
    ///
    /// **高度恒定**：提醒出现/消失不能让列表的可视行数跟着变。
    fn alert_line(&self, ui: &mut egui::Ui, p: &Palette, text: &str) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ALERT_H),
            egui::Sense::hover(),
        );
        ui.painter().text(
            egui::pos2(rect.left() + 2.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(10.5),
            p.warn,
        );
    }

    /// 提醒文案。没有超阈值的应用时返回 `None`。
    fn alert_text(&self, lang: Lang) -> Option<String> {
        let threshold = self.settings.alert_mb;
        if threshold == 0 {
            return None;
        }
        let snap = self.latest.as_ref()?;
        let limit = threshold.saturating_mul(1024 * 1024);

        let worst = snap
            .windowed
            .min10
            .iter()
            .map(|r| (r.app.as_str(), (r.total_bps() * 600.0) as u64))
            .filter(|(_, bytes)| *bytes >= limit)
            .max_by_key(|(_, bytes)| *bytes)?;

        let (key, bytes) = worst;
        let name = fit_name(&pretty_name(key), 16);
        Some(match lang {
            Lang::Zh => format!("⚠ 近 10 分钟 {name} 已用 {}", format_bytes(bytes)),
            Lang::En => format!("⚠ {name} used {} in 10 min", format_bytes(bytes)),
        })
    }

    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let zh = matches!(lang, Lang::Zh);
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
        }
        // 先拷一份：下面按钮要 &mut self，不能挂着对 toast 的借用
        let toast = self.toast.as_ref().map(|(msg, _)| msg.clone());

        // 定高，页脚才永远贴着面板底边（面板高度固定，列表吃掉剩下的空间）
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), FOOTER_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                let export = ui.add(
                    egui::Button::new(
                        RichText::new(lang.t("导出", "Export"))
                            .size(10.5)
                            .color(p.subtext),
                    )
                    .frame(false),
                );
                if export.clicked() {
                    self.export_history(zh);
                }

                // 临时提示直接画在页脚里：不另占一行，出现/消失时列表不会跳，
                // 也不会在列表底下留一条永远空着的「提示槽」。
                if let Some(msg) = &toast {
                    let w = (ui.available_width() - 52.0).max(60.0);
                    ui.add_sized(
                        Vec2::new(w, FOOTER_H),
                        egui::Label::new(
                            RichText::new(msg.as_str()).size(10.0).color(p.subtext),
                        )
                        .truncate(),
                    );
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let quit = ui.add(
                        egui::Button::new(
                            RichText::new(lang.t("退出", "Quit")).size(11.0).color(p.subtext),
                        )
                        .frame(false),
                    );
                    if quit.clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            },
        );
    }

    /// 底部临时提示。过期后自动消失。
    ///
    /// 同样**定高**：提示出现六秒后消失，不能让列表在那一瞬间长高一截。
    fn toast_line(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
        }
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), TOAST_H),
            egui::Sense::hover(),
        );
        let Some((msg, _)) = &self.toast else { return };
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            fit_name(msg, 54),
            egui::FontId::proportional(10.0),
            p.subtext,
        );
    }

    /// 把库里全部历史写成 CSV。
    fn export_history(&mut self, zh: bool) {
        let msg = export_history_csv(zh);
        self.toast = Some((msg, Instant::now()));
    }
}

/// 导出全部历史，返回可以直接展示的提示文案。
///
/// 几十万行以内都是几十毫秒，不值得为它再起一个线程和一套状态机，
/// 所以界面和 `--export` 命令行入口共用这一份同步实现。
pub fn export_history_csv(zh: bool) -> String {
    let result = (|| -> Result<(u64, String), String> {
        let store = tm_store::Store::open(&engine::db_path()).map_err(|e| e.to_string())?;
        let stamp = store.local_now_stamp().map_err(|e| e.to_string())?;
        let name = format!("Tramoing-{stamp}.csv");

        // 先写「下载」目录；被 TCC 拒绝（用户点了不允许）就退到应用自己的目录，
        // 这两个位置都试过才算真失败。
        let mut last_err = String::new();
        for dir in [
            tm_store::default_export_dir(),
            tm_store::data_dir().join("exports"),
        ] {
            let path = dir.join(&name);
            match store.export_csv(0, i64::MAX, &path) {
                Ok(rows) => return Ok((rows, tildify(&path.to_string_lossy()))),
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(last_err)
    })();

    let msg = match result {
        Ok((0, _)) => {
            if zh {
                "还没有历史数据".to_owned()
            } else {
                "No history yet".to_owned()
            }
        }
        Ok((n, path)) => {
            if zh {
                format!("已导出 {n} 行 → {path}")
            } else {
                format!("Exported {n} rows → {path}")
            }
        }
        Err(e) => {
            if zh {
                format!("导出失败：{e}")
            } else {
                format!("Export failed: {e}")
            }
        }
    };
    // 终端里也留一份，方便排查
    eprintln!("[export] {msg}");
    msg
}

// ---------- 绘制小工具 ----------

/// 设置项：左边标签，右边一组互斥选项。返回是否被改动。
fn option_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    const ROW_H: f32 = 32.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), egui::Sense::hover());
    let painter = ui.painter();

    painter.text(
        egui::pos2(rect.left() + 2.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(11.5),
        p.text,
    );

    // 选项区靠右，宽度按选项个数分，最少 44pt 一格
    let ctl_w = (options.len() as f32 * 52.0).min(rect.width() * 0.62);
    let ctl = egui::Rect::from_min_max(
        egui::pos2(rect.right() - ctl_w, rect.top() + 3.0),
        egui::pos2(rect.right(), rect.bottom() - 3.0),
    );
    painter.rect_filled(ctl, CornerRadius::same(6), p.chip);

    let item_w = ctl.width() / options.len() as f32;

    // 选中态滑块：位置缓动过去，而不是瞬间跳一格。
    // 用 `animate_value_with_time` 而不是自己记状态，egui 会负责持续重绘。
    let idx = options
        .iter()
        .position(|(v, _)| v == current)
        .unwrap_or(0) as f32;
    let pos = ui
        .ctx()
        .animate_value_with_time(ui.id().with(("opt-pill", label)), idx, 0.16);
    let pill = egui::Rect::from_min_size(
        egui::pos2(ctl.left() + pos * item_w, ctl.top()),
        Vec2::new(item_w, ctl.height()),
    );
    ui.painter()
        .rect_filled(pill.shrink(2.0), CornerRadius::same(5), p.panel);

    let mut changed = false;
    for (i, (value, text)) in options.iter().enumerate() {
        let r = egui::Rect::from_min_size(
            egui::pos2(ctl.left() + i as f32 * item_w, ctl.top()),
            Vec2::new(item_w, ctl.height()),
        );
        let resp = ui.interact(r, ui.id().with(("opt", label, i)), egui::Sense::click());
        let selected = *current == *value;

        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            *text,
            egui::FontId::proportional(10.5),
            if selected { p.text } else { p.subtext },
        );

        if resp.clicked() && !selected {
            *current = *value;
            changed = true;
        }
    }
    changed
}

/// 设置项：左边标签，右边一个开关。返回是否被点击。
fn toggle_row(ui: &mut egui::Ui, p: &Palette, label: &str, on: &mut bool) -> bool {
    const ROW_H: f32 = 32.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), egui::Sense::hover());
    // 整行都能点，比只点那个小圆点好按
    let resp = ui.interact(rect, ui.id().with(("toggle", label)), egui::Sense::click());

    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left() + 2.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(11.5),
        p.text,
    );

    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 16.0, rect.center().y),
        Vec2::new(30.0, 18.0),
    );
    // 0..1 的缓动值：轨道颜色和旋钮位置一起插值，开关才有「按下去」的手感
    let t = ui
        .ctx()
        .animate_bool_with_time(ui.id().with(("toggle-anim", label)), *on, 0.15);
    painter.rect_filled(track, CornerRadius::same(9), mix(p.switch_off, p.accent, t));
    let knob_x = track.left() + 9.0 + (track.width() - 18.0) * t.clamp(0.0, 1.0);
    painter.circle_filled(egui::pos2(knob_x, track.center().y), 7.0, Color32::WHITE);

    if resp.clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// 设置入口图标：三条带滑块的横线。手画而不是用 ⚙ 字符，
/// 是因为 Arial Unicode 里未必有这个字形，缺字会变成方块。
fn sliders_button(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::click());
    let hovered = resp.hovered();
    let color = if hovered { p.text } else { p.subtext };
    let painter = ui.painter();
    for i in 0..3 {
        let y = rect.top() + 3.0 + i as f32 * 5.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0, color),
        );
        // 滑块位置左右错开，看着像个均衡器而不是"汉堡菜单"
        let kx = rect.left() + 4.0 + ((i + 1) % 3) as f32 * 3.5;
        painter.circle_filled(egui::pos2(kx, y), 1.8, color);
    }
    resp
}

fn theme_pref(mode: ThemeMode) -> egui::ThemePreference {
    match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    }
}

/// `Auto` 表示「跟着字体可用性走」：能显示中文就用中文。
fn resolve_lang(mode: LangMode, auto: Lang) -> Lang {
    match mode {
        LangMode::Auto => auto,
        LangMode::Zh => Lang::Zh,
        LangMode::En => Lang::En,
    }
}

fn fit_name(name: &str, max_units: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for ch in name.chars() {
        let w = if (ch as u32) > 0x2E80 { 2 } else { 1 };
        if units + w > max_units {
            out.push('…');
            return out;
        }
        units += w;
        out.push(ch);
    }
    out
}

/// 把 home 目录前缀缩成 `~`。提示条里一行能多放出十几个字符。
fn tildify(path: &str) -> String {
    for var in ["HOME", "USERPROFILE"] {
        let Some(home) = std::env::var_os(var) else {
            continue;
        };
        let home = home.to_string_lossy();
        if !home.is_empty() && path.starts_with(home.as_ref()) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_owned()
}

/// 一条 1px 的淡分隔线。比 `ui.separator()` 轻，不会把面板切成几块。
fn hairline(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 1.0),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 0, p.border);
}

/// 列表滚动的吸附：停下时把偏移量对齐到整行。
///
/// `prev` 是这一帧开始时的偏移，`now` 是结束时的偏移。两者不同说明用户还在
/// 滚（滚轮或拖把手），原样保留；相同说明已经停下，把半行偏移吸到最近的整行。
/// 不吸的话触控板松手停在 12.3 这种位置，视口上下会各露半截行。
fn snap_scroll(prev: f32, now: f32, max: f32) -> f32 {
    let target = if (now - prev).abs() > 0.01 {
        now
    } else {
        (now / ROW_H).round() * ROW_H
    };
    target.clamp(0.0, max.max(0.0))
}

/// 列表为空时的一句话，垂直居中放在列表格里。
///
/// 面板高度是固定的，这句话要是贴着列表顶部，下面一大片空白会像界面没加载完。
fn empty_note(ui: &mut egui::Ui, text: &str, area_h: f32, p: &Palette) {
    // 0.42 而不是 0.5：文字自身的行高没算进来，靠上一点点看着才是正中
    ui.add_space(((area_h - 22.0) * 0.42).max(0.0));
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).size(12.0).color(p.subtext));
    });
}

/// 一行里的速率格子：定宽、右对齐、等宽字体。
///
/// 用 painter 直接落字而不是 `Label`：`Label` 的宽度随内容变，一列数字就
/// 对不齐了。这里宽度写死，右边缘固定，小数点自然成列。

/// 拿不到真实图标时的字母块。
///
/// 早先是纯色块 + 白字，颜色一多整列像调色盘；现在压成「同色系的淡底 +
/// 亮一档的字」，远看是彩色标记，近看又不抢应用名。
fn badge(painter: &egui::Painter, rect: egui::Rect, name: &str, dark: bool) {
    let c = icons::tile_color(name);
    let bg = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), if dark { 46 } else { 30 });
    painter.rect_filled(rect, CornerRadius::same(6), bg);
    let fg = if dark {
        mix(c, Color32::WHITE, 0.30)
    } else {
        c
    };
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(9.5),
        fg,
    );
}

/// 一格速率：数值（等宽）贴右，单位小一号、淡一档。
///
/// 不用 `format_speed` 整串落字：`1.3 MB/s` 里单位占了一半宽度，密度太低，
/// 一眼扫过去全是 `/s`。拆开之后数字更突出，单位退成注脚。
fn rate_cell(
    painter: &egui::Painter,
    right: f32,
    cy: f32,
    bps: f64,
    val_size: f32,
    val_color: Color32,
    unit_color: Color32,
) {
    // `0` 没有单位，补一个 B/s，免得这一行的数字跑到单位列里去
    let text = format_speed(bps);
    let (val, unit) = text.split_once(' ').unwrap_or((text.as_str(), "B/s"));
    let unit_text = format!(" {unit}");
    let unit_galley =
        painter.layout_no_wrap(unit_text, egui::FontId::proportional(8.5), unit_color);
    let val_galley = painter.layout_no_wrap(
        val.to_owned(),
        egui::FontId::monospace(val_size),
        val_color,
    );
    let unit_w = unit_galley.size().x;
    let val_w = val_galley.size().x;
    // 数值右边缘固定在「列右边界 − 单位宽」，与这一行的单位是 K 还是 M 无关
    let val_right = right - NUM_UNIT_W - 1.0;
    painter.galley(
        egui::pos2(val_right - val_w, cy - val_galley.size().y / 2.0),
        val_galley,
        val_color,
    );
    painter.galley(
        egui::pos2(right - unit_w, cy - unit_galley.size().y / 2.0),
        unit_galley,
        unit_color,
    );
}


/// 顶部一栏大数字：方向标签一行、数值一行。
///
/// 宽度必须由自己 allocate 定死。`allocate_ui_with_layout` 会按子 ui 的
/// 实际内容推进游标（数值 82.1 KB/s 只有 100pt 宽），两栏就缩不到等分，
/// 中间的竖线会歪到左半边去。
fn metric(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    arrow: &str,
    label: &str,
    value: Option<String>,
    color: Color32,
    sub: Color32,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::hover());
    let painter = ui.painter();
    let label_font = egui::FontId::proportional(10.5);

    // 箭头跟着数据方向着色：和状态栏、列表里的颜色是同一套语义
    painter.text(
        rect.left_top(),
        egui::Align2::LEFT_TOP,
        arrow,
        label_font.clone(),
        color,
    );
    let arrow_w = painter
        .layout_no_wrap(arrow.to_owned(), label_font.clone(), color)
        .size()
        .x;
    painter.text(
        egui::pos2(rect.left() + arrow_w + 3.0, rect.top()),
        egui::Align2::LEFT_TOP,
        label,
        label_font,
        sub,
    );

    let value_top = rect.top() + 18.0;
    match value {
        Some(v) => {
            painter.text(
                egui::pos2(rect.left(), value_top),
                egui::Align2::LEFT_TOP,
                v,
                egui::FontId::monospace(19.0),
                color,
            );
        }
        None => {
            // 数值占位条。静态的，不跟列表骨架一起呼吸——一屏里两个不同节奏的
            // 脉冲反而显得乱，这里只要「还没好」这一个信息。
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left(), value_top + 7.0),
                    Vec2::new((w * 0.62).min(96.0), 12.0),
                ),
                CornerRadius::same(4),
                sub.gamma_multiply(0.35),
            );
        }
    }
}

/// 分段控件：实时 / 1分钟 / 5分钟 / 1小时。画在给定矩形里，
/// 因为它现在跟「应用」标题共处一行，宽度由调用方决定。
fn segmented_at(
    ui: &mut egui::Ui,
    bar: egui::Rect,
    current: &mut Window,
    p: &Palette,
    lang: Lang,
) {
    ui.painter().rect_filled(bar, CornerRadius::same(6), p.chip);

    let all = Window::all();
    let item_w = bar.width() / all.len() as f32;

    // 滑块在四个档位之间滑动，而不是直接跳到新位置
    let idx = all.iter().position(|w| w == current).unwrap_or(0) as f32;
    let pos = ui
        .ctx()
        .animate_value_with_time(ui.id().with("win-pill"), idx, 0.18);
    let pill = egui::Rect::from_min_size(
        egui::pos2(bar.left() + pos * item_w, bar.top()),
        Vec2::new(item_w, bar.height()),
    );
    ui.painter()
        .rect_filled(pill.shrink(1.5), CornerRadius::same(5), p.panel);

    for (i, w) in all.iter().enumerate() {
        let r = egui::Rect::from_min_size(
            egui::pos2(bar.left() + i as f32 * item_w, bar.top()),
            Vec2::new(item_w, bar.height()),
        );
        let resp = ui.interact(r, ui.id().with(("win", i)), egui::Sense::click());
        let selected = *current == *w;

        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            w.label(lang),
            egui::FontId::proportional(10.5),
            if selected { p.text } else { p.subtext },
        );

        if resp.clicked() && !selected {
            *current = *w;
        }
    }
}

/// 缓出曲线。开头快、结尾慢，是最像「系统动效」的一条曲线。
fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// 两个颜色之间插值，用来给开关轨道做过渡。
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// 菜单栏/贴片用的速率写法：**长度恒定为 4 个字符**，右对齐补空格。
///
/// 菜单栏项是按标题宽度自适应的：文字一变长，整项就跟着变宽，右边的图标被
/// 推着来回挪。以前这里是「最短写法」，`9.9M` 和 `10M`、`56B` 和 `1.2M`
/// 长度不同，速率在单位边界上下浮动时整块标题就一直在跳。
///
/// 现在只保证一件事：不管数值多少，结果都是 4 个字符——
/// `"  0B"`、`"999B"`、`" 820K"`、`"1.2M"`、`" 12M"`、`"1.0G"`。
/// 再配上 `tm_macos::title_font` 里的等宽字体，宽度一个像素都不会变。
/// Windows 的贴片同理：宽度不变，数字列才不会左右抖。
///
/// 阈值取 999.5 而不是 1024：`{:.0}` 会把 1023.6K 进位成 `1024K`（5 个字符），
/// 所以提前一档换单位，保证取整后不会溢出 4 格。
pub(crate) fn menu_speed(bps: f64) -> String {
    const K: f64 = 1024.0;
    let b = bps.max(0.0);
    let (v, unit) = if b < 999.5 {
        (b, "B")
    } else if b < K * 999.5 {
        (b / K, "K")
    } else if b < K * K * 999.5 {
        (b / (K * K), "M")
    } else if b < K * K * K * 999.5 {
        (b / (K * K * K), "G")
    } else {
        (b / (K * K * K * K), "T")
    };
    // 字节这一档不用小数：0.5B 看着像坏了；K 以上保留一位小数，否则
    // 几百 K 和几 M 都只剩两位有效数字，扫一眼分不出量级。
    let s = if unit == "B" || v >= 9.95 {
        format!("{v:.0}{unit}")
    } else {
        format!("{v:.1}{unit}")
    };
    format!("{s:>4}")
}

// ---------- 托盘 ----------

/// 托盘图标边长（像素）。任务栏图标槽在 100%~200% 缩放下是 16~32px，
/// 给 32 让系统自己缩，比塞一张 36px 的更清楚。
#[cfg(not(target_os = "macos"))]
const TRAY_ICON_PX: u32 = 32;

/// 托盘图标线条颜色。Windows 只按位图画，不会像 macOS 模板图那样跟着主题反色。
#[cfg(windows)]
fn tray_tint() -> [u8; 3] {
    if tm_windows::theme::taskbar_is_light() {
        [28, 28, 32]
    } else {
        [255, 255, 255]
    }
}

#[cfg(all(not(target_os = "macos"), not(windows)))]
fn tray_tint() -> [u8; 3] {
    [28, 28, 32]
}

fn build_tray() -> Result<TrayHandle, String> {
    #[cfg(target_os = "macos")]
    {
        let (rgba, w, h) = icon::waveform();

        let item = tm_macos::StatusItem::new(|| {
            if let Some(tx) = TRAY_TX.get() {
                // 原生状态项的点击回调不带坐标，位置只能现查
                let _ = tx.send(TrayEvent::Click { fallback: None });
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        })
        .ok_or_else(|| "创建 NSStatusItem 失败（不在主线程？）".to_owned())?;
        item.set_icon(&rgba, w, h);
        // 这条日志是排查「图标不出现」的第一现场：没有位置说明还没被状态栏布局
        match item.icon_rect() {
            Some((x, y, w, h)) => eprintln!(
                "[ui] 状态栏项：x={x:.0} y={y:.0} w={w:.0} h={h:.0} visible={}",
                item.is_visible()
            ),
            None => eprintln!(
                "[ui] 状态栏项已创建但还没布局（visible={}）",
                item.is_visible()
            ),
        }
        return Ok(item);
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Windows 的托盘图标**不做模板反色**：深色任务栏上必须给白线，
        // 浅色任务栏上给黑线。以前给的是 macOS 那张纯黑模板图，
        // 在 Win11 默认的深色任务栏上等于没挂图标。
        let (rgba, w, h) = icon::waveform_tinted(TRAY_ICON_PX, tray_tint());
        let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Tramoing")
            .with_icon(icon)
            .build()
            .map_err(|e| e.to_string())?;
        install_tray_handler();
        Ok(tray)
    }
}

#[cfg(not(target_os = "macos"))]
fn install_tray_handler() {
    tray_icon::TrayIconEvent::set_event_handler(Some(|event: tray_icon::TrayIconEvent| {
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            rect,
            ..
        } = event
        {
            // 事件里的矩形是物理像素，和 `tray_icon_rect()` 同一套单位。
            // 正常情况用不上，图标被收进 Win11 的 `«` 溢出区时 `rect()`
            // 会返回 None，那时它是唯一的定位依据。
            let fallback = IconRect {
                x: rect.position.x as f32,
                y: rect.position.y as f32,
                w: rect.size.width as f32,
                h: rect.size.height as f32,
            };
            if let Some(tx) = TRAY_TX.get() {
                let _ = tx.send(TrayEvent::Click {
                    fallback: Some(fallback),
                });
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
    }));
}

/// 点了任务栏上的网速贴片。
///
/// 和点托盘图标共用同一套开合逻辑（`toggle`），只是**锚点用贴片自己**：
/// 贴片所在的位置和托盘图标可能差好几百像素，用托盘图标当锚点的话，
/// 点最左边的数字、面板却从右边冒出来，看着像点错了东西。
///
/// 贴片窗口拿不到 `&mut App`，所以只往 channel 里塞一条事件，真正的开合
/// 仍然在 `update()` 里按状态决定——两边各记一份「面板开没开」迟早对不上。
#[cfg(windows)]
pub(crate) fn badge_clicked(at: Option<tm_windows::screen::Rect>) {
    let Some(r) = at else {
        // 连自己的矩形都拿不到（窗口正在销毁）：退回按托盘图标摆
        if let Some(tx) = TRAY_TX.get() {
            let _ = tx.send(TrayEvent::Click { fallback: None });
        }
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        return;
    };
    let at = IconRect {
        x: r.x as f32,
        y: r.y as f32,
        w: r.w as f32,
        h: r.h as f32,
    };
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(TrayEvent::BadgeClick { at });
    }
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

/// 一块屏幕区域，单位**物理像素**。面板定位的数学全在这个单位下做，
/// 最后再统一除以缩放比换成 egui 的点，免得两边单位混着算。
#[derive(Debug, Clone, Copy)]
struct ScreenBox {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// 面板贴边时离屏幕边缘至少留多少（点）
const EDGE_MARGIN: f32 = 8.0;
/// 面板和状态栏图标之间的间隙（点）
const PANEL_GAP: f32 = 6.0;

/// 面板的落位。**全部用物理像素存**，下发前才按当前缩放比换成点。
///
/// 为什么不直接存点：混合 DPI 的双屏上，窗口被摆到另一块屏后
/// `pixels_per_point` 会变，存点的话同一个点会被换算成不同的物理位置，
/// 面板会在展开动画中途横向跳一下。存物理像素则怎么换都是同一个位置。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    /// 左边缘（物理像素）
    x: f32,
    /// 钉住图标的那条边：向上长时是**底边**，向下长时是**顶边**（物理像素）
    anchor_y: f32,
    /// 面板向上生长（任务栏在底部）
    up: bool,
    /// 可用纵向范围（物理像素），用来夹住面板别长到屏幕外
    top: f32,
    bottom: f32,
}

impl Placement {
    /// 给定面板高度（点）和缩放比，算出窗口左上角（点）。
    ///
    /// 向上长时窗口顶边 = 底边 − 高度，所以高度每变一点，顶边都要跟着重算；
    /// 向下长时顶边不动。夹边放在最后，面板比可用空间还高时优先保住顶部。
    fn top_left(&self, height_pt: f32, ppp: f32) -> egui::Pos2 {
        let ppp = ppp.max(1.0);
        let height_px = height_pt * ppp;
        let y = if self.up {
            self.anchor_y - height_px
        } else {
            self.anchor_y
        };
        // 上边界不留边距：向下展开时锚点本来就贴着菜单栏，多减 8px 会把面板
        // 从图标下面硬拉开一道缝。下边界留边距，别让面板压住任务栏。
        let lo = self.top;
        let hi = (self.bottom - height_px - EDGE_MARGIN * ppp).max(lo);
        egui::pos2(self.x / ppp, y.clamp(lo, hi) / ppp)
    }
}

/// 纯几何：给定图标位置和工作区，算出面板落位。不碰系统接口，可单测。
///
/// 入参全是物理像素，返回的也是物理像素。
fn compute_placement(icon: IconRect, work: ScreenBox, ppp: f32) -> Placement {
    let ppp = ppp.max(1.0);
    let margin = EDGE_MARGIN * ppp;
    let gap = PANEL_GAP * ppp;
    let panel_w = WINDOW_W * ppp;

    let icon_cx = icon.x + icon.w / 2.0;
    let icon_cy = icon.y + icon.h / 2.0;

    // 图标中心落在工作区下半部 → 任务栏在底部 → 面板向上长。
    // 反过来（macOS 菜单栏在顶部）就往下长。
    // 用工作区中线而不是「屏幕高度 − 任务栏高」：任务栏停靠在左/右/上时也成立。
    let up = icon_cy > work.y + work.h / 2.0;

    let mut x = icon_cx - panel_w / 2.0;
    let hi = work.x + work.w - panel_w - margin;
    x = x.clamp(work.x + margin, hi.max(work.x + margin));

    let anchor_y = if up {
        icon.y - gap
    } else {
        icon.y + icon.h + gap
    };

    Placement {
        x,
        anchor_y,
        up,
        top: work.y,
        bottom: work.y + work.h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_speed_keeps_a_constant_width() {
        // 长度恒定是硬要求：菜单栏项按标题宽度自适应，差一个字符整项就会跳
        assert_eq!(menu_speed(0.0), "  0B");
        assert_eq!(menu_speed(9.0), "  9B");
        assert_eq!(menu_speed(512.0), "512B");
        assert_eq!(menu_speed(999.0), "999B");
        // 单位边界两侧都必须还是 4 格
        assert_eq!(menu_speed(1023.0), "1.0K");
        assert_eq!(menu_speed(1024.0 * 820.0), "820K");
        assert_eq!(menu_speed(1024.0 * 9.9), "9.9K");
        assert_eq!(menu_speed(1024.0 * 10.0), " 10K");
        assert_eq!(menu_speed(1024.0 * 1024.0 * 1.25), "1.2M");
        assert_eq!(menu_speed(1024.0 * 1024.0 * 9.99), " 10M");
        assert_eq!(menu_speed(1024.0 * 1024.0 * 120.0), "120M");
        assert_eq!(menu_speed(1024.0 * 1024.0 * 1024.0 * 1.5), "1.5G");
    }

    #[test]
    fn menu_speed_is_always_four_chars() {
        for bps in [
            0.0, 0.4, 1.0, 56.0, 999.0, 999.4, 1000.0, 1023.9, 1024.0,
            1024.0 * 9.94, 1024.0 * 9.96, 1024.0 * 10.0, 1024.0 * 999.0,
            1024.0 * 1024.0 * 1.01, 1024.0 * 1024.0 * 9.99, 1024.0 * 1024.0 * 10.0,
            1024.0 * 1024.0 * 999.0, 1024.0 * 1024.0 * 1024.0 * 9.9,
            1024.0 * 1024.0 * 1024.0 * 999.0, 1e15,
        ] {
            assert_eq!(
                menu_speed(bps).chars().count(),
                4,
                "{} B/s -> {:?}",
                bps,
                menu_speed(bps)
            );
        }
    }

    #[test]
    fn scroll_snaps_to_whole_rows_when_idle() {
        // 用 ROW_H 而不是写死 24：行高一改，这些断言不该跟着失效
        let step = ROW_H;
        let max = step * 4.0;
        // 停在整行：不动
        assert_eq!(snap_scroll(0.0, 0.0, max), 0.0);
        assert_eq!(snap_scroll(step, step, max), step);
        // 停在半行（触控板松手的典型值）：吸到最近的整行
        assert_eq!(snap_scroll(step * 0.4, step * 0.4, max), 0.0);
        assert_eq!(snap_scroll(step * 0.6, step * 0.6, max), step);
        assert_eq!(snap_scroll(step * 1.4, step * 1.4, max), step);
        // 还在滚：这一帧别抢方向盘
        assert_eq!(snap_scroll(step * 0.2, step * 0.9, max), step * 0.9);
        // 吸附值不能越过最大偏移，否则会被 ScrollArea 夹回去，来回抖
        assert_eq!(snap_scroll(max, max, max), max);
        assert_eq!(snap_scroll(max * 1.2, max * 1.2, max), max);
        // 内容比视口短时没有可滚的
        assert_eq!(snap_scroll(0.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn fit_name_keeps_short_names_intact() {
        assert_eq!(fit_name("Google Chrome", NAME_UNITS), "Google Chrome");
        assert_eq!(fit_name("微信", NAME_UNITS), "微信");
    }

    #[test]
    fn fit_name_truncates_wide_text() {
        // 5 个汉字正好 10 个单位，第 6 个放不下 → 截断并补省略号
        assert_eq!(fit_name("很长的中文名字很长的中文名字", 10), "很长的中文…");
        // 22 个单位能装 11 个汉字
        assert_eq!(
            fit_name("一二三四五六七八九十一二", NAME_UNITS).chars().count(),
            NAME_UNITS / 2 + 1,
            "超长中文名应被截到 11 个字加省略号"
        );
    }

    #[test]
    fn tildify_shortens_home() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = format!("{}/Downloads/a.csv", home.to_string_lossy());
            assert_eq!(tildify(&p), "~/Downloads/a.csv");
        }
        assert_eq!(tildify("/opt/x.csv"), "/opt/x.csv");
    }

    fn icon_at(x: f32, y: f32, w: f32, h: f32) -> IconRect {
        IconRect { x, y, w, h }
    }

    #[test]
    fn taskbar_at_bottom_opens_upwards() {
        // 1920x1080、任务栏 40px。图标在任务栏里（工作区之外）
        let icon = icon_at(1800.0, 1040.0, 24.0, 24.0);
        let work = ScreenBox {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1040.0,
        };
        let p = compute_placement(icon, work, 1.0);

        assert!(p.up, "图标在下半屏时面板必须向上长");
        // 底边钉在图标上方 6px：1040 - 6
        assert!((p.anchor_y - 1034.0).abs() < 0.01);

        // 面板整体落在工作区内
        let pos = p.top_left(400.0, 1.0);
        assert!(pos.y >= 0.0);
        assert!(pos.y + 400.0 <= 1040.0);
    }

    #[test]
    fn menu_bar_at_top_opens_downwards() {
        // macOS：图标在菜单栏，工作区从菜单栏下沿开始
        let icon = icon_at(1600.0, 0.0, 24.0, 24.0);
        let work = ScreenBox {
            x: 0.0,
            y: 24.0,
            w: 1920.0,
            h: 1056.0,
        };
        let p = compute_placement(icon, work, 1.0);

        assert!(!p.up, "图标在顶部时面板要向下长");
        assert!((p.anchor_y - 30.0).abs() < 0.01);
        assert!((p.top_left(400.0, 1.0).y - 30.0).abs() < 0.01);
    }

    #[test]
    fn panel_is_clamped_inside_the_work_area() {
        // 图标贴在最右边，居中会伸到屏幕外，必须往里夹
        let icon = icon_at(1900.0, 1040.0, 20.0, 24.0);
        let work = ScreenBox {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1040.0,
        };
        let p = compute_placement(icon, work, 1.0);

        assert!(p.x >= EDGE_MARGIN);
        assert!(
            p.x + WINDOW_W <= 1920.0 - EDGE_MARGIN + 0.01,
            "右侧溢出：x={} 右边缘={}",
            p.x,
            p.x + WINDOW_W
        );
    }

    #[test]
    fn tall_panel_keeps_its_top_edge() {
        // 面板比可用高度还高：优先保住顶边，别把标题栏推到屏幕外
        let icon = icon_at(900.0, 900.0, 24.0, 24.0);
        let work = ScreenBox {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1000.0,
        };
        let p = compute_placement(icon, work, 1.0);
        assert_eq!(
            p.top_left(5000.0, 1.0).y,
            p.top,
            "面板比屏幕还高时应该从顶边开始画"
        );
    }

    #[test]
    fn hi_dpi_placement_is_reported_in_points() {
        // 200% 缩放：图标和工作区都是物理像素，落位也按物理像素存，
        // 只有下发时才换成点
        let icon = icon_at(3600.0, 2080.0, 48.0, 48.0);
        let work = ScreenBox {
            x: 0.0,
            y: 0.0,
            w: 3840.0,
            h: 2080.0,
        };
        let p = compute_placement(icon, work, 2.0);

        assert!(p.up);
        assert!((p.anchor_y - 2068.0).abs() < 0.01, "2080 - 6*2");
        assert!((p.bottom - 2080.0).abs() < 0.01);

        // 400pt 的面板在 200% 下是 800px。底边虽然钉在 2068，
        // 但下边界留 8pt 边距，最终顶边被夹到 (2080 − 800 − 16) = 1264px = 632pt
        let pos = p.top_left(400.0, 2.0);
        assert!((pos.y - 632.0).abs() < 0.01);
        // 换成点之后仍然落在工作区内
        assert!(pos.y >= 0.0 && pos.y + 400.0 <= 1040.0);
    }
}
