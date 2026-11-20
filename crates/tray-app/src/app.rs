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
#[cfg(target_os = "macos")]
use crate::settings::TrayMode;
use crate::settings::{LangMode, Settings, ThemeMode};
use crate::surface;
use crate::theme::{self, Lang, Palette};

pub const WINDOW_W: f32 = 408.0;
/// 面板高度，**固定值**。
///
/// 以前高度是逐帧量内容算出来的，结果：切到设置页会缩一截、应用数一变窗口就抽动。
/// 一个菜单栏弹窗本来就不该自己改尺寸 —— 固定之后，列表装不下就滚动，
/// 设置页底部留白，切页/换数据时窗口一动不动。
///
/// 两页使用相同尺寸，列表按剩余高度显示整行，设置页固定底部开关区。
pub const PANEL_H: f32 = 482.0;
const PANEL_PADDING: i8 = 16;

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
const FOOTER_H: f32 = 28.0;
/// 页脚带 = 空隙 + 分隔线 + 空隙 + 页脚
const FOOTER_BAND: f32 = 8.0 + 1.0 + 7.0 + FOOTER_H;
/// 近 10 分钟超量提醒行的高度（只在真的有提醒时占位）
const ALERT_H: f32 = 20.0;
/// 设置页里的临时提示行高度
const TOAST_H: f32 = 18.0;
/// 启动后多久才去查更新。
///
/// 开机自启的机器上，前几秒要先建窗口、拉起采样线程、扫一遍应用图标，
/// 再塞一个 DNS 解析进去，第一次点面板会明显迟滞。10 秒之后这些都已经安静了。
const UPDATE_FIRST_DELAY: Duration = Duration::from_secs(10);
/// 两次自动检查的最小间隔（秒）。一天一次足够——
/// 这个程序常驻托盘，查太勤既费流量也让人烦。
const UPDATE_INTERVAL_SECS: i64 = 86_400;

/// 每一行的「盒子」高度（含 1px 描边）。
///
/// 23pt 图标与文字共用中线，上下留出稳定的呼吸空间。
const ROW_BOX_H: f32 = 36.0;
/// 行与行之间的外边距。盒子本身有描边，间距不能太小，否则像一条表格。
const ROW_GAP: f32 = 5.0;
/// 列表里每一行占的高度 = 盒子 + 外边距
const ROW_H: f32 = ROW_BOX_H + ROW_GAP;
/// 行盒的圆角比面板小一档。
const ROW_RADIUS: u8 = 10;
/// 行盒左右内边距：让内容离描边有一个稳定的距离
const ROW_PAD_X: f32 = 8.0;
/// 行内「下行速率」列的固定宽度。数字列必须是定宽的：不定宽时一行显示
/// `0`、另一行显示 `1.2 MB/s`，整列数字会左右跳，扫一眼对不齐。
const NUM_W_RX: f32 = 76.0;
/// 上行与下行采用相同的数值、单位宽度。
const NUM_W_TX: f32 = 76.0;
/// 单位（`KB/s`、`MB/s`）占的固定宽度。
///
/// 必须定宽：单位宽度随 K/M 变化，若让数值贴着自己的单位右对齐，各行的
/// 数字就会差一两像素，竖着扫时小数点对不齐。
const NUM_UNIT_W: f32 = 25.0;
/// 两个数字列之间的空隙
const NUM_GAP: f32 = 6.0;
/// 右侧给滚动指示条留的过道。行盒不占这一条，指示条才不会压在描边上。
const SCROLL_GUTTER: f32 = 9.0;
/// 指示条宽度（自绘，不用 egui 自带的浮动把手）
const SCROLL_BAR_W: f32 = 3.0;
/// 应用名最多占用多少显示宽度单位（中日韩字符算 2，字母数字算 1）
#[cfg(test)]
const NAME_UNITS: usize = 22;
/// 顶部两张速率卡片的间隔。
const METRIC_GAP: f32 = 10.0;
/// 时间窗口分段控件的宽度。四档等宽，"1分钟" 也放得下。
const SEG_W: f32 = 218.0;
/// 分段控件高度
const SEG_H: f32 = 28.0;
/// 失焦后多久才真正收起。托盘点击会先让窗口失焦再派发点击事件，
/// 没有这个去抖就会「刚打开又立刻关上」。
const HIDE_DEBOUNCE: Duration = Duration::from_millis(320);
/// 底部提示条显示多久
const TOAST_TTL: Duration = Duration::from_secs(6);

/// 面板圆角（点）。
///
/// 底板由 [`App::draw`] 自己画，**不用 `egui::Frame`**：`Frame` 的背景矩形是
/// 跟着内容的 `min_rect` 走的，内容只要比可视区高出一点点，整块底板就被顶到
/// 窗口外面，底部圆角被裁掉 —— 表现就是「设置页的底部圆角比主面板小/像直角」。
const PANEL_RADIUS: u8 = 16;

/// 主面板 ↔ 设置页的切换时长。
///
/// 0.19s 是「看得出是推过去的，又不至于等着它」的那一档：比面板展开的
/// 0.13s 略长，因为这里有两页同时在动。
const VIEW_ANIM: Duration = Duration::from_millis(190);

/// 切页时两页横向错开的距离（点）。
///
/// 不按整页宽推：一小段位移配淡入淡出
/// 就够读出「换了一页」，也和这个弹窗的体量相称。
const VIEW_SLIDE: f32 = 64.0;

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
    #[cfg(target_os = "macos")]
    PerformanceClick,
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
    rx: engine::Engine,
    tray_rx: Receiver<TrayEvent>,
    latest: Option<Snapshot>,
    visible: bool,
    shown_at: Instant,
    auto_hidden_at: Option<Instant>,
    tray: Option<TrayHandle>,
    #[cfg(target_os = "macos")]
    performance: crate::performance::Monitor,
    #[cfg(target_os = "macos")]
    performance_tray: Option<tm_macos::StatusItem>,
    #[cfg(target_os = "macos")]
    performance_title: String,
    #[cfg(target_os = "macos")]
    performance_tip: String,
    #[cfg(target_os = "macos")]
    performance_tray_retry: Instant,
    #[cfg(target_os = "macos")]
    performance_anchor: bool,
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
    /// 切页动画：`(从哪一页切过来的, 开始时刻)`，播完就清空。
    ///
    /// 只有 `view` 停不住的地方才需要它：切页那一帧两页都得画 ——
    /// 旧页往左淡出、新页从右侧淡入，见 [`App::draw_page`]。
    transition: Option<(View, Instant)>,
    /// 底部临时提示（导出结果、自启切换结果），`(文案, 起始时间)`
    toast: Option<(String, Instant)>,
    /// 同一时刻只运行一个后台导出，避免重复点击覆盖同一文件。
    export_job: Option<crate::export::ExportJob>,
    /// 更新检测。网络动作全在后台线程，这里只是把事件攒成可画的状态。
    update: crate::update::Updater,
    /// 进程启动时刻。
    ///
    /// 自动检查要等启动 10 秒后再发请求：首帧那会儿还在建窗口、连采样线程，
    /// 再塞一个 DNS 解析进去，开机自启的机器上第一次点面板会明显迟滞。
    started_at: Instant,
    /// `--check-update`：跳过「启动延迟 + 24 小时」两个门槛，立刻查一次。
    force_update_check: bool,
    /// 调试用初始滚动位置，只设置一次；后续由 ScrollArea 保留连续滚动状态。
    initial_list_scroll: Option<f32>,
    initial_settings_scroll: Option<f32>,
    /// 调试用：启动后立刻把面板摆出来（`--show` / `TM_SHOW_ON_START`）
    show_on_start: bool,
    /// 显式 --show 用于预览，启动标记被消费后仍保持展开。
    keep_open_on_blur: bool,
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
    /// 任务栏上的网速贴片（Windows 专属，见 [`crate::badge_window`]）
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
        #[cfg(target_os = "macos")]
        let performance = {
            let wake = ctx.clone();
            let mut monitor = crate::performance::Monitor::new(move || wake.request_repaint());
            monitor.configure(settings.performance_enabled, settings.performance_interval);
            monitor
        };

        let app = Self {
            rx,
            tray_rx,
            latest: None,
            visible: false,
            shown_at: Instant::now(),
            auto_hidden_at: None,
            tray: None,
            #[cfg(target_os = "macos")]
            performance,
            #[cfg(target_os = "macos")]
            performance_tray: None,
            #[cfg(target_os = "macos")]
            performance_title: String::new(),
            #[cfg(target_os = "macos")]
            performance_tip: String::new(),
            #[cfg(target_os = "macos")]
            performance_tray_retry: Instant::now(),
            #[cfg(target_os = "macos")]
            performance_anchor: false,
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
            view: if std::env::var("TM_VIEW")
                .map(|v| v == "settings")
                .unwrap_or(false)
            {
                View::Settings
            } else {
                View::Main
            },
            // 首帧不播过渡：从托盘点开就直接是目标页
            transition: None,
            settings,
            retention,
            // 调试用：TM_TOAST="导出 12 行" 让页脚一上来就带着提示，方便核对排版
            toast: std::env::var("TM_TOAST")
                .ok()
                .map(|msg| (msg, Instant::now())),
            export_job: None,
            update: crate::update::Updater::new(),
            started_at: Instant::now(),
            force_update_check: std::env::var_os("TM_FORCE_UPDATE").is_some(),
            // 调试用：TM_SCROLL=140 给列表一个初始位置，方便截图核对滚动后的样子
            initial_list_scroll: std::env::var("TM_SCROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v: &f32| v.is_finite()),
            initial_settings_scroll: std::env::var("TM_SETTINGS_SCROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v: &f32| v.is_finite()),
            show_on_start: std::env::var_os("TM_SHOW_ON_START").is_some(),
            keep_open_on_blur: std::env::var_os("TM_SHOW_ON_START").is_some(),
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
        let app = {
            let mut app = app;
            app.ensure_tray();
            app
        };

        app
    }

    /// 到点就查一次更新。返回 true 表示这一帧刚发起请求。
    ///
    /// 判断放在每帧跑，而不是起一个定时线程：省一个线程，也让「关掉自动检查」
    /// 这种改动立刻生效，不用等定时器下一轮。
    ///
    /// **macOS 上实际是「第一次打开面板时查」**：那边收起面板走
    /// `Visible(false)`，隐藏后 eframe 不再调 `update()`（见 `park_window`），
    /// 这一段自然也不会跑。不是 bug —— 用户点开面板那一刻正好能看到
    /// 「正在检查更新…」，反而比在后台悄悄查更清楚。
    fn maybe_check_update(&mut self) -> bool {
        if self.update.is_busy() {
            return false;
        }
        // `--mock` 是离线演示模式，不该悄悄往外发请求；
        // 显式给了 --check-update 才放行，方便在假数据下核对进度条排版。
        if std::env::var_os("TM_FORCE_MOCK").is_some() && !self.force_update_check {
            return false;
        }
        let due = if self.force_update_check {
            true
        } else if !self.settings.update_check {
            false
        } else {
            self.started_at.elapsed() >= UPDATE_FIRST_DELAY
                && unix_now() - self.settings.last_check >= UPDATE_INTERVAL_SECS
        };
        if !due {
            return false;
        }
        self.force_update_check = false;
        // 先落盘再发请求：查的过程中进程被关掉，下次启动不该立刻再来一次。
        self.settings.last_check = unix_now();
        self.settings.save();
        self.update.check();
        true
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
        #[cfg(target_os = "macos")]
        let work = (if self.performance_anchor {
            &self.performance_tray
        } else {
            &self.tray
        })
        .as_ref()
        .and_then(|tray| tray.work_area())
        .map(|(x, y, w, h)| ScreenBox {
            x: x as f32 * ppp,
            y: y as f32 * ppp,
            w: w as f32 * ppp,
            h: h as f32 * ppp,
        })
        .unwrap_or(screen);
        #[cfg(not(any(windows, target_os = "macos")))]
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
        match self.rx.try_recv() {
            Ok(snap) => self.latest = Some(snap),
            Err(mpsc::TryRecvError::Disconnected) => {
                let snapshot = self.latest.get_or_insert_with(Snapshot::default);
                snapshot.total_rx = 0.0;
                snapshot.total_tx = 0.0;
                snapshot.trend.clear();
                snapshot.rates.clear();
                snapshot.windowed = Default::default();
                snapshot.error = Some(
                    self.lang
                        .t(
                            "采样线程已停止，请重新启动应用",
                            "Sampler stopped; restart the app",
                        )
                        .into(),
                );
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// 状态栏上的内容：悬停提示两个平台都有，文字标题只有 macOS 有。
    fn sync_tray_title(&mut self) {
        let Some(tray) = &self.tray else { return };

        // 悬停提示必须每秒跟着数据走。Windows 的托盘没有文字标题，
        // 这是面板之外唯一能看到实时速率的地方 —— 以前它被 macOS 专用的
        // 那段逻辑一起跳过了，悬停永远只有一句产品名。
        let tip = match &self.latest {
            Some(s) if s.error.is_some() => s.error.clone().unwrap_or_default(),
            Some(s) => format!(
                "↓ {}    ↑ {}",
                format_speed(s.total_rx),
                format_speed(s.total_tx)
            ),
            None => "Tramoing".to_owned(),
        };
        if tip != self.tray_tip {
            #[cfg(target_os = "macos")]
            tray.set_tooltip(Some(tip.as_str()));
            #[cfg(not(target_os = "macos"))]
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
            let key = format!("{mode:?}|{text}|{}", tray.appearance_is_dark());
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

    /// 单个双行性能状态项，与网速相邻；关闭时移除整个项并停止采样。
    #[cfg(target_os = "macos")]
    fn sync_performance_tray(&mut self) {
        if !self.settings.performance_enabled {
            self.performance_tray = None;
            self.performance_title.clear();
            self.performance_tip.clear();
            return;
        }
        if self.tray.is_none() {
            return;
        }
        if self.performance_tray.is_none() && Instant::now() >= self.performance_tray_retry {
            self.performance_tray = tm_macos::StatusItem::new(|| {
                if let Some(tx) = TRAY_TX.get() {
                    let _ = tx.send(TrayEvent::PerformanceClick);
                }
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            });
            self.performance_tray_retry = Instant::now() + Duration::from_secs(5);
        }
        let Some(tray) = &self.performance_tray else {
            return;
        };
        let reading = self.performance.reading();
        let cpu = reading.as_ref().and_then(|r| r.cpu);
        let gpu = reading.as_ref().and_then(|r| r.gpu);
        let cpu_text = crate::performance::menu_percent(cpu);
        let gpu_text = crate::performance::menu_percent(gpu);
        let key = format!(
            "{cpu_text}|{gpu_text}|{}|{}",
            self.settings.performance_color,
            tray.appearance_is_dark()
        );
        if key != self.performance_title {
            tray.set_utilization_lines(&cpu_text, &gpu_text, self.settings.performance_color);
            self.performance_title = key;
        }
        let unavailable = self.lang.t("暂不可用", "Unavailable");
        let exact = |v: Option<f64>| {
            v.map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| unavailable.into())
        };
        let mut tooltip = format!(
            "CPU {}\nGPU {}\n{} {}s",
            exact(cpu),
            exact(gpu),
            self.lang.t("刷新间隔", "Refresh"),
            self.settings.performance_interval
        );
        if gpu.is_none() && reading.is_some() {
            tooltip.push_str(self.lang.t(
                "\nGPU 驱动未提供利用率",
                "\nGPU driver does not report utilization",
            ));
        }
        tooltip.push_str(self.lang.t("\n点击打开设置", "\nClick to open settings"));
        if std::env::var_os("TM_FORCE_MOCK").is_some() {
            tooltip.push_str(self.lang.t("\n演示数据", "\nDemo data"));
        }
        if tooltip != self.performance_tip {
            tray.set_tooltip(Some(&tooltip));
            self.performance_tip = tooltip;
        }
        if std::env::var_os("TM_TRACE").is_some() {
            if let Ok(path) = std::env::var("TM_PERFORMANCE_PNG") {
                eprintln!(
                    "[performance] CPU={cpu_text} GPU={gpu_text} {} dump={}",
                    tray.debug_state(),
                    tray.dump_png(&path)
                );
            }
        }
    }

    /// 推送到任务栏原生子窗口。隐藏和移动随父任务栏生效，溢出托盘图标不影响它。
    #[cfg(windows)]
    fn sync_badge(&mut self, ctx: &egui::Context) {
        use crate::badge_window::BadgeState;

        let state = match &self.latest {
            Some(snap) if self.settings.tray_badge => Some(BadgeState {
                rx: snap.total_rx,
                tx: snap.total_tx,
                light: self.taskbar_light,
                colored: self.settings.tray_color,
            }),
            // 还没第一帧数据或用户关闭任务栏网速。
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
            if self.frame_no.is_multiple_of(30) {
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
        #[cfg(target_os = "macos")]
        self.sync_performance_tray();
        #[cfg(windows)]
        self.sync_badge(ctx);
        self.icons.poll();
        if let Some(job) = &self.export_job {
            let message = match job.try_result() {
                Ok(message) => Some(message),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(
                    self.lang
                        .t("导出任务意外结束", "Export interrupted")
                        .to_owned(),
                ),
            };
            if let Some(message) = message {
                self.export_job = None;
                self.toast = Some((message, Instant::now()));
            }
        }

        if let Some(settings) = crate::instance::take_activation() {
            if settings {
                self.goto(View::Settings);
            }
            if let Some(rect) = self.tray_icon_rect(ctx) {
                self.open(ctx, rect);
            } else {
                self.show_on_start = true;
            }
        }

        // ---- 更新检测 ----
        //
        // 网络动作全在后台线程，这里只吸收结果。失败是静默的（落 ui.log），
        // 断网、公司防火墙这类事用户处理不了，弹出来只会让人以为程序坏了。
        self.update.tick();
        if self.update.poll() {
            ctx.request_repaint();
        }
        if self.maybe_check_update() {
            ctx.request_repaint();
        }
        if self.update.is_busy() {
            // 下载进度挂在 1Hz 心跳上会像卡住，单独提一档；
            // 只是检查时不必这么勤，8 秒总超时，一帧都不用多画。
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        // 调试模式：摆到屏幕右上角，方便截图核对排版
        if self.show_on_start && !self.visible {
            self.show_on_start = false;
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
                TrayEvent::Click { fallback } => {
                    #[cfg(target_os = "macos")]
                    {
                        self.performance_anchor = false;
                    }
                    match self.tray_icon_rect(ctx).or(fallback) {
                        Some(rect) => self.toggle(ctx, rect),
                        // 状态项还没被布局（刚创建的头几帧）就拿不到位置，这次点击直接忽略
                        None => eprintln!("[ui] 状态栏项还没有位置，忽略这次点击"),
                    }
                }
                #[cfg(target_os = "macos")]
                TrayEvent::PerformanceClick => {
                    self.initial_settings_scroll = Some(0.0);
                    if let Some((x, y, w, h)) = self
                        .performance_tray
                        .as_ref()
                        .and_then(|tray| tray.icon_rect())
                    {
                        let ppp = ctx.pixels_per_point().max(1.0);
                        self.performance_anchor = true;
                        let at = IconRect {
                            x: x as f32 * ppp,
                            y: y as f32 * ppp,
                            w: w as f32 * ppp,
                            h: h as f32 * ppp,
                        };
                        if self.visible && self.view != View::Settings {
                            self.goto(View::Settings);
                            self.open(ctx, at);
                        } else {
                            self.goto(View::Settings);
                            self.toggle(ctx, at);
                        }
                    }
                }
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
                && !self.keep_open_on_blur
                && self.shown_at.elapsed() > HIDE_DEBOUNCE
            {
                self.close(ctx);
                self.auto_hidden_at = Some(Instant::now());
            }
        }

        self.draw(ctx);
        #[cfg(debug_assertions)]
        crate::ui_capture::capture(ctx, self.started_at);

        // 展开动画（约 0.3s）和切页动画期间要连续重绘，之后回到 1s 心跳
        if self.reveal_secs() < 0.4 || self.transitioning() {
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

        // 切页进度。播完就把状态清掉，之后按静止的单页画。
        let mut trans = None;
        if let Some((from, at)) = self.transition {
            let p = at.elapsed().as_secs_f32() / VIEW_ANIM.as_secs_f32();
            if p < 1.0 {
                trans = Some((from, p.max(0.0)));
            } else {
                self.transition = None;
            }
        }

        // 底板自己画，不用 `egui::Frame`。
        //
        // `Frame` 的背景矩形 = 内容的 `min_rect` 加内边距（egui frame.rs: `paint(content_rect)`），
        // 也就是说**内容多高，底板就多高**。设置页的行距一改、多一行、
        // 或者行高随字体变一点，底板就会比窗口高出几个点，底部圆角整个滑到
        // 窗口外被裁掉，看起来就是「设置页底部圆角比主面板小一档」。
        // 钉在窗口矩形上画，圆角就永远是圆的。
        let panel = ctx.content_rect();
        let mut background = ctx.layer_painter(egui::LayerId::background());
        background.set_opacity(alpha);
        surface::panel(&background, panel, PANEL_RADIUS, &palette);

        // 只留内边距，不再让 Frame 画任何东西
        let frame = egui::Frame::NONE.inner_margin(egui::Margin::same(PANEL_PADDING));
        let inner = panel.shrink(PANEL_PADDING as f32);

        egui::CentralPanel::default()
            .frame(frame)
            .show(ctx, |ui| match trans {
                // 静止态：只画当前页，位移和额外透明度都是 0
                None => {
                    let view = self.view;
                    self.draw_page(ui, inner, 0.0, alpha, &palette, lang, view);
                }
                // 切页态：两页错开画 —— 旧页先退干净，新页再接上。
                //
                // 不做「两页同时交叉淡入淡出」：两页都是密集的小字，叠在
                // 一起的那几帧会互相透出来，读起来像重影。错开之后中间
                // 只有一瞬空底板，反而像翻过一页。
                Some((from, p)) => {
                    let view = self.view;
                    // 方向跟着「去哪儿」走：进设置是往左推（新页从右边来），
                    // 返回是往右推（新页从左边回来）——和系统里「进入子页 / 退回上一页」
                    // 的手感一致，也让来回切换不会看起来是同一个动作。
                    let dir = if matches!(view, View::Settings) {
                        1.0
                    } else {
                        -1.0
                    };

                    // 旧页：前 45% 淡出，同时往同方向挪一小段（1/3 行程）
                    let out_p = (p / 0.45).clamp(0.0, 1.0);
                    // 新页：从 25% 起淡入，占满剩下的 75%
                    let in_p = ((p - 0.25) / 0.75).clamp(0.0, 1.0);
                    let k = ease_out_cubic(in_p);

                    self.draw_page(
                        ui,
                        inner,
                        -ease_out_cubic(out_p) * VIEW_SLIDE * 0.35 * dir,
                        alpha * (1.0 - out_p),
                        &palette,
                        lang,
                        from,
                    );
                    self.draw_page(
                        ui,
                        inner,
                        (1.0 - k) * VIEW_SLIDE * dir,
                        alpha * k,
                        &palette,
                        lang,
                        view,
                    );
                }
            });
    }

    /// 画一页。`dx` 是横向临时位移，`opacity` 是整页不透明度。
    ///
    /// 页面内容统一在这里裁到内边距以内：切页时文字滑到面板边缘会被切掉，
    /// 而不是飘到圆角外面的空白里。
    #[allow(clippy::too_many_arguments)]
    fn draw_page(
        &mut self,
        ui: &mut egui::Ui,
        inner: egui::Rect,
        dx: f32,
        opacity: f32,
        palette: &Palette,
        lang: Lang,
        view: View,
    ) {
        if opacity <= 0.001 {
            return;
        }
        let rect = inner.translate(egui::vec2(dx, 0.0));
        // id_salt 按页区分：两页的控件 id 不能撞车（否则开关/滑块的动画状态会串）
        let salt = match view {
            View::Main => "page-main",
            View::Settings => "page-settings",
        };
        let mut page = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(rect));
        page.set_clip_rect(inner);
        page.set_opacity(opacity);
        page.set_width(WINDOW_W - 2.0 * PANEL_PADDING as f32);
        // 两页的纵向间距显式分配，避免字体高度与默认 item_spacing 叠加溢出。
        page.spacing_mut().item_spacing.y = 0.0;
        match view {
            View::Main => self.main_body(&mut page, palette, lang),
            View::Settings => self.settings_body(&mut page, palette, lang),
        }
    }

    /// 换页。带过渡动画；已经在目标页就什么都不做（连点两次设置不会重播）。
    fn goto(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.transition = Some((self.view, Instant::now()));
        self.view = view;
    }

    /// 切页动画是否还在播（`update` 靠它决定这帧要不要连续重绘）。
    fn transitioning(&self) -> bool {
        self.transition
            .is_some_and(|(_, at)| at.elapsed() < VIEW_ANIM)
    }

    /// 展开动画的进度 0..1（已缓动）。窗口没显示时恒为 0。
    fn open_alpha(&self) -> f32 {
        if !self.visible {
            return 0.0;
        }
        let elapsed = self
            .opened_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(1.0);
        ease_out_cubic((elapsed / 0.13).clamp(0.0, 1.0))
    }

    /// 距展开已经过去多久（秒）。逐行浮现按它算延迟。
    fn reveal_secs(&self) -> f32 {
        self.opened_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(1.0)
    }

    /// 主面板。
    ///
    /// 标题、两张速率卡片、应用列表（含时间窗口和列标题）、固定页脚。
    ///
    /// 高度是固定的（[`PANEL_H`]），所以列表吃掉「剩下多少」，页脚靠底。
    fn main_body(&mut self, ui: &mut egui::Ui, palette: &Palette, lang: Lang) {
        let p = palette;
        let reveal = self.reveal_secs();

        self.header(ui, p, lang);
        ui.add_space(12.0);
        self.totals(ui, p, lang);
        ui.add_space(18.0);

        // 时间窗口紧贴列表：它只决定列表按哪段时间排序，控件离内容越近越好。
        self.list_header(ui, p, lang);
        ui.add_space(10.0);
        self.list_columns(ui, p, lang);
        ui.add_space(6.0);
        // 提醒行有没有内容要在排版前就知道：它占一行高度，列表得先让出来。
        // 以前是无条件预留 22pt，结果没提醒时列表底下永远空一截。
        let alert = self.alert_text(lang);
        self.list(ui, p, lang, reveal, alert.is_some());
        if let Some(text) = &alert {
            self.alert_line(ui, p, text);
        }

        // 页脚自己按面板底边定位（含分隔线），这里不再补空隙：
        // 以前是 add_space + hairline + add_space 拼出来的，加上 item_spacing
        // 实际比 FOOTER_BAND 高出十来点，页脚就这样被顶出了面板底边
        self.footer(ui, p, lang);
    }

    /// 设置页。
    fn settings_body(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let zh = matches!(lang, Lang::Zh);
        let mut changed = false;

        let bounds = ui.max_rect();
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::hover());
        let back_rect = egui::Rect::from_min_size(header.min, Vec2::splat(30.0));
        if icon_button(ui, back_rect, "settings-back", p, paint_back_icon)
            .on_hover_text(lang.t("返回", "Back"))
            .clicked()
        {
            self.goto(View::Main);
        }
        ui.painter().text(
            egui::pos2(header.left() + 40.0, header.center().y),
            egui::Align2::LEFT_CENTER,
            lang.t("设置", "Settings"),
            theme::display_font(17.0),
            p.text,
        );
        let version_rect =
            egui::Rect::from_min_max(egui::pos2(header.right() - 58.0, header.top()), header.max);
        let version = ui
            .interact(
                version_rect,
                ui.id().with("version-check"),
                egui::Sense::click(),
            )
            .on_hover_text(lang.t("检查更新", "Check for updates"));
        ui.painter()
            .rect_filled(version_rect.shrink2(egui::vec2(0.0, 3.0)), 12.0, p.chip);
        surface::focus(ui.painter(), version_rect, &version, p);
        if version.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        ui.painter().text(
            version_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("v{}", crate::update::CURRENT),
            egui::FontId::proportional(10.5),
            if version.hovered() { p.accent } else { p.unit },
        );
        if version.clicked() {
            self.update.check();
        }
        ui.add_space(12.0);

        // 中间选项独立滚动，标题、返回和开机自启始终留在原位。
        let options_height = (bounds.bottom() - 42.0 - TOAST_H - 15.0 - ui.cursor().top()).max(1.0);
        ui.spacing_mut().scroll.floating_width = 3.0;
        settings_scroll_area(options_height, self.initial_settings_scroll.take()).show(ui, |ui| {
            ui.set_width(ui.available_width() - 8.0);
            #[cfg(target_os = "macos")]
            {
                self.performance_options(ui, p, lang, &mut changed);
                ui.add_space(12.0);
            }
            settings_group(ui, p, lang.t("显示", "Display"), |ui| {
                // ---- 菜单栏（只有 macOS 有这回事）----
                //
                // Windows 的任务栏托盘没有文字标题，这两项在那边是纯装饰：
                // 调了不会有任何变化。宁可少两行，也不能给用户一个假的开关。
                #[cfg(target_os = "macos")]
                {
                    if option_row(
                        ui,
                        p,
                        lang.t("网速显示", "Network speed"),
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
                    // 彩色时下行绿、上行蓝；单色时全部跟随系统标签色。
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

                // ---- 任务栏（只有 Windows 有这回事）----
                //
                // Windows 数字由原生任务栏子窗口绘制，这里控制显示和配色。
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
            });
            ui.add_space(12.0);
            settings_group(ui, p, lang.t("统计", "Statistics"), |ui| {
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
            });
        });

        // 提示与开关区分别固定在底部，设置行不会因状态文案出现而移动。
        let line_y = bounds.bottom() - 42.0;
        let toast_rect = egui::Rect::from_min_size(
            egui::pos2(bounds.left() + 2.0, line_y - TOAST_H - 7.0),
            Vec2::new(bounds.width() - 4.0, TOAST_H),
        );
        let mut status = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("settings-status")
                .max_rect(toast_rect),
        );
        self.toast_line(&mut status, p, lang);
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(bounds.left(), line_y),
                egui::pos2(bounds.right(), line_y + 1.0),
            ),
            0.0,
            p.border,
        );
        let row = egui::Rect::from_min_max(
            egui::pos2(bounds.left() + 2.0, line_y + 6.0),
            egui::pos2(bounds.right() - 2.0, bounds.bottom()),
        );

        // ---- 开机自启（状态来自系统，不是配置文件）----
        let mut on = self.launch_at_login;
        if toggle_row_at(ui, row, p, lang.t("开机自启", "Launch at login"), &mut on) {
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

    #[cfg(target_os = "macos")]
    fn performance_options(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        changed: &mut bool,
    ) {
        let top = ui.cursor().top();
        settings_group(
            ui,
            p,
            lang.t("菜单栏性能", "Menu bar performance"),
            |ui| {
                *changed |= option_row(
                    ui,
                    p,
                    "CPU / GPU",
                    &mut self.settings.performance_enabled,
                    &[(false, lang.t("关闭", "Off")), (true, lang.t("开启", "On"))],
                );
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("刷新间隔", "Refresh"),
                    &mut self.settings.performance_interval,
                    &[
                        (1, lang.t("1 秒", "1s")),
                        (2, lang.t("2 秒", "2s")),
                        (5, lang.t("5 秒", "5s")),
                    ],
                );
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("性能配色", "Colors"),
                    &mut self.settings.performance_color,
                    &[
                        (true, lang.t("彩色", "Color")),
                        (false, lang.t("单色", "Mono")),
                    ],
                );
            },
        );
        let line = egui::Rect::from_min_size(
            egui::pos2(ui.max_rect().right() - 158.0, top),
            Vec2::new(158.0, 18.0),
        );
        let reading = self.performance.reading();
        let text = if self.performance.error.is_some() {
            lang.t("采样未启动", "Sampler unavailable").to_owned()
        } else if self.settings.performance_enabled {
            format!(
                "CPU {}    GPU {}",
                crate::performance::menu_percent(reading.as_ref().and_then(|r| r.cpu)),
                crate::performance::menu_percent(reading.as_ref().and_then(|r| r.gpu))
            )
        } else {
            lang.t("上排 CPU · 下排 GPU", "CPU above GPU").to_owned()
        };
        ellipsis_text(ui.painter(), line, &text, 10.5, p.subtext, Align::Max);
        let hint = if self.performance.error.is_some() {
            lang.t(
                "性能采样启动失败，请关闭后重新开启",
                "Could not start; toggle off and on",
            )
        } else if !self.settings.performance_enabled {
            lang.t(
                "开启后在网速旁以同一组两行显示 CPU 和 GPU 使用率",
                "Show CPU above GPU in one item beside network speed",
            )
        } else if reading.as_ref().is_some_and(|r| r.gpu.is_none()) {
            lang.t(
                "GPU 驱动未提供利用率，使用 --% 表示不可用",
                "GPU driver does not report utilization; --% means unavailable",
            )
        } else {
            lang.t(
                "CPU 为整机平均占用；多 GPU 显示最忙设备",
                "CPU is the system average; GPU is the busiest device",
            )
        };
        ui.interact(line, ui.id().with("performance-hint"), egui::Sense::hover())
            .on_hover_text(hint);
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
        #[cfg(target_os = "macos")]
        {
            self.performance.configure(
                self.settings.performance_enabled,
                self.settings.performance_interval,
            );
            self.performance_title.clear();
            self.performance_tray_retry = Instant::now();
            self.sync_performance_tray();
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        // 优先显示历史库里的 24 小时合计；库还没写进数据时退回本次运行
        let (day_rx, day_tx) = self
            .latest
            .as_ref()
            .map(|s| (s.day_rx, s.day_tx))
            .unwrap_or((0, 0));
        let (srx, stx) = self.session();
        let has_day = day_rx > 0 || day_tx > 0;
        let (rx, tx) = if has_day {
            (day_rx, day_tx)
        } else {
            (srx, stx)
        };
        let mut open_settings = false;
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.spacing_mut().item_spacing.x = 6.0;
            let (mark, _) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::hover());
            surface::network_mark(ui.painter(), mark, p);
            ui.label(
                RichText::new(lang.t("网络流量", "Network"))
                    .font(theme::display_font(17.0))
                    .color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let entry = sliders_button(ui, p).on_hover_text(lang.t("设置", "Settings"));
                if entry.clicked() {
                    open_settings = true;
                }
                ui.add_space(4.0);
                // 第一帧数据还没到时整块省略：写「本次 0 B」既不准确也没意义
                if self.latest.is_some() {
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            if has_day {
                                lang.t("24小时", "24h")
                            } else {
                                lang.t("本次", "Session")
                            },
                            format_bytes(rx.saturating_add(tx))
                        ))
                        .size(12.0)
                        .color(p.subtext),
                    );
                }
            });
        });
        if open_settings {
            self.goto(View::Settings);
        }
    }

    /// 两张等宽速率卡片，数值和单位分开绘制。
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

        const H: f32 = 102.0;
        let trend = self
            .latest
            .as_ref()
            .map(|s| s.trend.as_slice())
            .unwrap_or(&[]);
        let rx_trend: Vec<_> = trend.iter().map(|v| v[0]).collect();
        let tx_trend: Vec<_> = trend.iter().map(|v| v[1]).collect();
        let col = ((ui.available_width() - METRIC_GAP) / 2.0).max(60.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            metric(
                ui,
                col,
                H,
                "↓",
                lang.t("下载", "Download"),
                rx_text,
                &rx_trend,
                p.download,
                p,
            )
            .on_hover_text(lang.t(
                "最近的下载趋势 · 独立刻度",
                "Recent download activity · independent scale",
            ));

            ui.add_space(METRIC_GAP);

            metric(
                ui,
                col,
                H,
                "↑",
                lang.t("上传", "Upload"),
                tx_text,
                &tx_trend,
                p.upload,
                p,
            )
            .on_hover_text(lang.t(
                "最近的上传趋势 · 独立刻度",
                "Recent upload activity · independent scale",
            ));
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
                RichText::new(format!("{} · {count}", lang.t("应用", "Apps")))
                    .font(theme::display_font(14.0))
                    .color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(SEG_W, SEG_H), egui::Sense::hover());
                segmented_at(ui, rect, &mut self.window, p, lang);
            });
        });
    }

    fn list_columns(&self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), egui::Sense::hover());
        let tx_right = rect.right() - SCROLL_GUTTER - ROW_PAD_X - 18.0;
        let rx_right = tx_right - NUM_W_TX - NUM_GAP;
        for (x, label, align) in [
            (
                rect.left() + ROW_PAD_X,
                lang.t("应用名称", "Application"),
                egui::Align2::LEFT_CENTER,
            ),
            (
                rx_right,
                lang.t("↓ 下载", "↓ Down"),
                egui::Align2::RIGHT_CENTER,
            ),
            (
                tx_right,
                lang.t("↑ 上传", "↑ Up"),
                egui::Align2::RIGHT_CENTER,
            ),
        ] {
            ui.painter().text(
                egui::pos2(x, rect.center().y),
                align,
                label,
                egui::FontId::proportional(11.5),
                p.unit,
            );
        }
    }

    fn list(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, reveal: f32, has_alert: bool) {
        // 行高、间距及页脚预留统一计算，视口高度按整行分配，滚动偏移保持连续。
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
        } else if self.latest.as_ref().is_some_and(|s| s.error.is_some()) {
            Some(lang.t(
                "采集暂时中断，正在重新连接",
                "Sampling interrupted; reconnecting",
            ))
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
            let out =
                app_list_scroll_area(list_h, self.initial_list_scroll.take()).show(ui, |ui| {
                    // 让出右侧过道：行盒的描边不能和指示条叠在一起
                    ui.set_width(ui.available_width() - SCROLL_GUTTER);
                    // 行距自己控制，不叠加默认 item_spacing。
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

            let now = out.state.offset.y;
            let max_off = (out.content_size.y - out.inner_rect.height()).max(0.0);

            // 自绘滚动指示条：3pt 宽，贴在行盒右侧的过道里。
            // 用 egui 自带的浮动把手会压住行盒的描边，在「每行一张小卡」
            // 的版式里格外明显；自己画一条细的最省事，也不抢视线。
            if max_off > 1.0 {
                let track = out.inner_rect;
                let frac = (track.height() / out.content_size.y).clamp(0.06, 1.0);
                let h = (track.height() * frac).max(24.0);
                let y = track.top() + (track.height() - h) * (now / max_off).clamp(0.0, 1.0);
                let x = ui.max_rect().right() - SCROLL_BAR_W;
                let color = if p.panel.r() < 128 {
                    Color32::from_rgba_unmultiplied(255, 255, 255, 70)
                } else {
                    Color32::from_rgba_unmultiplied(0, 0, 0, 60)
                };
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, y), Vec2::new(SCROLL_BAR_W, h)),
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
        let width = ui.available_width() - SCROLL_GUTTER;
        let rows = rows.max(1);
        // 骨架与真实列表使用相同行高和间距。
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
                p.surface,
                Stroke::new(0.5, p.border.gamma_multiply(pulse * fade)),
                egui::StrokeKind::Inside,
            );
            let bar = |x: f32, w: f32| {
                egui::Rect::from_min_size(egui::pos2(x, rect.center().y - 4.5), Vec2::new(w, 9.0))
            };
            // 图标位：和真实行的 18pt 方块对齐
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(
                        rect.left() + ROW_PAD_X,
                        rect.center().y - icons::ICON_PT / 2.0,
                    ),
                    Vec2::splat(icons::ICON_PT),
                ),
                CornerRadius::same(6),
                color,
            );
            let right = rect.right() - ROW_PAD_X;
            let tx_x = right - 18.0 - NUM_W_TX;
            let rx_x = tx_x - NUM_GAP - NUM_W_RX;
            // 名字长度按行号错开（走一个固定序列，不是随机），
            // 否则每帧变一次，骨架会自己抖
            let name_left = rect.left() + ROW_PAD_X + icons::ICON_PT + 9.0;
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
    /// 速率固定列并右对齐；应用名使用剩余宽度按真实字体省略。
    fn row(&mut self, ui: &mut egui::Ui, r: &AppRate, p: &Palette) {
        let name = pretty_name(r.app.as_str());
        // 系统守护进程没有 `.app`，NSWorkspace 什么都取不到：走齿轮，
        // 也就不用把它们排进后台队列白跑一趟
        let service = tm_core::is_system_service(r.app.as_str());
        let tex = if service {
            None
        } else {
            self.icons.get(r.app.as_str())
        };
        let dark = p.dark;

        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_BOX_H), egui::Sense::hover());
        // 排名会变化，点击状态与应用键绑定，不能用行号作 ID。
        let resp = ui.interact(
            rect,
            ui.id().with(("app-details", r.app.as_str())),
            egui::Sense::click(),
        );

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
            mix(p.surface, p.accent, t * 0.08),
            Stroke::new(0.7, mix(p.border, p.accent, t * 0.5)),
            egui::StrokeKind::Inside,
        );

        // 数字不贴到描边上
        let right = rect.right() - ROW_PAD_X;

        // 图标：真实图标直接贴；系统服务发齿轮；其余拿不到就用低饱和的字母块
        let icon = egui::Rect::from_min_size(
            egui::pos2(
                rect.left() + ROW_PAD_X,
                rect.center().y - icons::ICON_PT / 2.0,
            ),
            Vec2::splat(icons::ICON_PT),
        );
        if service {
            let gear = self.icons.service_icon();
            service_badge(painter, icon, &gear, p, dark);
        } else {
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
        }

        // 两列速率的右边界：先定数字，名字再去吃剩下的宽度
        let tx_right = right - 18.0;
        let rx_right = tx_right - NUM_W_TX - NUM_GAP;
        let name_left = icon.right() + 9.0;

        let name_rect = egui::Rect::from_min_max(
            egui::pos2(name_left, rect.top()),
            egui::pos2(rx_right - NUM_W_RX - 5.0, rect.bottom()),
        );
        ellipsis_text(painter, name_rect, &name, 13.0, p.text, Align::Min);
        let arrow_center = egui::pos2(right - 3.0, rect.center().y);
        painter.add(egui::Shape::line(
            vec![
                arrow_center + egui::vec2(-2.0, -4.0),
                arrow_center + egui::vec2(2.0, 0.0),
                arrow_center + egui::vec2(-2.0, 4.0),
            ],
            Stroke::new(1.2, p.unit),
        ));
        surface::focus(painter, rect, &resp, p);
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        rate_cell(
            painter,
            rx_right,
            rect.center().y,
            r.rx_bps,
            14.0,
            p.download,
            p.unit,
        );
        rate_cell(
            painter,
            tx_right,
            rect.center().y,
            r.tx_bps,
            14.0,
            p.upload,
            p.unit,
        );
        let resp = resp.on_hover_text(format!("{}\n{}", name, tildify(r.app.as_str())));
        egui::Popup::from_toggle_button_response(&resp)
            .width(280.0)
            .frame(
                egui::Frame::popup(ui.style())
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(12)
                    .inner_margin(12),
            )
            .show(|ui| {
                ui.label(
                    RichText::new(&name)
                        .font(theme::display_font(14.0))
                        .color(p.text),
                );
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(tildify(r.app.as_str()))
                            .size(11.0)
                            .color(p.subtext),
                    )
                    .wrap(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("↓ {}", format_speed(r.rx_bps))).color(p.download),
                    );
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(format!("↑ {}", format_speed(r.tx_bps))).color(p.upload),
                    );
                });
            });
    }

    /// 近 10 分钟单应用超过阈值时给一行提醒。阈值没配就什么都不画。
    ///
    /// 使用十分钟窗口里的实际字节累计值；文字过长时省略，悬停可读全文。
    fn alert_line(&self, ui: &mut egui::Ui, p: &Palette, text: &str) {
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ALERT_H),
            egui::Sense::hover(),
        );
        response.on_hover_text(text);
        let demo = text
            == self.lang.t(
                "演示数据 · 不写入历史记录",
                "Demo data · history is not saved",
            );
        let color = if demo { p.subtext } else { p.warn };
        ui.painter()
            .circle_filled(egui::pos2(rect.left() + 4.0, rect.center().y), 2.5, color);
        ellipsis_text(
            ui.painter(),
            egui::Rect::from_min_max(egui::pos2(rect.left() + 13.0, rect.top()), rect.max),
            text,
            10.0,
            color,
            Align::Min,
        );
    }

    /// 提醒文案。没有超阈值的应用时返回 `None`。
    fn alert_text(&self, lang: Lang) -> Option<String> {
        #[cfg(windows)]
        if let Some(error) = self.badge.error {
            return Some(error.into());
        }
        if let Some(s) = &self.latest {
            if let Some(e) = &s.error {
                return Some(e.clone());
            }
            if let Some(e) = &s.store_error {
                return Some(e.clone());
            }
            if s.mock {
                return Some(
                    lang.t(
                        "演示数据 · 不写入历史记录",
                        "Demo data · history is not saved",
                    )
                    .into(),
                );
            }
        }
        let threshold = self.settings.alert_mb;
        if threshold == 0 {
            return None;
        }
        let snap = self.latest.as_ref()?;
        let limit = threshold.saturating_mul(1024 * 1024);

        let worst = snap
            .windowed
            .min10_bytes
            .iter()
            .map(|(app, rx, tx)| (app.as_str(), rx.saturating_add(*tx)))
            .filter(|(_, bytes)| *bytes >= limit)
            .max_by_key(|(_, bytes)| *bytes)?;

        let (key, bytes) = worst;
        let name = fit_name(&pretty_name(key), 16);
        Some(match lang {
            Lang::Zh => format!("⚠ 近 10 分钟 {name} 已用 {}", format_bytes(bytes)),
            Lang::En => format!("⚠ {name} used {} in 10 min", format_bytes(bytes)),
        })
    }

    /// 页脚：一条淡分隔线 + 两个图标按钮（导出历史 / 退出），临时提示画在中间。
    ///
    /// **整条带子只按面板底边定位、内容全靠 painter 落笔**。以前是一条流式的
    /// `add_space(8) + hairline + add_space(4) + 两个 Button`：每次 `allocate_*`
    /// 都会再插一次 `item_spacing`，加上按钮高度跟着字形走（中文 10.5pt 与英文
    /// 字高不同），页脚实际高度比 [`FOOTER_BAND`] 多出十几点，把内容顶出了
    /// 面板底边；换语言、换字号时还会变一次。现在高度恒等于 [`FOOTER_BAND`]。
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

        // 顶在面板底边：不跟游标走，列表怎么滚都不影响这条带子的位置
        let full = ui.max_rect();
        let band = egui::Rect::from_min_max(
            egui::pos2(full.left(), full.bottom() - FOOTER_BAND),
            full.max,
        );

        let line_y = band.top() + 8.0;
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(band.left(), line_y),
                egui::pos2(band.right(), line_y + 1.0),
            ),
            0.0,
            p.border,
        );

        // 图标那一行：分隔线下面空 4pt 起，到面板底边为止
        let row = egui::Rect::from_min_max(egui::pos2(band.left(), line_y + 1.0 + 7.0), band.max);
        let cy = row.center().y;

        let export_width = if self.export_job.is_some() {
            106.0
        } else {
            96.0
        };
        let export_rect = egui::Rect::from_min_size(row.min, Vec2::new(export_width, row.height()));
        let export = action_button(
            ui,
            export_rect,
            "footer-export",
            if self.export_job.is_some() {
                lang.t("导出中…", "Exporting…")
            } else {
                lang.t("导出历史", "Export")
            },
            p,
            paint_export_icon,
        )
        .on_hover_text(lang.t("导出历史", "Export history"));
        if export.clicked() && self.export_job.is_none() {
            self.export_history(zh, ui.ctx());
        }

        // 页脚中间这一条：有更新相关的事就显示更新，否则退回临时提示。
        //
        // 更新优先——用户点「检查更新」就是为了看结果，被一条三秒前的
        // 「已导出 12 行」盖住，会像是点了没反应。
        let left = export_rect.right() + 8.0;
        let right = row.right() - 64.0 - 8.0;
        if !self.update_strip(ui, p, lang, left, right, cy) {
            if let Some(msg) = &toast {
                if right - left > 40.0 {
                    ellipsis_text(
                        ui.painter(),
                        egui::Rect::from_min_max(
                            egui::pos2(left, row.top()),
                            egui::pos2(right, row.bottom()),
                        ),
                        msg,
                        10.0,
                        p.subtext,
                        Align::Center,
                    );
                }
            }
        }

        let quit_rect = egui::Rect::from_min_size(
            egui::pos2(row.right() - 64.0, row.top()),
            Vec2::new(64.0, row.height()),
        );
        let quit = action_button(
            ui,
            quit_rect,
            "footer-quit",
            lang.t("退出", "Quit"),
            p,
            paint_quit_icon,
        )
        .on_hover_text(lang.t("退出", "Quit"));
        if quit.clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 更新状态对应的文案。`None` 表示这一帧没有要显示的。
    ///
    /// 抽出来是因为主面板页脚和设置页提示行都要画它：点设置页的版本号
    /// 也能触发检查，那边如果什么都不显示，用户会以为按钮坏了。
    fn update_text(&self, lang: Lang) -> Option<(String, bool)> {
        use crate::update::Phase;
        let zh = matches!(lang, Lang::Zh);
        let (text, actionable) = match &self.update.phase {
            Phase::Idle => return None,
            Phase::Checking => (lang.t("正在检查更新…", "Checking…").to_owned(), false),
            Phase::Latest => (lang.t("已是最新版本", "Up to date").to_owned(), false),
            Phase::Available(a) => (
                if zh {
                    format!("v{} 可用 · 点此下载", a.version)
                } else {
                    format!("v{} available · click", a.version)
                },
                true,
            ),
            Phase::Downloading { version, pct } => (
                if zh {
                    format!("正在下载 v{} · {:.0}%", version, pct * 100.0)
                } else {
                    format!("Downloading v{} · {:.0}%", version, pct * 100.0)
                },
                false,
            ),
            Phase::Ready { version, .. } => (
                if zh {
                    format!("v{} 已就绪 · 点此安装", version)
                } else {
                    format!("v{} ready · click to install", version)
                },
                true,
            ),
            Phase::Error(e) => (e.clone(), false),
        };
        Some((text, actionable))
    }

    /// 点更新提示之后干什么。两个入口（页脚、设置页）共用。
    fn update_action(&mut self) {
        use crate::update::Phase;
        // 先拷一份：下面要 &mut self，不能挂着对 phase 的借用
        match &self.update.phase.clone() {
            // 清单里没有本平台的包时退而求其次：打开下载页。
            // 直接什么都不做的话，用户看到「可点」却点不动，比不提示更糟。
            Phase::Available(a) if a.asset.is_none() => {
                if let Err(e) = crate::update::open_url(&a.homepage) {
                    self.toast = Some((e, Instant::now()));
                }
            }
            Phase::Available(a) => self.update.download(a),
            Phase::Ready { path, .. } => {
                if let Err(e) = crate::update::run_installer(path) {
                    self.toast = Some((e, Instant::now()));
                }
            }
            _ => {}
        }
    }

    /// 页脚中间的更新提示。返回 `true` 表示这一帧它占用了提示区。
    ///
    /// 只有「有新版本 / 下载中 / 可安装 / 刚查完」才出现。这一条**不占额外高度**：
    /// 面板高度是写死的，在列表上方加横幅会把最后一行挤出去；页脚中间本来是
    /// 临时提示的位置，复用最安全。
    ///
    /// 可点的两种状态（可下载、可安装）才注册 click；其余用 `Sense::hover`，
    /// 免得用户点「正在下载」反而把状态搅乱。
    fn update_strip(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        left: f32,
        right: f32,
        cy: f32,
    ) -> bool {
        use crate::update::Phase;
        let Some((text, actionable)) = self.update_text(lang) else {
            return false;
        };
        if right - left < 40.0 {
            return true;
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(left, cy - FOOTER_H / 2.0),
            egui::pos2(right, cy + FOOTER_H / 2.0),
        );
        let sense = if actionable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let mut resp = ui.allocate_rect(rect, sense);
        if actionable {
            resp = resp.on_hover_text(lang.t("点击继续", "Click to continue"));
        }
        let color = if matches!(&self.update.phase, Phase::Error(_)) {
            p.warn
        } else if actionable && resp.hovered() {
            // 可点的那两种状态用强调色：页脚平时全是灰字，
            // 只有这里需要用户动手，值得抢一次注意力。
            p.accent
        } else {
            p.subtext
        };
        ellipsis_text(ui.painter(), rect, &text, 10.0, color, Align::Center);

        if actionable && resp.clicked() {
            self.update_action();
        }
        true
    }

    /// 设置页的提示行。定高，过期自动消失。
    ///
    /// 更新状态优先于普通提示：版本号就挂在这一页的标题栏上，点完必须有个回声，
    /// 否则用户不知道是「还在查」还是「按钮是假的」。
    fn toast_line(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
        }
        let update = self.update_text(lang);
        let actionable = update.as_ref().is_some_and(|(_, a)| *a);
        let rect =
            egui::Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), TOAST_H));
        let sense = if actionable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let mut resp = ui.allocate_rect(rect, sense);
        if actionable {
            resp = resp.on_hover_text(lang.t("点击继续", "Click to continue"));
        }

        if let Some((text, _)) = &update {
            let color = if matches!(&self.update.phase, crate::update::Phase::Error(_)) {
                p.warn
            } else if actionable && resp.hovered() {
                p.accent
            } else {
                p.subtext
            };
            ellipsis_text(ui.painter(), rect, text, 10.0, color, Align::Min);
            if actionable && resp.clicked() {
                self.update_action();
            }
            return;
        }

        let Some((msg, _)) = &self.toast else { return };
        ellipsis_text(ui.painter(), rect, msg, 10.0, p.subtext, Align::Min);
    }

    /// 把库里全部历史写成 CSV。
    fn export_history(&mut self, zh: bool, ctx: &egui::Context) {
        let ctx = ctx.clone();
        match crate::export::ExportJob::start(
            move || export_history_csv(zh),
            move || ctx.request_repaint(),
        ) {
            Ok(job) => {
                self.export_job = Some(job);
                self.toast = Some((
                    self.lang
                        .t("正在导出历史…", "Exporting history…")
                        .to_owned(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                self.toast = Some((
                    format!("{}: {error}", self.lang.t("导出失败", "Export failed")),
                    Instant::now(),
                ));
            }
        }
    }
}

/// 导出全部历史，返回可以直接展示的提示文案。
///
/// 界面在后台任务中调用；`--export` 命令行入口直接等待结果。
pub fn export_history_csv(zh: bool) -> String {
    let result = (|| -> Result<(u64, String), String> {
        let path = engine::db_path();
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok((0, String::new()));
        }
        let store = tm_store::Store::open_read_only(&path).map_err(|e| e.to_string())?;
        if store.row_count().map_err(|e| e.to_string())? == 0 {
            return Ok((0, String::new()));
        }
        let stamp = store.local_now_stamp().map_err(|e| e.to_string())?;

        // 先写「下载」目录；被 TCC 拒绝（用户点了不允许）就退到应用自己的目录，
        // 这两个位置都试过才算真失败。
        let mut last_err = String::new();
        for dir in [
            tm_store::default_export_dir(),
            tm_store::data_dir().join("exports"),
        ] {
            for suffix in 0..1000 {
                let name = if suffix == 0 {
                    format!("Tramoing-{stamp}.csv")
                } else {
                    format!("Tramoing-{stamp}-{suffix}.csv")
                };
                let path = dir.join(name);
                match store.export_csv(0, i64::MAX, &path) {
                    Ok(rows) => return Ok((rows, tildify(&path.to_string_lossy()))),
                    Err(tm_store::StoreError::Io(e))
                        if e.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        last_err = e.to_string();
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        break;
                    }
                }
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

/// 一行内按实际字体宽度省略；禁止长名字或状态文字覆盖相邻控件。
fn ellipsis_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    text: &str,
    size: f32,
    color: Color32,
    align: Align,
) {
    if rect.width() <= 0.0 {
        return;
    }
    let mut job = egui::text::LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(size),
        color,
        rect.width(),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    let x = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x - galley.size().x / 2.0,
        Align::Max => rect.right() - galley.size().x,
    };
    let y = rect.center().y - galley.size().y / 2.0;
    painter
        .with_clip_rect(rect.intersect(painter.clip_rect()))
        .galley(egui::pos2(x, y), galley, color);
}

fn settings_group(ui: &mut egui::Ui, p: &Palette, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    let (label, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), egui::Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(
            label.left_center() + egui::vec2(3.0, 0.0),
            Vec2::new(3.0, 10.0),
        ),
        1.5,
        p.accent,
    );
    ui.painter().text(
        label.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        theme::display_font(12.0),
        p.subtext,
    );
    ui.add_space(4.0);
    let width = ui.available_width();
    egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(0.7, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(width - 21.4);
            ui.spacing_mut().item_spacing.y = 0.0;
            body(ui);
        });
}

/// 设置项：左边标签，右边一组互斥选项。返回是否被改动。
fn option_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    const ROW_H: f32 = 34.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), egui::Sense::hover());
    let painter = ui.painter();

    // 各组选项共用起点；两档、三档、四档都在同一列内均分。
    let ctl_w = 218.0_f32.min(rect.width() * 0.65);
    let ctl = egui::Rect::from_min_max(
        egui::pos2(rect.right() - ctl_w, rect.top() + 3.0),
        egui::pos2(rect.right(), rect.bottom() - 3.0),
    );
    let label_rect = egui::Rect::from_min_max(
        rect.min + egui::vec2(2.0, 0.0),
        egui::pos2(ctl.left() - 6.0, rect.bottom()),
    );
    ellipsis_text(painter, label_rect, label, 12.0, p.text, Align::Min);
    ui.interact(
        label_rect,
        ui.id().with(("setting-label", label)),
        egui::Sense::hover(),
    )
    .on_hover_text(label);
    painter.rect_filled(ctl, CornerRadius::same(14), p.chip);

    let item_w = ctl.width() / options.len() as f32;

    // 选中态滑块：位置缓动过去，而不是瞬间跳一格。
    // 用 `animate_value_with_time` 而不是自己记状态，egui 会负责持续重绘。
    let selected_index = options.iter().position(|(v, _)| v == current);
    if let Some(idx) = selected_index {
        let pos =
            ui.ctx()
                .animate_value_with_time(ui.id().with(("opt-pill", label)), idx as f32, 0.16);
        let pill = egui::Rect::from_min_size(
            egui::pos2(ctl.left() + pos * item_w, ctl.top()),
            Vec2::new(item_w, ctl.height()),
        );
        surface::pill(ui.painter(), pill.shrink(2.0), p);
    }

    let mut changed = false;
    for (i, (value, text)) in options.iter().enumerate() {
        let r = egui::Rect::from_min_size(
            egui::pos2(ctl.left() + i as f32 * item_w, ctl.top()),
            Vec2::new(item_w, ctl.height()),
        );
        let resp = ui.interact(r, ui.id().with(("opt", label, i)), egui::Sense::click());
        let selected = *current == *value;
        resp.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::RadioButton,
                ui.is_enabled(),
                selected,
                format!("{label}: {text}"),
            )
        });
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            if !selected {
                ui.painter().rect_filled(
                    r.shrink(2.0),
                    CornerRadius::same(12),
                    p.accent.gamma_multiply(0.10),
                );
            }
        }

        ellipsis_text(
            ui.painter(),
            r.shrink2(egui::vec2(3.0, 0.0)),
            text,
            10.5,
            if selected { p.on_selection } else { p.subtext },
            Align::Center,
        );

        surface::focus(ui.painter(), r, &resp, p);
        if resp.clicked() && !selected {
            *current = *value;
            changed = true;
        }
    }
    changed
}

/// 设置项：左边标签，右边一个开关。返回是否被点击。
///
/// 行矩形由调用方给定，不走流式布局：设置页底部那行要贴着面板底边定位
/// （见 `settings_body` 的注释），位置不能再让游标决定。
fn toggle_row_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    p: &Palette,
    label: &str,
    on: &mut bool,
) -> bool {
    // 整行都能点，比只点那个小圆点好按
    let resp = ui.interact(rect, ui.id().with(("toggle", label)), egui::Sense::click());

    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left() + 2.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        p.text,
    );

    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 19.0, rect.center().y),
        Vec2::new(36.0, 21.0),
    );
    // 0..1 的缓动值：轨道颜色和旋钮位置一起插值，开关才有「按下去」的手感
    let t = ui
        .ctx()
        .animate_bool_with_time(ui.id().with(("toggle-anim", label)), *on, 0.15);
    painter.rect_filled(
        track,
        CornerRadius::same(11),
        mix(p.switch_off, p.selection, t),
    );
    let knob_x = track.left() + 10.5 + (track.width() - 21.0) * t.clamp(0.0, 1.0);
    painter.circle_filled(egui::pos2(knob_x, track.center().y), 8.0, Color32::WHITE);
    surface::focus(painter, rect, &resp, p);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if resp.clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// 设置入口图标：三条带滑块的横线。手画而不是用 ⚙ 字符，
/// 是因为 Arial Unicode 里未必有这个字形，缺字会变成方块。
///
/// 点击区比图形大一圈（16pt 的画，28×28pt 的判定）：100% 缩放下图标只有
/// 16 个像素，面板里又只有这一个入口，点空一个像素就等于「设置点不动」。
fn sliders_button(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let (hit, _) = ui.allocate_exact_size(Vec2::splat(30.0), egui::Sense::hover());
    let rect = hit.shrink(6.0);
    let resp = ui.interact(hit, ui.id().with("settings-entry"), egui::Sense::click());
    let hovered = resp.hovered();
    let color = if p.dark { p.subtext } else { p.accent };

    let painter = ui.painter();
    painter.rect_filled(hit, 11.0, p.chip);
    if hovered {
        // 悬停给一块底，先让人看出「这是能点的」，再谈点不点得中
        painter.rect_filled(hit, CornerRadius::same(11), p.accent.gamma_multiply(0.12));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    for i in 0..3 {
        let y = rect.top() + 3.0 + i as f32 * 5.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.4, color),
        );
        // 滑块位置左右错开，看着像个均衡器而不是"汉堡菜单"
        let kx = rect.left() + 4.0 + ((i + 1) % 3) as f32 * 3.5;
        painter.circle_filled(egui::pos2(kx, y), 2.1, p.chip);
        painter.circle_stroke(egui::pos2(kx, y), 2.1, Stroke::new(1.2, color));
    }
    surface::focus(painter, hit, &resp, p);
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

/// 当前 Unix 秒。只用来和 `settings.last_check` 比间隔，不需要日历精度。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// 图标按钮：手画的形状 + 固定大小的点击区。页脚（导出/退出）和设置页的
/// 返回共用。
///
/// 为什么不用 `egui::Button`：按钮尺寸 = 字形尺寸 + padding，中文和英文不一样，
/// 换语言时会跟着变一点。页脚贴着面板底边，长高就会把内容顶出去；标题栏里的
/// 返回如果带文字，也会把「设置」两字挤得不居中。这里只画字形，尺寸从一开始就定死。
fn icon_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &str,
    p: &Palette,
    paint: fn(&egui::Painter, egui::Rect, Color32),
) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(id), egui::Sense::click());
    let color = if p.dark { p.subtext } else { p.accent };
    ui.painter().rect_filled(rect, 10.0, p.chip);
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), p.accent.gamma_multiply(0.12));
    }
    paint(ui.painter(), rect, color);
    surface::focus(ui.painter(), rect, &resp, p);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn action_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    id: &str,
    label: &str,
    p: &Palette,
    paint: fn(&egui::Painter, egui::Rect, Color32),
) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(id), egui::Sense::click());
    let color = if p.dark { p.subtext } else { p.accent };
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), p.accent.gamma_multiply(0.10));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        Vec2::splat(24.0),
    );
    paint(ui.painter(), icon, color);
    ui.painter().text(
        egui::pos2(rect.left() + 29.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.5),
        color,
    );
    surface::focus(ui.painter(), rect, &resp, p);
    resp
}

/// 导出：一个开口朝上的托盘，中间是往上走的箭头。
///
/// 不用「下载」那个向下箭头：导出是把数据**交出去**，箭头朝上更贴近语义，
/// 也和老版本「导出」两个字的位置对得上。
fn paint_export_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.35, color);
    let half = 5.2;
    let top = c.y + 0.9;
    let bottom = c.y + 5.7;
    // 托盘：三面开口的 U（底角不圆，1.3pt 的线在 14pt 里也看不出直角）
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(c.x - half, top),
            egui::pos2(c.x - half, bottom),
            egui::pos2(c.x + half, bottom),
            egui::pos2(c.x + half, top),
        ],
        stroke,
    ));
    // 箭头：竖杆 + 两撇，杆要从托盘里穿出去
    painter.line_segment(
        [egui::pos2(c.x, c.y + 3.6), egui::pos2(c.x, c.y - 5.9)],
        stroke,
    );
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(c.x - 3.1, c.y - 2.9),
            egui::pos2(c.x, c.y - 6.0),
            egui::pos2(c.x + 3.1, c.y - 2.9),
        ],
        stroke,
    ));
}

/// 退出：电源符号 —— 一圈留了缺口的圆 + 顶上那根竖线。
fn paint_quit_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.35, color);
    let r = 5.4;
    let cy = c.y + 0.7;
    // 缺口留在正上方：从 310° 顺着扫 280°，正好绕开 270° 那一小段
    let mut pts = Vec::with_capacity(25);
    for i in 0..=24 {
        let a = (310.0 + 280.0 * i as f32 / 24.0).to_radians();
        pts.push(egui::pos2(c.x + r * a.cos(), cy + r * a.sin()));
    }
    painter.add(egui::Shape::line(pts, stroke));
    painter.line_segment(
        [egui::pos2(c.x, c.y - 6.5), egui::pos2(c.x, c.y - 0.5)],
        stroke,
    );
}

/// 返回：一个左尖括号。
///
/// 不用「‹」字符：不同字体的字形宽度和基线都不一样，标题栏里会显得忽高忽低。
fn paint_back_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.6, color);
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(c.x + 3.4, c.y - 5.0),
            egui::pos2(c.x - 3.2, c.y),
            egui::pos2(c.x + 3.4, c.y + 5.0),
        ],
        stroke,
    ));
}

/// 不按帧吸附或覆盖偏移：macOS 触控板的小幅事件之间也会有空闲重绘帧。
/// 把这种空帧当成手势结束，会不断抹掉不足半行的位移，让列表看起来滚不动。
fn app_list_scroll_area(height: f32, initial_offset: Option<f32>) -> egui::ScrollArea {
    let area = egui::ScrollArea::vertical()
        .id_salt("app-list")
        .max_height(height)
        .auto_shrink([false, false])
        // 右侧过道中的自绘指示条不覆盖行盒描边。
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
    match initial_offset {
        Some(offset) => area.vertical_scroll_offset(offset),
        None => area,
    }
}

fn settings_scroll_area(height: f32, initial_offset: Option<f32>) -> egui::ScrollArea {
    let area = egui::ScrollArea::vertical()
        .id_salt("settings-options")
        .max_height(height)
        .auto_shrink([false, false]);
    match initial_offset {
        Some(offset) => area.vertical_scroll_offset(offset),
        None => area,
    }
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

/// 拿不到真实图标时的字母块。
fn badge(painter: &egui::Painter, rect: egui::Rect, name: &str, dark: bool) {
    let c = icons::tile_color(name);
    surface::gradient(
        painter,
        rect,
        6.0,
        mix(c, Color32::WHITE, 0.12),
        mix(c, Color32::BLACK, if dark { 0.25 } else { 0.10 }),
    );
    let initial = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        egui::FontId::proportional(16.0),
        Color32::WHITE,
    );
}

/// 系统服务的图标：中性灰底 + 齿轮。
///
/// 不走 [`badge`] 的彩色：那套颜色的语义是「某个应用」，系统服务恰恰要看起来
/// 不属于任何应用。用中性灰底加一只
/// 齿轮，扫一眼就知道这行是 `mDNSResponder` 那种后台东西，不是装的应用。
fn service_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    gear: &egui::TextureHandle,
    p: &Palette,
    dark: bool,
) {
    let bg = mix(p.unit, p.surface, if dark { 0.50 } else { 0.30 });
    painter.rect_filled(rect, CornerRadius::same(6), bg);
    painter.image(
        gear.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::from_rgb(241, 245, 255),
    );
}

/// 一格速率：数值贴右，单位小一号、淡一档；极端大数约束在本列内。
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
        painter.layout_no_wrap(unit_text, egui::FontId::proportional(9.5), unit_color);
    let mut val_galley =
        painter.layout_no_wrap(val.to_owned(), theme::display_font(val_size), val_color);
    let max_value_width = NUM_W_TX - NUM_UNIT_W - 3.0;
    if val_galley.size().x > max_value_width {
        val_galley = painter.layout_no_wrap(
            val.to_owned(),
            theme::display_font(val_size * max_value_width / val_galley.size().x),
            val_color,
        );
    }
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
#[allow(clippy::too_many_arguments)]
fn metric(
    ui: &mut egui::Ui,
    w: f32,
    h: f32,
    arrow: &str,
    label: &str,
    value: Option<String>,
    trend: &[f64],
    color: Color32,
    p: &Palette,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), egui::Sense::hover());
    let painter = ui.painter();
    let bottom = mix(p.surface, color, if p.dark { 0.12 } else { 0.06 });
    surface::gradient(painter, rect, 12.0, p.surface, bottom);
    surface::sparkline(painter, rect.shrink(1.0), trend, color, p.dark);
    painter.rect_stroke(
        rect,
        12.0,
        Stroke::new(0.8, mix(p.border, color, if p.dark { 0.20 } else { 0.08 })),
        egui::StrokeKind::Inside,
    );

    let down = arrow == "↓";
    let circle = egui::pos2(rect.left() + 26.0, rect.top() + 47.0);
    let disc = egui::Rect::from_center_size(circle, Vec2::splat(28.0));
    let disc_top = mix(p.surface, color, if p.dark { 0.82 } else { 0.20 });
    let disc_bottom = mix(p.surface, color, if p.dark { 0.53 } else { 0.40 });
    surface::gradient(painter, disc, 14.0, disc_top, disc_bottom);
    painter.circle_stroke(
        circle,
        14.0,
        Stroke::new(0.6, mix(disc_top, disc_bottom, 0.5)),
    );
    surface::direction(
        painter,
        circle,
        down,
        5.5,
        if p.dark { Color32::WHITE } else { color },
    );

    let text_left = rect.left() + 56.0;
    surface::direction(
        painter,
        egui::pos2(text_left + 3.0, rect.top() + 22.0),
        down,
        3.8,
        color,
    );
    painter.text(
        egui::pos2(text_left + 12.0, rect.top() + 22.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        p.subtext,
    );
    match value {
        Some(v) => {
            let (number, unit) = v.split_once(' ').unwrap_or((&v, "B/s"));
            let value_color = if p.dark {
                p.text
            } else {
                mix(p.text, color, 0.18)
            };
            let unit = painter.layout_no_wrap(
                unit.to_owned(),
                egui::FontId::proportional(11.0),
                p.subtext,
            );
            let mut font_size = 28.0;
            let mut number = painter.layout_no_wrap(
                number.to_owned(),
                theme::display_font(font_size),
                value_color,
            );
            let available = (rect.right() - 10.0 - text_left - unit.size().x - 5.0).max(1.0);
            if number.size().x > available {
                font_size *= available / number.size().x;
                number = painter.layout_no_wrap(
                    number.text().to_owned(),
                    theme::display_font(font_size),
                    value_color,
                );
            }
            let bottom = rect.top() + 64.0;
            let unit_left = text_left + number.size().x + 5.0;
            painter.galley(
                egui::pos2(text_left, bottom - number.size().y),
                number,
                value_color,
            );
            painter.galley(
                egui::pos2(unit_left, bottom - unit.size().y - 3.0),
                unit,
                p.subtext,
            );
        }
        None => {
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(text_left, rect.top() + 43.0),
                    Vec2::new(w - 70.0, 16.0),
                ),
                5.0,
                p.subtext.gamma_multiply(0.25),
            );
        }
    }
    response
}

/// 分段控件：实时 / 1分钟 / 5分钟 / 1小时。画在给定矩形里，
/// 因为它现在跟「应用」标题共处一行，宽度由调用方决定。
fn segmented_at(ui: &mut egui::Ui, bar: egui::Rect, current: &mut Window, p: &Palette, lang: Lang) {
    ui.painter()
        .rect_filled(bar, CornerRadius::same(14), p.chip);

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
    surface::pill(ui.painter(), pill.shrink(2.0), p);

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
            egui::FontId::proportional(12.0),
            if selected { p.on_selection } else { p.subtext },
        );

        surface::focus(ui.painter(), r, &resp, p);
        if resp.clicked() && !selected {
            *current = *w;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
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
        Ok(item)
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
            0.0,
            0.4,
            1.0,
            56.0,
            999.0,
            999.4,
            1000.0,
            1023.9,
            1024.0,
            1024.0 * 9.94,
            1024.0 * 9.96,
            1024.0 * 10.0,
            1024.0 * 999.0,
            1024.0 * 1024.0 * 1.01,
            1024.0 * 1024.0 * 9.99,
            1024.0 * 1024.0 * 10.0,
            1024.0 * 1024.0 * 999.0,
            1024.0 * 1024.0 * 1024.0 * 9.9,
            1024.0 * 1024.0 * 1024.0 * 999.0,
            1e15,
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

    fn scroll_frame(
        ctx: &egui::Context,
        frame: usize,
        rows: usize,
        wheel: Option<(egui::MouseWheelUnit, f32)>,
    ) -> (f32, f32) {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(320.0, 300.0),
            )),
            time: Some(frame as f64 / 60.0),
            ..Default::default()
        };
        input
            .events
            .push(egui::Event::PointerMoved(egui::pos2(50.0, 50.0)));
        if let Some((unit, delta)) = wheel {
            input.events.push(egui::Event::MouseWheel {
                unit,
                delta: egui::vec2(0.0, delta),
                modifiers: egui::Modifiers::NONE,
            });
        }
        let mut result = (0.0, 0.0);
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let out = app_list_scroll_area(156.0, None).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in 0..rows {
                        ui.allocate_exact_size(Vec2::new(260.0, ROW_BOX_H), egui::Sense::hover());
                        if i + 1 < rows {
                            ui.add_space(ROW_GAP);
                        }
                    }
                });
                result = (
                    out.state.offset.y,
                    (out.content_size.y - out.inner_rect.height()).max(0.0),
                );
            });
        });
        result
    }

    #[test]
    fn trackpad_scroll_survives_frames_between_small_events() {
        let ctx = egui::Context::default();
        for frame in 0..180 {
            let delta = match frame {
                2..62 if frame % 3 == 2 => Some(-3.0),
                92..152 if frame % 3 == 2 => Some(3.0),
                _ => None,
            };
            let (offset, _) = scroll_frame(
                &ctx,
                frame,
                10,
                delta.map(|d| (egui::MouseWheelUnit::Point, d)),
            );
            if (62..92).contains(&frame) {
                assert!(
                    (offset - 60.0).abs() < 0.1,
                    "small trackpad deltas were lost: {offset}"
                );
            }
            if frame >= 152 {
                assert!(
                    offset < 0.1,
                    "reverse scroll did not return to the top: {offset}"
                );
            }
        }
    }

    #[test]
    fn mouse_wheel_reaches_bottom_and_shorter_list_clamps_offset() {
        let ctx = egui::Context::default();
        for frame in 0..90 {
            let wheel = matches!(frame, 2 | 6 | 10).then_some((egui::MouseWheelUnit::Line, -10.0));
            let rows = if frame < 61 { 20 } else { 3 };
            let (offset, max) = scroll_frame(&ctx, frame, rows, wheel);
            if frame == 60 {
                assert!(max > 400.0 && (offset - max).abs() < 0.1);
            }
            if frame >= 62 {
                assert_eq!((offset, max), (0.0, 0.0));
            }
        }
    }

    #[test]
    fn redesigned_selectors_accept_pointer_and_keyboard_in_both_themes() {
        for dark in [true, false] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let palette = theme::palette(dark);
            let mut appearance = ThemeMode::System;
            let mut window = Window::Live;
            let mut click_at = egui::Pos2::ZERO;
            let mut keyboard_id = egui::Id::NULL;
            for frame in 0..5 {
                let mut input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(WINDOW_W, PANEL_H),
                    )),
                    time: Some(frame as f64 / 60.0),
                    ..Default::default()
                };
                if frame == 1 || frame == 2 {
                    input.events.push(egui::Event::PointerMoved(click_at));
                    input.events.push(egui::Event::PointerButton {
                        pos: click_at,
                        button: egui::PointerButton::Primary,
                        pressed: frame == 1,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                if frame == 3 {
                    ctx.memory_mut(|m| m.request_focus(keyboard_id));
                    input.events.push(egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                        .show(ctx, |ui| {
                            let top = ui.cursor().top();
                            click_at = egui::pos2(ui.max_rect().right() - 20.0, top + 17.0);
                            option_row(
                                ui,
                                &palette,
                                "Appearance",
                                &mut appearance,
                                &[
                                    (ThemeMode::System, "System"),
                                    (ThemeMode::Light, "Light"),
                                    (ThemeMode::Dark, "Dark"),
                                ],
                            );
                            let (bar, _) = ui
                                .allocate_exact_size(Vec2::new(SEG_W, SEG_H), egui::Sense::hover());
                            keyboard_id = ui.id().with(("win", 3_usize));
                            segmented_at(ui, bar, &mut window, &palette, Lang::En);
                        });
                });
                if frame >= 2 {
                    assert_eq!(appearance, ThemeMode::Dark);
                }
                if frame >= 3 {
                    assert_eq!(window, Window::Hour1);
                }
            }
        }
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
            fit_name("一二三四五六七八九十一二", NAME_UNITS)
                .chars()
                .count(),
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
