//! 托盘弹窗主体：状态栏图标 + 无边框弹出面板。

mod update_ui;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};
use tm_core::{format_bytes, format_speed, pretty_name, AppKey, AppRate};
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
use crate::surface;
use crate::theme::{self, Lang, Palette};

pub const WINDOW_W: f32 = 408.0;
/// 面板高度，**固定值**。
///
/// 以前高度是逐帧量内容算出来的，结果：切到设置页会缩一截、应用数一变窗口就抽动。
/// 一个菜单栏弹窗本来就不该自己改尺寸 —— 固定之后，列表装不下就滚动，
/// 设置页按可用高度滚动，切页/换数据时窗口一动不动。
///
/// 两页使用相同尺寸，列表填满剩余高度并连续滚动，设置页固定标题和页签。
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
/// 行末图钉的独立点击区域，与速率列之间留一点空隙。
const PIN_SIZE: f32 = 26.0;
const PIN_GAP: f32 = 4.0;
#[cfg(windows)]
const NETWORK_BUTTON_W: f32 = PIN_SIZE;
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
const SETTINGS_TAB_ANIM: Duration = Duration::from_millis(180);

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
enum SettingsTab {
    #[default]
    StatusBar,
    Statistics,
    General,
}

impl SettingsTab {
    fn index(self) -> usize {
        match self {
            Self::StatusBar => 0,
            Self::Statistics => 1,
            Self::General => 2,
        }
    }
}

impl Window {
    pub fn all() -> [Window; 4] {
        [Window::Live, Window::Min1, Window::Min5, Window::Hour1]
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Window::Live => lang.t("window.live"),
            Window::Min1 => lang.t("window.minute"),
            Window::Min5 => lang.t("window.five_minutes"),
            Window::Hour1 => lang.t("window.hour"),
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
    #[cfg(not(target_os = "macos"))]
    Click { fallback: Option<IconRect> },
    #[cfg(target_os = "macos")]
    MenuClick {
        item: MenuItem,
        anchor: Option<tm_macos::ClickAnchor>,
    },
    /// 用户点了任务栏上的网速或性能贴片（Windows 专属）。
    ///
    /// 和托盘点击分开记，是因为**锚点不一样**：贴片在任务栏条里，和托盘
    /// 图标之间可能隔着一整个第三方任务栏插件。按托盘
    /// 图标摆的话，点最左边的数字、面板却从右边托盘上方冒出来，看着像点
    /// 错了东西。这里直接以被点的贴片为锚点——点哪儿弹哪儿。
    #[cfg(windows)]
    BadgeClick { at: IconRect },
    #[cfg(windows)]
    BadgeReordered {
        order: tm_core::taskbar::TaskbarOrder,
    },
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuItem {
    Icon,
    Speed,
    Performance,
}

// tray-icon 只允许注册一个全局回调，所以用全局 channel 把事件转回 App。
static TRAY_TX: OnceLock<Sender<TrayEvent>> = OnceLock::new();
static CTX: OnceLock<egui::Context> = OnceLock::new();

pub struct App {
    rx: engine::Engine,
    device_ip: crate::device_ip::Monitor,
    tray_rx: Receiver<TrayEvent>,
    latest: Option<Snapshot>,
    visible: bool,
    shown_at: Instant,
    auto_hidden_at: Option<Instant>,
    tray: Option<TrayHandle>,
    #[cfg(target_os = "macos")]
    speed_tray: Option<tm_macos::StatusItem>,
    #[cfg(target_os = "macos")]
    speed_title: String,
    #[cfg(target_os = "macos")]
    speed_tip: String,
    #[cfg(target_os = "macos")]
    speed_tray_retry: Instant,
    #[cfg(any(target_os = "macos", windows))]
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
    menu_anchor: MenuItem,
    #[cfg(target_os = "macos")]
    menu_work_area: Option<ScreenBox>,
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
    #[cfg(windows)]
    network_access: crate::network_access::NetworkAccess,
    #[cfg(windows)]
    network_panel: crate::network_panel::NetworkPanel,
    /// 采样线程读取的保留天数，改设置后即时生效
    retention: Arc<AtomicI64>,
    /// 当前是主面板还是设置页
    view: View,
    settings_tab: SettingsTab,
    settings_transition: Option<(SettingsTab, Instant)>,
    /// 切页动画：`(从哪一页切过来的, 开始时刻)`，播完就清空。
    ///
    /// 只有 `view` 停不住的地方才需要它：切页那一帧两页都得画 ——
    /// 旧页往左淡出、新页从右侧淡入，见 [`App::draw_page`]。
    transition: Option<(View, Instant)>,
    /// 底部临时提示（导出结果、自启切换结果），`(文案, 起始时间)`
    toast: Option<(String, Instant)>,
    /// 同一时刻只运行一个后台导出，避免重复点击覆盖同一文件。
    export_job: Option<crate::export::ExportJob<Result<ExportedHistory, String>>>,
    diagnostics_job: Option<crate::export::ExportJob<Result<std::path::PathBuf, String>>>,
    diagnostics_result: Option<Result<std::path::PathBuf, String>>,
    error_prompt: Option<crate::error_dialog::Prompt>,
    update_prompt: Option<update_ui::UpdatePrompt>,
    /// 更新检测。网络动作全在后台线程，这里只是把事件攒成可画的状态。
    update: crate::update::Updater,
    #[cfg(windows)]
    sensor_components: crate::sensor_components::Manager,
    /// 进程启动时刻。
    ///
    /// 自动检查要等启动 10 秒后再发请求：首帧那会儿还在建窗口、连采样线程，
    /// 再塞一个 DNS 解析进去，开机自启的机器上第一次点面板会明显迟滞。
    started_at: Instant,
    /// `--check-update`：跳过「启动延迟 + 24 小时」两个门槛，立刻查一次。
    force_update_check: bool,
    /// 一次性列表滚动位置：调试初始位置，或置顶后回到顶部。
    /// 消费后继续由 ScrollArea 保留连续滚动状态。
    next_list_scroll: Option<f32>,
    /// 调试用设置页初始滚动位置。
    initial_settings_scroll: Option<f32>,
    /// 首次引导或显式预览请求，成功展开后消费。
    show_on_start: bool,
    /// 首次引导等待菜单栏完成布局，预览模式仍可使用固定位置。
    wait_for_tray_on_start: bool,
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
    /// Windows 隐藏图标后仍保留托盘句柄；不能再用它的旧位置定位面板。
    #[cfg(windows)]
    tray_visible: bool,
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
    pub fn new(cc: &eframe::CreationContext<'_>, launch: crate::startup::Launch) -> Self {
        #[cfg(windows)]
        {
            crate::windows_panel::prepare(cc);
            if let Some(gl) = &cc.gl {
                use eframe::glow::{HasContext, RENDERER, VENDOR, VERSION};
                // Read-only context metadata makes driver-specific UI reports actionable.
                unsafe {
                    log::info!(target: "tramoing::window", "OpenGL vendor={} renderer={} version={}",
                        gl.get_parameter_string(VENDOR), gl.get_parameter_string(RENDERER), gl.get_parameter_string(VERSION));
                }
            }
        }
        let ctx = cc.egui_ctx.clone();
        let _ = CTX.set(ctx.clone());
        tm_diagnostics::set_error_waker({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });

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

        if let Some(path) = theme::install_cjk(&ctx) {
            log::info!(target: "tramoing::ui", "已加载补充字体：{path}");
        } else {
            log::warn!(target: "tramoing::ui", "未找到补充字体，使用默认字体");
        }
        let lang = crate::i18n::resolve(settings.lang);

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
        #[cfg(any(target_os = "macos", windows))]
        let performance = {
            let wake = ctx.clone();
            let mut monitor = crate::performance::Monitor::new(move || wake.request_repaint());
            monitor.configure(
                settings.performance_enabled,
                settings.performance_interval,
                settings.performance_temperature,
            );
            monitor
        };

        let app = Self {
            rx,
            device_ip: crate::device_ip::Monitor::new({
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            }),
            tray_rx,
            latest: None,
            visible: false,
            shown_at: Instant::now(),
            auto_hidden_at: None,
            tray: None,
            #[cfg(target_os = "macos")]
            speed_tray: None,
            #[cfg(target_os = "macos")]
            speed_title: String::new(),
            #[cfg(target_os = "macos")]
            speed_tip: String::new(),
            #[cfg(target_os = "macos")]
            speed_tray_retry: Instant::now(),
            #[cfg(any(target_os = "macos", windows))]
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
            menu_anchor: MenuItem::Icon,
            #[cfg(target_os = "macos")]
            menu_work_area: None,
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
            view: if std::env::var("TM_VIEW")
                .map(|v| v == "settings")
                .unwrap_or(false)
            {
                View::Settings
            } else {
                View::Main
            },
            settings_tab: SettingsTab::default(),
            settings_transition: None,
            // 首帧不播过渡：从托盘点开就直接是目标页
            transition: None,
            settings,
            #[cfg(windows)]
            network_access: crate::network_access::NetworkAccess::new(
                std::env::var_os("TM_FORCE_MOCK").is_some(),
            ),
            #[cfg(windows)]
            network_panel: crate::network_panel::NetworkPanel::default(),
            retention,
            // 调试用：TM_TOAST="导出 12 行" 让页脚一上来就带着提示，方便核对排版
            toast: std::env::var("TM_TOAST")
                .ok()
                .map(|msg| (msg, Instant::now())),
            export_job: None,
            diagnostics_job: None,
            diagnostics_result: None,
            error_prompt: None,
            update_prompt: None,
            update: {
                let wake = ctx.clone();
                let mut updater =
                    crate::update::Updater::with_waker(move || wake.request_repaint());
                if std::env::var_os("TM_FORCE_MOCK").is_none() {
                    updater.restore_download();
                }
                updater
            },
            #[cfg(windows)]
            sensor_components: crate::sensor_components::Manager::new(),
            started_at: Instant::now(),
            force_update_check: std::env::var_os("TM_FORCE_UPDATE").is_some(),
            // 调试用：TM_SCROLL=140 给列表一个初始位置，方便截图核对滚动后的样子
            next_list_scroll: std::env::var("TM_SCROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v: &f32| v.is_finite()),
            initial_settings_scroll: std::env::var("TM_SETTINGS_SCROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v: &f32| v.is_finite()),
            show_on_start: launch.show,
            wait_for_tray_on_start: launch.wait_for_tray,
            keep_open_on_blur: launch.keep_open,
            // macOS 靠「前几帧再隐藏」抵消 eframe 的强制显示；Windows 改成摆到屏幕外
            #[cfg(not(windows))]
            startup_hide: 3,
            frame_no: 0,
            #[cfg(windows)]
            startup_parked: false,
            opened_at: None,
            had_focus: false,
            #[cfg(windows)]
            tray_visible: true,
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
            #[cfg(debug_assertions)]
            crate::ui_capture::prepare_network_panel(
                &mut app.network_panel,
                &mut app.network_access,
                lang,
            );
            app
        };

        #[cfg(debug_assertions)]
        let app = {
            let mut app = app;
            if crate::ui_capture::enabled() {
                match std::env::var("TM_CAPTURE_STATE").as_deref() {
                    Ok(
                        "settings-general"
                        | "settings-diagnostics-saved"
                        | "settings-diagnostics-error"
                        | "settings-diagnostics-busy",
                    ) => {
                        app.view = View::Settings;
                        app.settings_tab = SettingsTab::General;
                        app.diagnostics_result = crate::ui_capture::diagnostics_result(lang);
                    }
                    Ok("settings-statusbar" | "settings-color-download" | "settings-color-gpu") => {
                        app.view = View::Settings;
                        app.settings_tab = SettingsTab::StatusBar;
                    }
                    Ok("settings-statistics") => {
                        app.view = View::Settings;
                        app.settings_tab = SettingsTab::Statistics;
                    }
                    Ok("error-prompt") => {
                        log::error!(target: "tramoing::preview", "采集连接暂时中断，正在尝试重新连接。");
                    }
                    Ok("update-ready") => {
                        let version = "1.2.3".to_owned();
                        app.update.phase = crate::update::Phase::Ready {
                            version: version.clone(),
                            path: tm_store::data_dir().join("preview-only"),
                            launch_error: None,
                        };
                        app.update_prompt = Some(update_ui::UpdatePrompt::restart(version));
                    }
                    Ok("update-download-failed") => {
                        app.update_prompt = Some(update_ui::UpdatePrompt::download_failed(
                            "HTTP 503 · Service Unavailable".into(),
                        ));
                    }
                    _ => {}
                }
            }
            app
        };

        app
    }

    /// 到点就查一次更新。返回 true 表示这一帧刚发起请求。
    ///
    /// 判断放在每帧跑，而不是起一个定时线程：省一个线程，也让「关掉自动检查」
    /// 这种改动立刻生效，不用等定时器下一轮。
    fn maybe_check_update(&mut self) -> bool {
        if !self.update.can_check() {
            return false;
        }
        // `--mock` 是离线演示模式，不该悄悄往外发请求；
        // 显式给了 --check-update 才放行，方便在假数据下核对进度条排版。
        if std::env::var_os("TM_FORCE_MOCK").is_some() && !self.force_update_check {
            return false;
        }
        let due = if self.force_update_check {
            true
        } else if !self.settings.update_check
            || !self.update.can_auto_check()
            || crate::update::manifest_url().is_none()
        {
            false
        } else {
            let now = unix_now();
            self.started_at.elapsed() >= UPDATE_FIRST_DELAY
                && (self.settings.last_check <= 0
                    || now < self.settings.last_check
                    || now.saturating_sub(self.settings.last_check) >= UPDATE_INTERVAL_SECS)
        };
        if !due {
            return false;
        }
        self.force_update_check = false;
        self.check_update();
        true
    }

    /// 手动与自动检查共享时间戳，手动检查后不再紧接着重复发起每日检查。
    fn check_update(&mut self) {
        if !self.update.can_check() {
            return;
        }
        // 先落盘再发请求：查的过程中进程被关掉，下次启动不该立刻再来一次。
        self.settings.last_check = unix_now();
        self.settings.last_check_result = crate::settings::UpdateCheckResult::Pending;
        self.settings.save();
        self.update.check();
        self.record_update_check_result();
    }

    /// 延迟创建状态栏图标，失败则过几秒重试。
    fn ensure_tray(&mut self) {
        if self.tray.is_some() || Instant::now() < self.tray_build_at {
            return;
        }
        match build_tray() {
            Ok(t) => self.tray = Some(t),
            Err(e) => {
                log::error!(target: "tramoing::tray", "状态栏图标创建失败：{e}");
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
        // Any successful opening consumes the first-launch request, including
        // activation and error prompts that arrive before the welcome frame.
        self.show_on_start = false;
        self.wait_for_tray_on_start = false;
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
        let work = self
            .menu_work_area
            .or_else(|| {
                self.menu_item(self.menu_anchor)
                    .and_then(|tray| tray.work_area())
                    .map(|(x, y, w, h)| ScreenBox {
                        x: x as f32 * ppp,
                        y: y as f32 * ppp,
                        w: w as f32 * ppp,
                        h: h as f32 * ppp,
                    })
            })
            .unwrap_or(screen);
        #[cfg(not(any(windows, target_os = "macos")))]
        let work = screen;

        compute_placement(icon, work, ppp)
    }

    pub fn close(&mut self, ctx: &egui::Context) {
        if self.error_prompt.is_some() || self.has_update_prompt() {
            return;
        }
        #[cfg(windows)]
        if self.sensor_components.is_installing() {
            return;
        }
        if matches!(
            self.update.phase,
            crate::update::Phase::Installing { .. } | crate::update::Phase::Restarting
        ) {
            return;
        }
        #[cfg(windows)]
        self.network_panel.close();
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
            // 所有任务栏/菜单栏入口都从主面板展开，不沿用上次停留的设置页。
            // 同时清掉切页动画，避免展开首帧闪出旧的设置内容。
            self.view = View::Main;
            self.transition = None;
            self.open(ctx, rect);
        }
    }

    fn drain_engine(&mut self) {
        match self.rx.try_recv() {
            Ok(snap) => {
                #[cfg(debug_assertions)]
                let snap = crate::ui_capture::preview_snapshot(snap, self.lang);
                self.latest = Some(snap);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let snapshot = self.latest.get_or_insert_with(Snapshot::default);
                snapshot.total_rx = 0.0;
                snapshot.total_tx = 0.0;
                snapshot.trend.clear();
                snapshot.rates.clear();
                snapshot.windowed = Default::default();
                let message = self.lang.t("ui.sampler_stopped_restart_the_app").to_owned();
                if snapshot.error.as_ref() != Some(&message) {
                    log::error!(target: "tramoing::sampler", "{message}");
                }
                snapshot.error = Some(message);
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn network_tooltip(&self) -> String {
        match &self.latest {
            Some(s) if s.error.is_some() => s.error.clone().unwrap_or_default(),
            Some(s) => format!(
                "↓ {}    ↑ {}",
                format_speed(s.total_rx),
                format_speed(s.total_tx)
            ),
            None => "Tramoing".to_owned(),
        }
    }

    /// 图标只承载图像和悬停提示，网速使用独立的原生状态项。
    fn sync_tray_icon(&mut self) {
        let Some(tray) = &self.tray else { return };
        let tip = self.network_tooltip();
        #[cfg(windows)]
        let tip = if self.settings.performance_enabled {
            let reading = self.performance.reading();
            format!(
                "{tip}\nCPU {}    GPU {}{}",
                crate::performance::menu_reading(
                    reading.as_ref().and_then(|r| r.cpu),
                    reading.as_ref().and_then(|r| r.temperatures.cpu),
                    self.settings.performance_temperature
                ),
                crate::performance::menu_reading(
                    reading.as_ref().and_then(|r| r.gpu),
                    reading.as_ref().and_then(|r| r.temperatures.gpu),
                    self.settings.performance_temperature
                ),
                if std::env::var_os("TM_FORCE_MOCK").is_some() {
                    self.lang.t("tray.demo_data")
                } else {
                    ""
                }
            )
        } else {
            tip
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
            // 统计项创建失败时临时保留图标，保证始终能打开设置。
            let stats_visible = [&self.speed_tray, &self.performance_tray]
                .into_iter()
                .flatten()
                .any(|item| item.is_visible());
            let visible = self.settings.tray_icon || !stats_visible;
            if tray.is_visible() != visible {
                tray.set_visible(visible);
            }
            if std::env::var_os("TM_TRACE").is_some() {
                if let Ok(path) = std::env::var("TM_TRAY_PNG") {
                    eprintln!(
                        "[icon] rect={:?} visible={} {} dump={}",
                        tray.icon_rect(),
                        tray.is_visible(),
                        tray.debug_state(),
                        tray.dump_png(&path)
                    );
                }
            }
        }
    }

    /// 网速是独立的菜单栏项，关闭时保留句柄以便恢复其位置。
    #[cfg(target_os = "macos")]
    fn sync_speed_tray(&mut self) {
        if !self.settings.tray_badge {
            if let Some(tray) = &self.speed_tray {
                if tray.is_visible() {
                    tray.set_visible(false);
                }
            }
            return;
        }
        if self.tray.is_none() {
            return;
        }
        if self.speed_tray.is_none() && Instant::now() >= self.speed_tray_retry {
            self.speed_tray = tm_macos::StatusItem::new_named_with_click(
                "dev.guolongkang.tramoing.speed",
                |anchor| {
                    if let Some(tx) = TRAY_TX.get() {
                        let _ = tx.send(TrayEvent::MenuClick {
                            item: MenuItem::Speed,
                            anchor,
                        });
                    }
                    if let Some(ctx) = CTX.get() {
                        ctx.request_repaint();
                    }
                },
            );
            self.speed_tray_retry = Instant::now() + Duration::from_secs(5);
        }
        let Some(tray) = &self.speed_tray else { return };
        let (rx, tx) = self
            .latest
            .as_ref()
            .map(|s| (s.total_rx, s.total_tx))
            .unwrap_or((0.0, 0.0));
        let (up, down) = (menu_speed(tx), menu_speed(rx));
        let key = format!("{up}|{down}|{}", tray.appearance_is_dark());
        if key != self.speed_title {
            tray.set_speed_lines(&tm_macos::SpeedLines {
                up: &up,
                down: &down,
                colored: self.settings.tray_color,
                colors: self.settings.display_colors,
            });
            self.speed_title = key;
        }
        let tip = self.network_tooltip();
        if tip != self.speed_tip {
            tray.set_tooltip(Some(&tip));
            self.speed_tip = tip;
        }
        if !tray.is_visible() {
            tray.set_visible(true);
        }
        if std::env::var_os("TM_TRACE").is_some() {
            if let Ok(path) = std::env::var("TM_SPEED_PNG") {
                eprintln!(
                    "[speed] rect={:?} {} dump={}",
                    tray.icon_rect(),
                    tray.debug_state(),
                    tray.dump_png(&path)
                );
            }
        }
    }

    /// 独立的双行性能状态项；关闭时隐藏并停止采样，保留原生句柄和位置。
    #[cfg(target_os = "macos")]
    fn sync_performance_tray(&mut self) {
        if !self.settings.performance_enabled {
            if let Some(tray) = &self.performance_tray {
                if tray.is_visible() {
                    tray.set_visible(false);
                }
            }
            return;
        }
        if self.tray.is_none() {
            return;
        }
        if self.performance_tray.is_none() && Instant::now() >= self.performance_tray_retry {
            self.performance_tray = tm_macos::StatusItem::new_named_with_click(
                "dev.guolongkang.tramoing.performance",
                |anchor| {
                    if let Some(tx) = TRAY_TX.get() {
                        let _ = tx.send(TrayEvent::MenuClick {
                            item: MenuItem::Performance,
                            anchor,
                        });
                    }
                    if let Some(ctx) = CTX.get() {
                        ctx.request_repaint();
                    }
                },
            );
            self.performance_tray_retry = Instant::now() + Duration::from_secs(5);
        }
        let Some(tray) = &self.performance_tray else {
            return;
        };
        let reading = self.performance.reading();
        let cpu = reading.as_ref().and_then(|r| r.cpu);
        let gpu = reading.as_ref().and_then(|r| r.gpu);
        let cpu_text = crate::performance::menu_reading(
            cpu,
            reading.as_ref().and_then(|r| r.temperatures.cpu),
            self.settings.performance_temperature,
        );
        let gpu_text = crate::performance::menu_reading(
            gpu,
            reading.as_ref().and_then(|r| r.temperatures.gpu),
            self.settings.performance_temperature,
        );
        let key = format!(
            "{cpu_text}|{gpu_text}|{}|{}",
            self.settings.performance_color,
            tray.appearance_is_dark()
        );
        if key != self.performance_title {
            tray.set_utilization_lines(
                &cpu_text,
                &gpu_text,
                self.settings.performance_color,
                self.settings.display_colors,
            );
            self.performance_title = key;
        }
        let unavailable = self.lang.t("common.unavailable");
        let exact = |v: Option<f64>| {
            v.map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| unavailable.into())
        };
        let mut tooltip = self.lang.format(
            "performance.tooltip",
            &[
                ("cpu", &exact(cpu)),
                ("gpu", &exact(gpu)),
                ("seconds", &self.settings.performance_interval.to_string()),
            ],
        );
        if self.settings.performance_temperature {
            tooltip.push_str(&self.lang.format(
                "performance.temperature_tooltip",
                &[
                    (
                        "cpu",
                        &crate::performance::menu_temperature(
                            reading.as_ref().and_then(|r| r.temperatures.cpu),
                        ),
                    ),
                    (
                        "gpu",
                        &crate::performance::menu_temperature(
                            reading.as_ref().and_then(|r| r.temperatures.gpu),
                        ),
                    ),
                ],
            ));
        }
        if gpu.is_none() && reading.is_some() {
            tooltip.push_str(self.lang.t("ui.gpu_driver_does_not_report_utilization"));
        }
        tooltip.push_str(self.lang.t("ui.click_to_open_panel"));
        if std::env::var_os("TM_FORCE_MOCK").is_some() {
            tooltip.push_str(self.lang.t("tray.demo_data"));
        }
        if tooltip != self.performance_tip {
            tray.set_tooltip(Some(&tooltip));
            self.performance_tip = tooltip;
        }
        if !tray.is_visible() {
            tray.set_visible(true);
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

        // 隔离截图可以展示已开启的设置，但不创建真实任务栏统计项。
        #[cfg(debug_assertions)]
        if crate::ui_capture::enabled() {
            return;
        }

        let state = if self.settings.tray_badge || self.settings.performance_enabled {
            let reading = self.performance.reading();
            Some(BadgeState {
                speed: self
                    .latest
                    .as_ref()
                    .filter(|_| self.settings.tray_badge)
                    .map(|s| (s.total_rx, s.total_tx)),
                performance: self.settings.performance_enabled,
                reading,
                temperatures: self.settings.performance_temperature,
                light: self.taskbar_light,
                colored: self.settings.tray_color,
                performance_colored: self.settings.performance_color,
                colors: self.settings.display_colors,
                order: self.settings.taskbar_order.clone(),
            })
        } else {
            None
        };
        self.badge.show(ctx, state);
        // 任务栏统计区尚未就绪或创建失败时保留托盘入口。
        let badge_available = self.badge.error.is_none()
            && (self.settings.performance_enabled
                || (self.settings.tray_badge && self.latest.is_some()));
        let visible = self.settings.tray_icon || !badge_available;
        if let Some(tray) = &self.tray {
            if self.tray_visible != visible && tray.set_visible(visible).is_ok() {
                self.tray_visible = visible;
            }
        }
    }

    /// 状态栏图标在屏幕上的位置，单位是物理像素（`compute_placement` 的约定）。
    #[cfg(not(target_os = "macos"))]
    fn tray_icon_rect(&self, ctx: &egui::Context) -> Option<IconRect> {
        let tray = self.tray.as_ref()?;
        #[cfg(windows)]
        if !self.tray_visible {
            return None;
        }

        let _ = ctx;
        let r = tray.rect()?;
        Some(IconRect {
            x: r.position.x as f32,
            y: r.position.y as f32,
            w: r.size.width as f32,
            h: r.size.height as f32,
        })
    }

    #[cfg(target_os = "macos")]
    fn menu_item(&self, item: MenuItem) -> Option<&tm_macos::StatusItem> {
        match item {
            MenuItem::Icon => self.tray.as_ref(),
            MenuItem::Speed => self.speed_tray.as_ref(),
            MenuItem::Performance => self.performance_tray.as_ref(),
        }
    }

    #[cfg(target_os = "macos")]
    fn menu_item_rect(&self, ctx: &egui::Context, item: MenuItem) -> Option<IconRect> {
        let (x, y, w, h) = self.menu_item(item)?.icon_rect()?;
        let ppp = ctx.pixels_per_point().max(1.0);
        Some(IconRect {
            x: x as f32 * ppp,
            y: y as f32 * ppp,
            w: w as f32 * ppp,
            h: h as f32 * ppp,
        })
    }

    /// 主动展开时选择实际可见的入口；鼠标点击始终使用被点项自己的位置。
    fn panel_anchor(&mut self, ctx: &egui::Context) -> Option<IconRect> {
        #[cfg(target_os = "macos")]
        {
            self.menu_work_area = None;
            for item in [MenuItem::Icon, MenuItem::Speed, MenuItem::Performance] {
                if let Some(rect) = self.menu_item_rect(ctx, item) {
                    self.menu_anchor = item;
                    return Some(rect);
                }
            }
            None
        }
        #[cfg(not(target_os = "macos"))]
        self.tray_icon_rect(ctx)
    }

    fn reveal_panel_for_prompt(&mut self, ctx: &egui::Context) {
        if !self.visible {
            if let Some(anchor) = self.panel_anchor(ctx) {
                self.open(ctx, anchor);
            } else {
                self.show_on_start = true;
            }
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
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Remove native status items while the UI thread and its windows are
        // still alive, before Engine::drop waits for the final history flush.
        // A later installer timeout must not leave an Explorer ghost icon.
        self.tray.take();
        #[cfg(target_os = "macos")]
        {
            self.speed_tray.take();
            self.performance_tray.take();
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 全透明，圆角之外的区域才不会出现一块底色
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        crate::windows_panel::prepare(_frame);
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
        #[cfg(windows)]
        if let Some(message) = self.network_access.tick(ctx, self.lang) {
            self.toast = Some((message, Instant::now()));
        }
        #[cfg(target_os = "macos")]
        {
            self.sync_speed_tray();
            self.sync_performance_tray();
        }
        self.sync_tray_icon();
        self.icons.poll();
        if let Some(job) = &self.export_job {
            let message = match job.try_result() {
                Ok(result) => Some(export_message(&result, self.lang)),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(self.lang.t("ui.export_interrupted").to_owned())
                }
            };
            if let Some(message) = message {
                self.export_job = None;
                self.toast = Some((message, Instant::now()));
            }
        }

        if let Some(job) = &self.diagnostics_job {
            let result = match job.try_result() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(self.lang.t("ui.diagnostic_export_interrupted").into()))
                }
            };
            if let Some(result) = result {
                if let Err(error) = &result {
                    log::error!(target: "tramoing::diagnostics", "{error}");
                }
                self.diagnostics_job = None;
                self.diagnostics_result = Some(result);
            }
        }
        if self.diagnostics_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        if let Some(activation) = crate::instance::take_activation() {
            self.force_update_check |= activation.check_update;
            self.view = if activation.settings {
                View::Settings
            } else {
                View::Main
            };
            self.transition = None;
            if let Some(rect) = self.panel_anchor(ctx) {
                self.open(ctx, rect);
            } else {
                self.show_on_start = true;
            }
        }

        #[cfg(windows)]
        {
            if self.sensor_components.poll() {
                ctx.request_repaint();
            }
            if self.sensor_components.is_busy() {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
        }

        // ---- 更新检测 ----
        //
        // 网络动作全在后台线程，这里吸收结果；错误交给统一的日志导出提示。
        self.update.tick();
        if self.update.poll() {
            ctx.request_repaint();
        }
        self.record_update_check_result();
        self.service_update_install_action(ctx);
        if self.update.phase == crate::update::Phase::Restarting {
            crate::update::log_line("升级已准备完成，正在保存历史并退出");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.maybe_check_update() {
            ctx.request_repaint();
        }
        if self.update.is_busy() {
            // 下载进度挂在 1Hz 心跳上会像卡住，单独提一档；
            // 只是检查时不必这么勤，8 秒总超时，一帧都不用多画。
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        self.poll_error_prompt(ctx);

        // 首次引导等待真实菜单栏位置；显式预览和长时间无法布局时使用备用位置。
        if self.show_on_start && !self.visible {
            let anchor = self
                .wait_for_tray_on_start
                .then(|| self.panel_anchor(ctx))
                .flatten();
            if anchor.is_some()
                || !self.wait_for_tray_on_start
                || self.started_at.elapsed() >= Duration::from_secs(3)
            {
                let anchor = anchor.unwrap_or_else(|| {
                    let ppp = ctx.pixels_per_point().max(1.0);
                    let w = ctx
                        .input(|i| i.viewport().monitor_size)
                        .map_or(1440.0, |m| m.x);
                    let y = std::env::var("TM_PANEL_Y")
                        .ok()
                        .and_then(|v| v.parse::<f32>().ok())
                        .unwrap_or(0.0);
                    IconRect {
                        x: (w - 90.0) * ppp,
                        y: y * ppp,
                        w: 40.0 * ppp,
                        h: 24.0 * ppp,
                    }
                });
                self.open(ctx, anchor);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
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
                #[cfg(not(target_os = "macos"))]
                TrayEvent::Click { fallback } => {
                    #[cfg(windows)]
                    if !self.tray_visible {
                        continue;
                    }
                    match self.tray_icon_rect(ctx).or(fallback) {
                        Some(rect) => self.toggle(ctx, rect),
                        // 状态项还没被布局（刚创建的头几帧）就拿不到位置，这次点击直接忽略
                        None => eprintln!("[ui] 状态栏项还没有位置，忽略这次点击"),
                    }
                }
                #[cfg(target_os = "macos")]
                TrayEvent::MenuClick { item, anchor } => {
                    if self.menu_item(item).is_none_or(|tray| !tray.is_visible()) {
                        continue;
                    }
                    // macOS can report overlapping proxy-window frames for distinct
                    // status items. Mouse callbacks carry the actual click screen.
                    let ppp = ctx.pixels_per_point().max(1.0);
                    self.menu_work_area = anchor.map(|anchor| {
                        let (x, y, w, h) = anchor.work_area;
                        ScreenBox {
                            x: x as f32 * ppp,
                            y: y as f32 * ppp,
                            w: w as f32 * ppp,
                            h: h as f32 * ppp,
                        }
                    });
                    let at = anchor
                        .map(|anchor| {
                            let (x, y, w, h) = anchor.rect;
                            IconRect {
                                x: x as f32 * ppp,
                                y: y as f32 * ppp,
                                w: w as f32 * ppp,
                                h: h as f32 * ppp,
                            }
                        })
                        .or_else(|| self.menu_item_rect(ctx, item));
                    if let Some(at) = at {
                        self.menu_anchor = item;
                        self.toggle(ctx, at);
                    }
                }
                #[cfg(windows)]
                TrayEvent::BadgeClick { at } => {
                    self.toggle(ctx, at);
                }
                #[cfg(windows)]
                TrayEvent::BadgeReordered { order } => {
                    if self.settings.taskbar_order != order {
                        self.settings.taskbar_order = order;
                        self.settings.save();
                    }
                }
            }
        }
        // 先处理拖动落盘，再推送数值，避免旧顺序在松开鼠标的这一帧闪回。
        #[cfg(windows)]
        self.sync_badge(ctx);

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
                && self.error_prompt.is_none()
                && !self.has_update_prompt()
                && !matches!(self.update.phase, crate::update::Phase::Installing { .. } | crate::update::Phase::Restarting)
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

        #[cfg(windows)]
        if self.visible {
            if let Some(path) =
                self.network_panel
                    .show(ctx, &mut self.network_access, &palette, lang)
            {
                let exists = self.settings.is_pinned(&path);
                if !exists {
                    self.settings.toggle_pin(&path);
                    self.settings.save();
                }
                self.next_list_scroll = Some(0.0);
                self.toast = Some((
                    lang.format(
                        if exists {
                            "apps.already_added"
                        } else {
                            "apps.added"
                        },
                        &[("name", &pretty_name(&path))],
                    ),
                    Instant::now(),
                ));
            }
        }
        if self.has_update_prompt() {
            self.show_update_prompt(ctx, &palette, lang);
        } else {
            self.show_error_prompt(ctx, &palette, lang);
        }
    }

    fn poll_error_prompt(&mut self, ctx: &egui::Context) {
        let errors = tm_diagnostics::take_errors();
        if !errors.is_empty() {
            if let Some(prompt) = &mut self.error_prompt {
                prompt.append(errors);
            } else {
                if self.diagnostics_job.is_none() {
                    self.diagnostics_result = None;
                }
                self.error_prompt = Some(crate::error_dialog::Prompt::new(errors));
            }
        }
        if self.error_prompt.is_some() {
            self.reveal_panel_for_prompt(ctx);
        }
    }

    fn show_error_prompt(&mut self, ctx: &egui::Context, p: &Palette, lang: Lang) {
        use crate::error_dialog::Action;
        let Some(prompt) = &self.error_prompt else {
            return;
        };
        match prompt.show(
            ctx,
            p,
            lang,
            self.diagnostics_job.is_some(),
            self.diagnostics_result.as_ref(),
        ) {
            Action::None => {}
            Action::Export => self.export_diagnostics(ctx),
            Action::Dismiss => self.error_prompt = None,
            Action::Reveal => {
                if let Some(Ok(path)) = &self.diagnostics_result {
                    if let Err(error) = crate::diagnostics::reveal(path) {
                        log::error!(target: "tramoing::diagnostics", "打开报告位置失败：{error}");
                        self.diagnostics_result = Some(Err(error));
                    }
                }
            }
        }
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
    /// 标题、两张速率卡片、应用列表（含时间窗口）、固定页脚。
    ///
    /// 高度是固定的（[`PANEL_H`]），所以列表吃掉「剩下多少」，页脚靠底。
    fn main_body(&mut self, ui: &mut egui::Ui, palette: &Palette, lang: Lang) {
        let p = palette;
        let reveal = self.reveal_secs();
        // egui 的 max_rect 会随子控件扩张；页脚必须使用排版前的可见边界。
        let bounds = ui.max_rect();

        self.header(ui, p, lang);
        ui.add_space(12.0);
        self.totals(ui, p, lang);
        ui.add_space(18.0);

        // 时间窗口紧贴列表：它只决定列表按哪段时间排序，控件离内容越近越好。
        self.list_header(ui, p, lang);
        ui.add_space(10.0);
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
        self.footer(ui, p, lang, bounds);
    }

    /// 设置页按使用场景分组；页签和状态固定，选项区独立滚动。
    fn settings_body(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let bounds = ui.max_rect();
        let mut changed = false;
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::hover());
        let back_rect = egui::Rect::from_min_size(header.min, Vec2::splat(30.0));
        if icon_button(ui, back_rect, "settings-back", p, paint_back_icon)
            .on_hover_text(lang.t("common.back"))
            .clicked()
        {
            self.goto(View::Main);
        }
        let title_rect = ui.painter().text(
            header.left_center() + egui::vec2(40.0, 0.0),
            egui::Align2::LEFT_CENTER,
            lang.t("settings.title"),
            theme::display_font(17.0),
            p.text,
        );
        let status_rect = egui::Rect::from_min_max(
            egui::pos2(title_rect.right() + 16.0, header.top()),
            egui::pos2(header.right() - 2.0, header.bottom()),
        );
        ui.add_space(6.0);
        let previous_tab = self.settings_tab;
        settings_tabs(ui, p, lang, &mut self.settings_tab);
        if previous_tab != self.settings_tab {
            self.settings_transition = Some((previous_tab, Instant::now()));
        }
        ui.add_space(6.0);

        let options_height = (bounds.bottom() - ui.cursor().top()).max(1.0);
        let content_rect = egui::Rect::from_min_size(
            ui.cursor().min,
            Vec2::new(ui.available_width(), options_height),
        );
        let mut opacity = 1.0;
        let mut offset = 0.0;
        if let Some((from, started)) = self.settings_transition {
            let progress = started.elapsed().as_secs_f32() / SETTINGS_TAB_ANIM.as_secs_f32();
            if progress < 1.0 {
                opacity = ease_out_cubic(progress);
                let direction = if self.settings_tab.index() > from.index() {
                    1.0
                } else {
                    -1.0
                };
                offset = (1.0 - opacity) * 18.0 * direction;
                ui.ctx().request_repaint();
            } else {
                self.settings_transition = None;
            }
        }
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("settings-content")
                .max_rect(content_rect.translate(egui::vec2(offset, 0.0))),
        );
        content.set_clip_rect(content_rect.intersect(ui.clip_rect()));
        if opacity < 0.95 {
            content.disable();
        }
        content.multiply_opacity(opacity);
        // 分页各自保留滚动位置，滚动条独占空隙，不覆盖颜色输入与开关。
        let scroll = &mut content.spacing_mut().scroll;
        scroll.floating = false;
        scroll.bar_width = 3.0;
        scroll.bar_inner_margin = 7.0;
        scroll.bar_outer_margin = 0.0;
        scroll.foreground_color = false;
        let visuals = content.visuals_mut();
        visuals.extreme_bg_color = Color32::TRANSPARENT;
        visuals.widgets.inactive.bg_fill = mix(p.border, p.subtext, 0.25);
        visuals.widgets.hovered.bg_fill = mix(p.border, p.accent, 0.45);
        visuals.widgets.active.bg_fill = p.accent;
        settings_scroll_area(options_height, self.initial_settings_scroll.take())
            .id_salt(("settings-options", self.settings_tab))
            .show(&mut content, |ui| {
                ui.set_width(ui.available_width());
                match self.settings_tab {
                    SettingsTab::General => self.general_options(ui, p, lang, &mut changed),
                    SettingsTab::StatusBar => {
                        self.status_bar_options(ui, p, lang, &mut changed);
                    }
                    SettingsTab::Statistics => self.statistics_options(ui, p, lang, &mut changed),
                }
            });

        self.settings_status(ui, p, lang, status_rect);
        if changed {
            self.apply_settings(ui.ctx());
        }
    }

    fn general_options(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, changed: &mut bool) {
        settings_group(ui, p, "appearance", lang.t("ui.appearance"), |ui| {
            *changed |= theme_options(ui, p, lang, &mut self.settings.theme);
            ui.add_space(4.0);
            *changed |= language_row(ui, p, lang, &mut self.settings.lang);
        });
        ui.add_space(8.0);
        settings_group(
            ui,
            p,
            "startup_updates",
            lang.t("ui.startup_updates"),
            |ui| {
                let mut on = self.launch_at_login;
                if toggle_row(ui, p, lang.t("ui.launch_at_login"), &mut on) {
                    match autostart::set_enabled(on) {
                        Ok(()) => {
                            self.launch_at_login = on;
                            self.toast = Some((
                                lang.t("ui.startup_preference_updated").into(),
                                Instant::now(),
                            ));
                        }
                        Err(error) => {
                            log::error!(target: "tramoing::autostart", "{error}");
                            self.toast = Some((
                                lang.format("cli.autostart_failed", &[("error", &error)]),
                                Instant::now(),
                            ));
                        }
                    }
                }
                settings_separator(ui, p);
                *changed |= toggle_row(
                    ui,
                    p,
                    lang.t("ui.auto_check_updates"),
                    &mut self.settings.update_check,
                );
                settings_separator(ui, p);
                ui.horizontal(|ui| {
                    ui.set_height(38.0);
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(
                            lang.format("settings.version", &[("version", crate::update::CURRENT)]),
                        )
                        .size(11.0)
                        .color(p.subtext),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if settings_action(ui, p, lang.t("ui.check_now"), self.update.can_check())
                            .clicked()
                        {
                            self.check_update();
                        }
                    });
                });
                self.update_check_history(ui, p, lang);
                settings_separator(ui, p);
                let mut page = None;
                let release_page = self.update.release_page();
                ui.horizontal(|ui| {
                    ui.set_height(30.0);
                    ui.add_space(2.0);
                    ui.visuals_mut().hyperlink_color = p.accent;
                    if ui
                        .link(RichText::new(lang.t("ui.github_repository")).size(10.5))
                        .on_hover_text(crate::update::REPOSITORY_URL)
                        .clicked()
                    {
                        page = Some(crate::update::REPOSITORY_URL);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .link(RichText::new(lang.t("ui.download_manually")).size(10.5))
                            .on_hover_text(
                                lang.t("ui.if_the_download_fails_open_github_releases_in"),
                            )
                            .clicked()
                        {
                            page = Some(release_page);
                        }
                    });
                });
                if let Some(url) = page {
                    if let Err(error) = crate::update::open_url(url) {
                        self.toast = Some((error, Instant::now()));
                    }
                }
            },
        );
        ui.add_space(8.0);
        settings_group(
            ui,
            p,
            "diagnostics_feedback",
            lang.t("ui.diagnostics_feedback"),
            |ui| {
                self.diagnostics_controls(ui, p, lang);
                ui.add_space(8.0);
                settings_separator(ui, p);
                ui.add_space(4.0);
                settings_note(
                    ui,
                    p,
                    lang.t("ui.saved_on_this_device_no_automatic_uploads"),
                );
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("ui.report_contents_sharing"))
                        .size(10.5)
                        .color(p.subtext),
                )
                .id_salt("diagnostics-contents")
                .show(ui, |ui| {
                    settings_note(
                        ui,
                        p,
                        lang.t("ui.includes_system_details_helper_status_and_recent_logs"),
                    );
                    settings_note(
                        ui,
                        p,
                        lang.t("ui.send_the_report_and_steps_to_reproduce_to"),
                    );
                });
            },
        );
    }

    fn diagnostics_controls(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let busy = self.diagnostics_job.is_some();
        #[cfg(debug_assertions)]
        let busy = busy || crate::ui_capture::diagnostics_busy();
        let export_label = match (&self.diagnostics_result, busy) {
            (_, true) => lang.t("export.in_progress"),
            (Some(Ok(_)), false) => lang.t("ui.export_again"),
            (Some(Err(_)), false) => lang.t("ui.try_again"),
            (None, false) => lang.t("ui.export_report"),
        };
        let mut export = false;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.set_min_height(42.0);
            export = ui
                .add_enabled(
                    !busy,
                    egui::Button::new(RichText::new(export_label).size(11.0).color(p.on_selection))
                        .min_size(Vec2::new(96.0, 32.0))
                        .fill(p.selection)
                        .stroke(Stroke::NONE)
                        .corner_radius(8),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 42.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    diagnostics_report_icon(ui, p);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(lang.t("ui.diagnostic_report"))
                                .font(theme::display_font(12.0))
                                .color(p.text),
                        );
                        ui.add_space(3.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(lang.t("ui.logs_and_system_details"))
                                    .size(10.5)
                                    .color(p.subtext),
                            )
                            .wrap(),
                        );
                    });
                },
            );
        });
        if tm_diagnostics::log_path().is_none() || tm_diagnostics::write_error().is_some() {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(lang.t("ui.logs_could_not_be_saved_export_diagnostics_and"))
                        .small()
                        .color(p.warn),
                )
                .wrap(),
            );
        }
        if busy {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(13.0).color(p.accent));
                ui.label(
                    RichText::new(lang.t("ui.collecting_diagnostics"))
                        .size(10.5)
                        .color(p.subtext),
                );
            });
        } else if let Some(result) = &self.diagnostics_result {
            ui.add_space(8.0);
            match result {
                Ok(path) => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(lang.t("ui.report_saved"))
                                .size(11.0)
                                .color(p.download),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if settings_action(
                                ui,
                                p,
                                lang.t("common.show_file"),
                                true,
                            )
                            .clicked()
                            {
                                if let Err(error) = crate::diagnostics::reveal(path) {
                                    log::error!(target: "tramoing::diagnostics", "打开报告位置失败：{error}");
                                    self.toast = Some((error, Instant::now()));
                                }
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(path.file_name().unwrap_or_default().to_string_lossy())
                                .size(10.5)
                                .color(p.subtext),
                        )
                        .truncate()
                        .selectable(true),
                    )
                    .on_hover_text(path.display().to_string());
                }
                Err(error) => {
                    ui.label(
                        RichText::new(lang.t("ui.export_failed_please_try_again"))
                            .size(11.0)
                            .color(p.warn),
                    );
                    egui::CollapsingHeader::new(
                        RichText::new(lang.t("ui.error_details"))
                            .size(10.5)
                            .color(p.subtext),
                    )
                    .id_salt("diagnostics-export-error")
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(error).size(10.5).color(p.warn))
                                .wrap()
                                .selectable(true),
                        );
                    });
                }
            }
        }
        if export {
            self.export_diagnostics(ui.ctx());
        }
    }

    fn export_diagnostics(&mut self, ctx: &egui::Context) {
        if self.diagnostics_job.is_some() {
            return;
        }
        let mut status = match &self.latest {
            Some(snapshot) => format!(
                "sampler_error={}\nstore_error={}\nmock={}\n",
                snapshot.error.as_deref().unwrap_or("none"),
                snapshot.store_error.as_deref().unwrap_or("none"),
                snapshot.mock
            ),
            None => "No sampler snapshot received\n".into(),
        };
        // Do not include app lists, traffic totals, or update URLs in the report.
        let viewport = ctx.input(|i| i.viewport().clone());
        status.push_str(&format!(
            "ui_pixels_per_point={}\nui_native_pixels_per_point={:?}\nui_content_size={:?}\nui_inner_rect={:?}\nui_outer_rect={:?}\n",
            ctx.pixels_per_point(), viewport.native_pixels_per_point,
            ctx.content_rect().size(), viewport.inner_rect, viewport.outer_rect
        ));
        if let Some(prompt) = &self.error_prompt {
            status.push_str(&format!("\n[触发提示的错误]\n{}\n", prompt.context()));
        }
        status.push_str(&format!("update_busy={}\n", self.update.is_busy()));
        if let Some(reading) = self.performance.reading() {
            status.push_str(&format!(
                "temperature_cpu_c={:?}\ntemperature_gpu_c={:?}\ntemperature_source={}\n",
                reading.temperatures.cpu, reading.temperatures.gpu, reading.temperatures.source
            ));
        }
        status.push_str(&format!(
            "performance_error={}\n",
            self.performance.error.as_deref().unwrap_or("none")
        ));
        #[cfg(windows)]
        status.push_str(&format!(
            "network_verified={}\nnetwork_error={}\n",
            self.network_access.verified,
            self.network_access.error.as_deref().unwrap_or("none")
        ));
        let wake = ctx.clone();
        self.diagnostics_result = None;
        match crate::export::ExportJob::start(
            move || crate::diagnostics::export_report(&status),
            move || wake.request_repaint(),
        ) {
            Ok(job) => self.diagnostics_job = Some(job),
            Err(error) => {
                log::error!(target: "tramoing::diagnostics", "启动诊断导出失败：{error}");
                self.diagnostics_result = Some(Err(error.to_string()));
            }
        }
    }

    fn status_bar_options(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        changed: &mut bool,
    ) {
        let bar_dark = self.bar_is_dark();
        settings_group(ui, p, "status_icon", lang.t("settings.app_icon"), |ui| {
            *changed |= visibility_toggle_row(
                ui,
                p,
                lang,
                lang.t("settings.show_icon"),
                &mut self.settings.tray_icon,
                self.settings.tray_badge || self.settings.performance_enabled,
            );
        });
        ui.add_space(8.0);
        settings_group(ui, p, "network_speed", lang.t("ui.network_speed"), |ui| {
            *changed |= visibility_toggle_row(
                ui,
                p,
                lang,
                lang.t("ui.show_network_speed"),
                &mut self.settings.tray_badge,
                self.settings.tray_icon || self.settings.performance_enabled,
            );
            if self.settings.tray_badge {
                settings_separator(ui, p);
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("appearance.colors"),
                    &mut self.settings.tray_color,
                    &[
                        (true, lang.t("appearance.color")),
                        (false, lang.t("appearance.mono")),
                    ],
                );
                if self.settings.tray_color {
                    *changed |= color_option_row(
                        ui,
                        p,
                        lang,
                        lang.t("network.download"),
                        &mut self.settings.display_colors.download,
                        tm_core::color::download(bar_dark),
                    );
                    *changed |= color_option_row(
                        ui,
                        p,
                        lang,
                        lang.t("network.upload"),
                        &mut self.settings.display_colors.upload,
                        tm_core::color::upload(bar_dark),
                    );
                }
                #[cfg(windows)]
                if self.settings.performance_enabled {
                    settings_note(
                        ui,
                        p,
                        lang.t("ui.drag_the_taskbar_items_to_change_their_order"),
                    );
                }
            } else {
                settings_note(ui, p, lang.t("ui.show_live_upload_and_download_speeds"));
            }
        });
        ui.add_space(8.0);
        #[cfg(any(target_os = "macos", windows))]
        self.performance_options(ui, p, lang, changed);
    }

    fn statistics_options(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        changed: &mut bool,
    ) {
        settings_group(
            ui,
            p,
            "application_list",
            lang.t("ui.application_list"),
            |ui| {
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("ui.max_apps"),
                    &mut self.settings.max_apps,
                    &[(5, "5"), (10, "10"), (20, "20")],
                );
                settings_note(
                    ui,
                    p,
                    lang.t("ui.pinned_apps_stay_first_scroll_to_see_more"),
                );
            },
        );
        ui.add_space(8.0);
        settings_group(ui, p, "history_alerts", lang.t("ui.history_alerts"), |ui| {
            *changed |= option_row(
                ui,
                p,
                lang.t("ui.keep_history"),
                &mut self.settings.retention_days,
                &[
                    (7, lang.t("ui.7_days")),
                    (30, lang.t("ui.30_days")),
                    (90, lang.t("ui.90_days")),
                ],
            );
            settings_separator(ui, p);
            *changed |= option_row(
                ui,
                p,
                lang.t("ui.traffic_alert"),
                &mut self.settings.alert_mb,
                &[
                    (0, lang.t("ui.off")),
                    (100, "100M"),
                    (500, "500M"),
                    (1024, "1G"),
                ],
            );
            settings_note(ui, p, lang.t("ui.alert_when_an_app_exceeds_this_amount_in"));
        });
    }
    fn bar_is_dark(&self) -> bool {
        #[cfg(windows)]
        {
            !self.taskbar_light
        }
        #[cfg(target_os = "macos")]
        {
            self.tray
                .as_ref()
                .filter(|tray| tray.is_visible())
                .or(self.speed_tray.as_ref().filter(|tray| tray.is_visible()))
                .or(self
                    .performance_tray
                    .as_ref()
                    .filter(|tray| tray.is_visible()))
                .is_none_or(|tray| tray.appearance_is_dark())
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    fn performance_options(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        changed: &mut bool,
    ) {
        let bar_dark = self.bar_is_dark();
        settings_group(ui, p, "performance", lang.t("ui.performance"), |ui| {
            *changed |= visibility_toggle_row(
                ui,
                p,
                lang,
                lang.t("ui.show_cpu_gpu"),
                &mut self.settings.performance_enabled,
                self.settings.tray_icon || self.settings.tray_badge,
            );
            if self.settings.performance_enabled {
                settings_separator(ui, p);
                *changed |= toggle_row(
                    ui,
                    p,
                    lang.t("ui.show_temperature"),
                    &mut self.settings.performance_temperature,
                );
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("performance.refresh"),
                    &mut self.settings.performance_interval,
                    &[
                        (1, lang.t("ui.1s")),
                        (2, lang.t("ui.2s")),
                        (5, lang.t("ui.5s")),
                    ],
                );
                *changed |= option_row(
                    ui,
                    p,
                    lang.t("appearance.colors"),
                    &mut self.settings.performance_color,
                    &[
                        (true, lang.t("appearance.color")),
                        (false, lang.t("appearance.mono")),
                    ],
                );
                if self.settings.performance_color {
                    *changed |= color_option_row(
                        ui,
                        p,
                        lang,
                        "CPU",
                        &mut self.settings.display_colors.cpu,
                        tm_core::color::cpu(bar_dark),
                    );
                    *changed |= color_option_row(
                        ui,
                        p,
                        lang,
                        "GPU",
                        &mut self.settings.display_colors.gpu,
                        tm_core::color::gpu(bar_dark),
                    );
                }
                let reading = self.performance.reading();
                ui.add_space(6.0);
                settings_separator(ui, p);
                if self.performance.error.is_some() {
                    settings_note(
                        ui,
                        p,
                        lang.t("ui.sampler_unavailable_toggle_off_and_on_to_retry"),
                    );
                } else {
                    performance_readouts(
                        ui,
                        p,
                        lang,
                        reading.as_ref(),
                        self.settings.performance_temperature,
                    );
                }
                #[cfg(windows)]
                if self.settings.performance_temperature {
                    settings_separator(ui, p);
                    ui.add_space(6.0);
                    self.sensor_component_controls(ui, p, lang);
                    if let Some(thermal) = reading
                        .as_ref()
                        .map(|r| &r.temperatures)
                        .filter(|r| r.cpu.is_none())
                    {
                        use tm_core::temperature::CpuUnavailable as Reason;
                        let message = match thermal.cpu_unavailable {
                            Some(Reason::Starting) => lang.t("ui.reading_cpu_temperature"),
                            Some(Reason::HelperUnavailable) => {
                                lang.t("ui.cpu_sensor_helper_is_unavailable_install_or_start")
                            }
                            Some(Reason::ComponentMissing | Reason::DriverMissing)
                                if matches!(
                                    self.sensor_components.phase,
                                    crate::sensor_components::Phase::Installed { .. }
                                ) =>
                            {
                                lang.t("sensors.changed")
                            }
                            // The component manager offers installation or repair above.
                            Some(
                                Reason::ComponentMissing
                                | Reason::DriverMissing
                                | Reason::UnsupportedPlatform,
                            ) => "",
                            Some(Reason::DriverUnavailable) => {
                                lang.t("ui.cpu_sensor_driver_is_unavailable_check_its_installation")
                            }
                            Some(Reason::PermissionDenied) => lang.t(
                                "ui.cpu_sensor_helper_needs_administrator_privileges_repair_the",
                            ),
                            Some(Reason::Failed) => lang.t(
                                "ui.cpu_temperature_reading_failed_and_will_retry_automatically",
                            ),
                            _ => lang.t("ui.no_cpu_temperature_sensor_is_available_a_running"),
                        };
                        if !message.is_empty() {
                            settings_note(ui, p, message).on_hover_text(&thermal.source);
                        }
                    }
                }
            } else {
                settings_note(ui, p, lang.t("performance.display_hint"));
            }
        });
    }

    #[cfg(windows)]
    fn sensor_component_controls(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        use crate::sensor_components::Phase;
        if matches!(self.sensor_components.phase, Phase::Idle) {
            // Inspecting the machine never contacts a download server.
            self.sensor_components.inspect();
        }
        let phase = self.sensor_components.phase.clone();
        let update_busy = self.update.is_busy();
        match phase {
            Phase::Idle | Phase::Checking => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                    settings_note(ui, p, lang.t("sensors.checking"));
                });
            }
            Phase::Missing { bytes, reused } => {
                settings_note(ui, p, lang.t("sensors.missing"));
                if !reused.is_empty() {
                    settings_note(
                        ui,
                        p,
                        &lang.format("sensors.reusing", &[("items", &reused.join(", "))]),
                    );
                }
                let label = lang.format(
                    "sensors.download_size",
                    &[("size", &format!("{:.1}", bytes as f64 / 1_000_000.0))],
                );
                ui.horizontal_wrapped(|ui| {
                    if settings_action(ui, p, &label, !update_busy).clicked() {
                        self.sensor_components.download();
                    }
                    if settings_action(ui, p, lang.t("sensors.recheck"), true).clicked() {
                        self.sensor_components.inspect();
                    }
                });
            }
            Phase::Downloading { pct, label } | Phase::Pausing { pct, label } => {
                settings_note(
                    ui,
                    p,
                    &lang.format("sensors.downloading", &[("item", &label)]),
                );
                ui.add(
                    egui::ProgressBar::new(pct)
                        .desired_width(ui.available_width())
                        .show_percentage()
                        .fill(p.accent),
                );
                ui.add_space(5.0);
                let pausing = matches!(self.sensor_components.phase, Phase::Pausing { .. });
                if settings_action(
                    ui,
                    p,
                    lang.t(if pausing {
                        "sensors.pausing"
                    } else {
                        "sensors.pause"
                    }),
                    !pausing,
                )
                .clicked()
                {
                    self.sensor_components.pause();
                }
            }
            Phase::Paused { pct, error } => {
                if let Some(error) = error {
                    settings_note(
                        ui,
                        p,
                        &lang.format("sensors.download_failed", &[("error", &error)]),
                    );
                } else {
                    settings_note(
                        ui,
                        p,
                        &lang.format(
                            "sensors.paused",
                            &[("percent", &format!("{:.0}", pct * 100.0))],
                        ),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if settings_action(ui, p, lang.t("sensors.resume"), !update_busy).clicked() {
                        self.sensor_components.download();
                    }
                    if settings_action(ui, p, lang.t("sensors.recheck"), true).clicked() {
                        self.sensor_components.inspect();
                    }
                });
            }
            Phase::Ready { error } => {
                settings_note(ui, p, lang.t("sensors.ready"));
                if let Some(error) = error {
                    settings_note(
                        ui,
                        p,
                        &lang.format("sensors.install_failed", &[("error", &error)]),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if settings_action(ui, p, lang.t("sensors.install"), !update_busy).clicked() {
                        self.sensor_components.install();
                    }
                    if settings_action(ui, p, lang.t("sensors.recheck"), true).clicked() {
                        self.sensor_components.inspect();
                    }
                });
            }
            Phase::Installing => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                    settings_note(ui, p, lang.t("sensors.installing"));
                });
            }
            Phase::Installed { needs_restart } => {
                let (_, recheck) = egui::containers::Sides::new()
                    .shrink_left()
                    .height(30.0)
                    .show(
                        ui,
                        |ui| {
                            let (status, color) = if needs_restart {
                                (lang.t("sensors.restart_required"), p.warn)
                            } else {
                                (lang.t("sensors.installed"), p.download)
                            };
                            let (dot, _) =
                                ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot.center(), 2.5, color);
                            ui.add(
                                egui::Label::new(RichText::new(status).size(11.5).color(p.text))
                                    .wrap(),
                            )
                            .on_hover_text(lang.t("sensors.reuse_hint"));
                        },
                        |ui| settings_action(ui, p, lang.t("sensors.recheck"), !update_busy),
                    );
                if recheck.clicked() {
                    self.sensor_components.inspect();
                }
            }
            Phase::Unsupported => {
                settings_note(ui, p, lang.t("sensors.unsupported"));
            }
            Phase::Error(error) => {
                settings_note(
                    ui,
                    p,
                    &lang.format("sensors.check_failed", &[("error", &error)]),
                );
                if settings_action(ui, p, lang.t("sensors.recheck"), !update_busy).clicked() {
                    self.sensor_components.inspect();
                }
            }
        }
        if update_busy && !self.sensor_components.is_busy() {
            settings_note(ui, p, lang.t("sensors.wait_for_update"));
        }
    }

    /// 设置改动后立刻生效并落盘。
    fn apply_settings(&mut self, ctx: &egui::Context) {
        self.settings.save();
        ctx.set_theme(theme_pref(self.settings.theme));
        self.lang = crate::i18n::resolve(self.settings.lang);
        self.retention
            .store(self.settings.retention_days, Ordering::Relaxed);
        #[cfg(any(target_os = "macos", windows))]
        self.performance.configure(
            self.settings.performance_enabled,
            self.settings.performance_interval,
            self.settings.performance_temperature,
        );
        #[cfg(target_os = "macos")]
        {
            self.speed_title.clear();
            self.speed_tray_retry = Instant::now();
            self.performance_title.clear();
            self.performance_tray_retry = Instant::now();
            self.sync_speed_tray();
            self.sync_performance_tray();
        }
        self.sync_tray_icon();
        #[cfg(windows)]
        self.sync_badge(ctx);
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
                RichText::new(lang.t("ui.network"))
                    .font(theme::display_font(17.0))
                    .color(p.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let entry = sliders_button(ui, p).on_hover_text(lang.t("settings.title"));
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
                                lang.t("ui.24h")
                            } else {
                                lang.t("ui.session")
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

        const H: f32 = 78.0;
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
                lang.t("network.download"),
                rx_text,
                &rx_trend,
                p.download,
                p,
            )
            .on_hover_text(lang.t("ui.recent_download_activity_independent_scale"));

            ui.add_space(METRIC_GAP);

            metric(
                ui,
                col,
                H,
                "↑",
                lang.t("network.upload"),
                tx_text,
                &tx_trend,
                p.upload,
                p,
            )
            .on_hover_text(lang.t("ui.recent_upload_activity_independent_scale"));
        });
    }

    /// 列表上方的一行：左边「应用 · N」，右边时间窗口分段控件。
    fn list_header(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        // 和列表共用筛选逻辑，计数包含没有流量的置顶项。
        let count = self.listed_rates().len();

        // 先按控件高度建立整行，再排文字；默认横排的初始高度较小，
        // 后续按钮撑高后不会重新居中已排好的标签。
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), SEG_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.label(
                    RichText::new(lang.format("apps.count", &[("count", &count.to_string())]))
                        .font(theme::display_font(14.0))
                        .color(p.text),
                );
                #[cfg(windows)]
                self.network_panel.entry(ui, &self.network_access, p, lang);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(SEG_W, SEG_H), egui::Sense::hover());
                    segmented_at(ui, rect, &mut self.window, p, lang);
                });
            },
        );
    }

    fn listed_rates(&self) -> Vec<AppRate> {
        #[cfg(windows)]
        let restricted: Vec<_> = self
            .network_access
            .rules
            .iter()
            .map(|rule| rule.path.as_str())
            .collect();
        #[cfg(not(windows))]
        let restricted = Vec::new();
        app_list_rates_with_restrictions(
            self.latest.as_ref(),
            self.window,
            &self.settings,
            &restricted,
        )
    }

    fn list(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, reveal: f32, has_alert: bool) {
        // 列表用满页脚与提醒行之外的空间，不按整行取整，以免底部空出半行。
        let reserve = FOOTER_BAND + if has_alert { ALERT_H } else { 0.0 };
        let avail = (ui.available_height() - reserve).max(0.0);
        // 一次性分配整个列表区。allocate_ui_with_layout 只占用内容实际高度，
        // 空状态原来因此少占半块区域，把下面的提醒行留在面板中间。
        let (body, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), avail), egui::Sense::hover());
        let mut content = ui.new_child(egui::UiBuilder::new().max_rect(body));
        content.set_clip_rect(body.intersect(ui.clip_rect()));
        let ui = &mut content;

        // 先定下本帧列表：置顶项按固定顺序保留，剩余名额按采样器的流量排名填充。
        // 使用 owned 数据，下面才能 &mut self 取图标和切换置顶。
        let rates = self.listed_rates();
        let loading = self.latest.is_none() && rates.is_empty();
        let note = if loading {
            None
        } else if let Some(error) = self.latest.as_ref().and_then(|s| s.error.as_deref()) {
            Some(error)
        } else if rates.is_empty() {
            // 没有置顶项且所选窗口没有流量时才显示空状态。
            Some(if self.window == Window::Live {
                lang.t("ui.no_traffic_right_now")
            } else {
                lang.t("ui.no_traffic_in_this_window")
            })
        } else {
            None
        };

        if std::env::var_os("TM_TRACE_LIST").is_some() {
            eprintln!(
                "[list] avail={avail:.1} rows={} loading={loading}",
                rates.len()
            );
        }

        if loading {
            // 采样器要两个采样点才能差分出速率，第一个可用数据大约在 1–2 秒后。
            // 这段时间画骨架而不是留白：空面板看起来像坏了。
            let rows = ((avail + ROW_GAP) / ROW_H).ceil() as usize;
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), avail),
                Layout::top_down(Align::Min),
                |ui| self.skeleton(ui, p, rows),
            );
        } else if let Some(text) = note.filter(|_| rates.is_empty()) {
            let text = text.to_owned();
            if self.latest.as_ref().is_some_and(|s| s.error.is_some()) {
                egui::ScrollArea::vertical()
                    .id_salt("collector-error")
                    .max_height(avail)
                    .show(ui, |ui| {
                        ui.add_space(24.0);
                        ui.add(egui::Label::new(RichText::new(&text).small().color(p.warn)).wrap());
                        ui.add_space(12.0);
                        self.diagnostics_controls(ui, p, lang);
                        settings_note(
                            ui,
                            p,
                            lang.t("ui.send_the_report_and_steps_to_reproduce_to_2"),
                        );
                    });
            } else {
                empty_note(ui, &text, avail, p);
            }
        } else {
            let note = note.map(str::to_owned);
            let out = app_list_scroll_area(avail, self.next_list_scroll.take()).show(ui, |ui| {
                // 让出右侧过道：行盒的描边不能和指示条叠在一起
                ui.set_width(ui.available_width() - SCROLL_GUTTER);
                // 行距自己控制，不叠加默认 item_spacing。
                ui.spacing_mut().item_spacing.y = 0.0;
                if let Some(text) = note.as_deref() {
                    ui.label(RichText::new(text).small().color(p.warn));
                    self.diagnostics_controls(ui, p, lang);
                    ui.add_space(ROW_GAP);
                }
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
                Stroke::new(0.5_f32, p.border.gamma_multiply(pulse * fade)),
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
            let right = pin_button_rect(rect).left() - PIN_GAP;
            let tx_x = right - NUM_W_TX;
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
            painter.rect_filled(pin_button_rect(rect).shrink(7.0), 3.0, color);
            if i + 1 < rows {
                ui.add_space(ROW_GAP);
            }
        }
        // 呼吸动画需要持续重绘，否则骨架是静止的
        ui.ctx().request_repaint();
    }

    /// 一行：图标 + 名字 + 下行速率 + 上行速率 + 置顶按钮。
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
        let (resp, pin_resp) = app_row_actions(ui, rect, r.app.as_str(), &mut self.settings);
        #[cfg(windows)]
        let network_resp = network_row_action(ui, rect, r.app.as_str());
        let pinned = self.settings.is_pinned(r.app.as_str());
        if pin_resp.changed() {
            self.settings.save();
            if pinned {
                self.next_list_scroll = Some(0.0);
            }
            ui.ctx().request_repaint();
        }

        // 悬停：底色抬一档 + 描边转向强调色，120ms 过渡。
        //
        // 每一项本来就有描边，所以悬停不需要再加色块——只把这一格的描边点亮，
        // 视线就知道「操作的是这个」。渐变是为了不闪。
        let t = ui.ctx().animate_bool_with_time(
            ui.id().with(("row-hover", r.app.as_str())),
            resp.hovered() || pin_resp.hovered(),
            0.12,
        );
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(ROW_RADIUS),
            mix(p.surface, p.accent, t * 0.08),
            Stroke::new(0.7_f32, mix(p.border, p.accent, t * 0.5)),
            egui::StrokeKind::Inside,
        );

        // 数字与行末图钉分开，各自保留稳定的列宽。
        let right = pin_resp.rect.left() - PIN_GAP;

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
        let tx_right = right;
        let rx_right = tx_right - NUM_W_TX - NUM_GAP;
        let name_left = icon.right() + 9.0;

        let name_rect = egui::Rect::from_min_max(
            egui::pos2(name_left, rect.top()),
            egui::pos2(rx_right - NUM_W_RX - 5.0, rect.bottom()),
        );
        ellipsis_text(painter, name_rect, &name, 13.0, p.text, Align::Min);
        surface::focus(painter, resp.rect, &resp, p);
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
        paint_pin_button(ui, &pin_resp, pinned, p);
        let pin_label = self.lang.format("apps.pin_named", &[("name", &name)]);
        pin_resp.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                ui.is_enabled(),
                pinned,
                &pin_label,
            )
        });
        pin_resp.on_hover_text(format!(
            "{} · {name}",
            if pinned {
                self.lang.t("apps.unpin")
            } else {
                self.lang.t("apps.pin")
            }
        ));
        let resp = resp.on_hover_text(format!("{}\n{}", name, tildify(r.app.as_str())));
        #[cfg(windows)]
        self.network_button(ui, &network_resp, r.app.as_str(), p);
        egui::Popup::from_toggle_button_response(&resp)
            .width(280.0)
            .frame(
                egui::Frame::popup(ui.style())
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0_f32, p.border))
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
                #[cfg(windows)]
                if self.network_access.rule(r.app.as_str()).is_some() {
                    ui.colored_label(
                        p.warn,
                        if self.network_access.verified
                            && self
                                .network_access
                                .rule(r.app.as_str())
                                .is_some_and(|r| r.complete)
                        {
                            self.lang.t("ui.blocked_use_allow_to_restore_access")
                        } else {
                            self.lang.t("ui.network_rules_need_verification")
                        },
                    );
                }
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

    #[cfg(windows)]
    fn network_button(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        path: &str,
        p: &Palette,
    ) {
        let restricted = self.network_access.rule(path).is_some();
        let complete = self.network_access.verified
            && self.network_access.rule(path).is_some_and(|r| r.complete);
        let pending = self
            .network_access
            .pending_path
            .as_deref()
            .is_some_and(|p| tm_windows::network_policy::same_path(p, path));
        let invalid = tm_windows::network_policy::validate_path(path).err();
        let reason = invalid.or_else(|| self.network_access.unavailable_reason(self.lang));
        let enabled = reason.is_none() && !self.network_access.busy();
        let label = if pending {
            self.lang.t("ui.changing_network_access")
        } else if restricted {
            self.lang.t("ui.allow_network_access")
        } else {
            self.lang.t("ui.block_network_access")
        };
        let color = if !enabled {
            p.subtext
        } else if restricted {
            p.warn
        } else {
            p.accent
        };
        let background = mix(
            p.surface,
            color,
            if response.hovered() && enabled {
                0.18
            } else {
                0.08
            },
        );
        let painter = ui.painter();
        painter.rect(
            response.rect.shrink(2.0),
            CornerRadius::same(6),
            background,
            Stroke::new(
                0.7_f32,
                mix(p.border, color, if complete { 0.7 } else { 0.3 }),
            ),
            egui::StrokeKind::Inside,
        );
        let center = response.rect.center();
        let stroke = Stroke::new(1.1_f32, color);
        if pending {
            let phase = ui.input(|i| i.time) as f32 * 4.0;
            let points = (0..=24)
                .map(|i| {
                    let angle = phase + i as f32 / 24.0 * std::f32::consts::TAU * 0.75;
                    center + Vec2::angled(angle) * 5.8
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.4_f32, color)));
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else {
            // 地球表示网络权限；仅在系统核验成功后画断网斜线。
            painter.circle_stroke(center, 6.0, stroke);
            painter.line_segment(
                [center - Vec2::new(6.0, 0.0), center + Vec2::new(6.0, 0.0)],
                stroke,
            );
            let meridian = (0..32)
                .map(|i| {
                    let angle = i as f32 / 32.0 * std::f32::consts::TAU;
                    center + Vec2::new(2.7 * angle.cos(), 6.0 * angle.sin())
                })
                .collect();
            painter.add(egui::Shape::closed_line(meridian, stroke));
            if complete {
                let slash = [center - Vec2::splat(5.5), center + Vec2::splat(5.5)];
                painter.line_segment(slash, Stroke::new(3.8_f32, background));
                painter.line_segment(slash, Stroke::new(1.5_f32, color));
            } else if restricted {
                // 缓存或不完整规则仍提供恢复入口，以圆点标记待核验状态。
                let badge = center + Vec2::new(5.0, -5.0);
                painter.circle(badge, 2.5, background, Stroke::new(1.0_f32, p.warn));
                painter.circle_filled(badge, 0.7, p.warn);
            }
        }
        surface::focus(painter, response.rect, response, p);
        if enabled && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                enabled,
                format!("{label} · {}", pretty_name(path)),
            )
        });
        response.clone().on_hover_text(if pending {
            label
        } else if let Some(reason) = reason.as_deref() {
            reason
        } else if complete {
            self.lang.t("ui.blocked_click_to_allow_network_access")
        } else if restricted {
            self.lang.t("ui.rules_need_verification_click_to_remove")
        } else {
            self.lang
                .t("ui.allowed_click_to_block_including_local_proxy_connections")
        });
        if enabled && response.clicked() {
            self.network_access.set(path, !restricted, ui.ctx());
        }
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
        let demo = self
            .latest
            .as_ref()
            .is_some_and(|s| s.mock && s.store_error.is_none());
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
            return Some(lang.format("errors.taskbar_failed", &[("error", error)]));
        }
        if let Some(s) = &self.latest {
            // 采集异常已经在列表区显示，底部只承载其他独立提醒。
            if let Some(e) = &s.store_error {
                return Some(lang.format("errors.history_failed", &[("error", e)]));
            }
            if s.mock {
                return Some(lang.t("ui.demo_data_history_is_not_saved").into());
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
        Some(lang.format(
            "alerts.traffic_used",
            &[("name", &name), ("amount", &format_bytes(bytes))],
        ))
    }

    /// 页脚：导出 / 退出之间常驻本机 IP，有更新或临时提示时上下分行。
    ///
    /// **整条带子只按面板底边定位、内容全靠 painter 落笔**。以前是一条流式的
    /// `add_space(8) + hairline + add_space(4) + 两个 Button`：每次 `allocate_*`
    /// 都会再插一次 `item_spacing`，加上按钮高度跟着字形走（中文 10.5pt 与英文
    /// 字高不同），页脚实际高度比 [`FOOTER_BAND`] 多出十几点，把内容顶出了
    /// 面板底边；换语言、换字号时还会变一次。现在高度恒等于 [`FOOTER_BAND`]。
    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, full: egui::Rect) {
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

        let export_label = if self.export_job.is_some() {
            lang.t("export.in_progress_2")
        } else {
            lang.t("ui.export")
        };
        let action_width = |label: &str| {
            (ui.painter()
                .layout_no_wrap(label.to_owned(), egui::FontId::proportional(12.5), p.text)
                .size()
                .x
                + 35.0)
                .clamp(64.0, row.width() * 0.30)
        };
        let export_width = action_width(export_label);
        let quit_width = action_width(lang.t("common.quit"));
        let export_rect = egui::Rect::from_min_size(row.min, Vec2::new(export_width, row.height()));
        let export = action_button(
            ui,
            export_rect,
            "footer-export",
            export_label,
            p,
            paint_export_icon,
        )
        .on_hover_text(lang.t("ui.export_history"));
        if export.clicked() && self.export_job.is_none() {
            self.export_history(ui.ctx());
        }

        // IP 常驻。提示放在上半行，避免待下载/待安装状态长期遮住地址。
        // 更新仍优先于普通提示。
        // 两侧按钮宽度不同，取较大边距保持 IP 对齐整个面板的水平中心。
        let center_inset = export_width.max(quit_width) + 8.0;
        let left = row.left() + center_inset;
        let right = row.right() - center_inset;
        let center_rect =
            egui::Rect::from_min_max(egui::pos2(left, row.top()), egui::pos2(right, row.bottom()));
        let mut ip_rect = center_rect;
        if self.update_text(lang).is_some() || toast.is_some() {
            let mut status_rect = center_rect;
            status_rect.max.y = center_rect.center().y;
            ip_rect.min.y = status_rect.bottom();
            if !self.update_strip(ui, p, lang, status_rect) {
                if let Some(msg) = &toast {
                    ellipsis_text(
                        ui.painter(),
                        status_rect,
                        msg,
                        10.0,
                        p.subtext,
                        Align::Center,
                    );
                    ui.interact(
                        status_rect,
                        ui.id().with("footer-toast"),
                        egui::Sense::hover(),
                    )
                    .on_hover_text(msg);
                }
            }
        }
        self.device_ip_strip(ui, p, lang, ip_rect);

        let quit_rect = egui::Rect::from_min_size(
            egui::pos2(row.right() - quit_width, row.top()),
            Vec2::new(quit_width, row.height()),
        );
        let quit = action_button(
            ui,
            quit_rect,
            "footer-quit",
            lang.t("common.quit"),
            p,
            paint_quit_icon,
        )
        .on_hover_text(lang.t("common.quit"));
        if quit.clicked() {
            #[cfg(windows)]
            if self.sensor_components.is_installing() {
                self.toast = Some((lang.t("sensors.finish_before_quit").into(), Instant::now()));
                return;
            }
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn device_ip_strip(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, rect: egui::Rect) {
        let address = self.device_ip.address();
        let loading = self.device_ip.is_loading();
        let label = address.map_or_else(
            || {
                if loading {
                    lang.t("ui.ip_loading")
                } else {
                    lang.t("ui.ip_unavailable")
                }
                .to_owned()
            },
            |ip| format!("IP {ip}"),
        );
        let response = ui
            .interact(
                rect,
                ui.id().with("footer-device-ip"),
                if address.is_some() {
                    egui::Sense::click()
                } else {
                    egui::Sense::hover()
                },
            )
            .on_hover_text(address.map_or_else(
                || {
                    if loading {
                        lang.t("ui.reading_local_ip_address")
                    } else {
                        lang.t("ui.no_local_ip_address_available")
                    }
                    .to_owned()
                },
                |ip| lang.format("network.ip_tooltip", &[("ip", &ip.to_string())]),
            ));
        let color = if address.is_some() && response.hovered() {
            p.accent
        } else {
            p.subtext
        };
        ellipsis_text(ui.painter(), rect, &label, 11.5, color, Align::Center);
        if address.is_some() {
            surface::focus(ui.painter(), rect, &response, p);
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        if response.clicked() {
            if let Some(ip) = address {
                ui.ctx().copy_text(ip.to_string());
            }
        }
    }

    /// 更新状态对应的文案。`None` 表示这一帧没有要显示的。
    ///
    /// 主面板页脚和设置页标题共用；在「启动与更新」中检查后，切换页签仍能看到进度。
    fn update_text(&self, lang: Lang) -> Option<(String, bool)> {
        use crate::update::Phase;
        let (text, actionable) = match &self.update.phase {
            Phase::Idle => return None,
            Phase::Checking => (lang.t("ui.checking").to_owned(), false),
            Phase::Latest => (lang.t("ui.up_to_date").to_owned(), false),
            Phase::Available(a) => (
                lang.format("update.release_page", &[("version", &a.version)]),
                true,
            ),
            Phase::Downloading { version, pct } => (
                lang.format(
                    "update.downloading",
                    &[
                        ("version", version),
                        ("percent", &format!("{:.0}", pct * 100.0)),
                    ],
                ),
                true,
            ),
            Phase::Retrying { pct, retry_at, .. } => {
                let seconds = retry_at.saturating_duration_since(Instant::now()).as_secs();
                (
                    lang.format(
                        "update.retrying",
                        &[
                            ("percent", &format!("{:.0}", pct * 100.0)),
                            ("seconds", &seconds.to_string()),
                        ],
                    ),
                    true,
                )
            }
            Phase::Pausing { .. } => (lang.t("ui.pausing_saving_progress").into(), false),
            Phase::Paused {
                error: Some(error), ..
            } => (
                lang.format("update.download_error", &[("error", error)]),
                true,
            ),
            Phase::Paused { pct, .. } => (
                lang.format(
                    "update.paused",
                    &[("percent", &format!("{:.0}", pct * 100.0))],
                ),
                true,
            ),
            Phase::Ready {
                launch_error: Some(error),
                ..
            } => (
                lang.format("update.install_error", &[("error", error)]),
                true,
            ),
            Phase::Ready { version, .. } => {
                let key = if self.update.is_install_deferred() {
                    "update.deferred"
                } else {
                    "update.ready"
                };
                (lang.format(key, &[("version", version)]), true)
            }
            Phase::Installing { .. } => (lang.t("ui.preparing_update").to_owned(), false),
            Phase::Restarting => (lang.t("ui.restarting").to_owned(), false),
            Phase::Error(e) => (lang.format("update.check_error", &[("error", e)]), false),
        };
        Some((text, actionable))
    }

    /// 点更新提示之后干什么。两个入口（页脚、设置页）共用。
    fn update_action(&mut self, ctx: &egui::Context) {
        use crate::update::Phase;
        #[cfg(windows)]
        if self.sensor_components.is_installing() {
            self.toast = Some((
                self.lang.t("sensors.finish_install_first").into(),
                Instant::now(),
            ));
            return;
        }
        // 先拷一份：下面要 &mut self，不能挂着对 phase 的借用
        match &self.update.phase.clone() {
            // 清单里没有本平台的包时退而求其次：打开下载页。
            // 直接什么都不做的话，用户看到「可点」却点不动，比不提示更糟。
            Phase::Available(_) => {
                if let Err(e) = crate::update::open_url(self.update.release_page()) {
                    self.toast = Some((e, Instant::now()));
                }
            }
            Phase::Downloading { .. } | Phase::Retrying { .. } => self.update.pause_download(),
            Phase::Paused { .. } => self.update.resume_download(),
            Phase::Ready { version, .. } => {
                self.update_prompt = Some(update_ui::UpdatePrompt::restart(version.clone()));
                ctx.request_repaint();
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
    /// 下载和等待重试时可暂停，暂停后可继续；准备安装时不接受重复点击。
    fn update_strip(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        rect: egui::Rect,
    ) -> bool {
        use crate::update::Phase;
        let Some((text, actionable)) = self.update_text(lang) else {
            return false;
        };
        if rect.width() < 40.0 {
            return true;
        }
        let sense = if actionable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let mut resp = ui.allocate_rect(rect, sense).on_hover_text(&text);
        if actionable {
            resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        let color = if matches!(
            &self.update.phase,
            Phase::Error(_) | Phase::Paused { error: Some(_), .. }
        ) {
            p.warn
        } else if actionable && resp.hovered() {
            // 可点击的更新操作在悬停时使用强调色。
            p.accent
        } else {
            p.subtext
        };
        ellipsis_text(ui.painter(), rect, &text, 10.0, color, Align::Center);

        if actionable && resp.clicked() {
            self.update_action(ui.ctx());
        }
        true
    }

    /// 状态在标题右侧显示，不占用滚动区；长消息省略，悬停可查看全文。
    /// 更新状态优先于普通提示，没有消息时说明设置会自动保存。
    fn settings_status(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, rect: egui::Rect) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
        }
        let update = self.update_text(lang);
        let actionable = update.as_ref().is_some_and(|(_, a)| *a);
        let text = update
            .as_ref()
            .map(|(text, _)| text.as_str())
            .or_else(|| self.toast.as_ref().map(|(text, _)| text.as_str()))
            .unwrap_or_else(|| lang.t("ui.changes_apply_and_save_automatically"));
        let sense = if actionable {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let mut resp = ui
            .interact(rect, ui.id().with("settings-status"), sense)
            .on_hover_text(text);
        if actionable {
            resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), text)
            });
            surface::focus(ui.painter(), rect, &resp, p);
        }
        let color = if matches!(
            &self.update.phase,
            crate::update::Phase::Error(_) | crate::update::Phase::Paused { error: Some(_), .. }
        ) {
            p.warn
        } else if actionable {
            p.accent
        } else if update.is_some() || self.toast.is_some() {
            p.subtext
        } else {
            p.unit
        };
        ellipsis_text(ui.painter(), rect, text, 10.5, color, Align::Max);
        if actionable && resp.clicked() {
            self.update_action(ui.ctx());
        }
    }

    /// 把库里全部历史写成 CSV。
    fn export_history(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        match crate::export::ExportJob::start(export_history_csv, move || ctx.request_repaint()) {
            Ok(job) => {
                self.export_job = Some(job);
                self.toast = Some((
                    self.lang.t("ui.exporting_history").to_owned(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                self.toast = Some((
                    format!("{}: {error}", self.lang.t("ui.export_failed")),
                    Instant::now(),
                ));
            }
        }
    }
}

/// Background work returns data; the UI formats it using the language selected
/// when it receives the result, even if the language changed during the export.
#[derive(Debug)]
pub struct ExportedHistory {
    pub rows: u64,
    pub path: String,
}

pub fn export_history_csv() -> Result<ExportedHistory, String> {
    let result = (|| -> Result<ExportedHistory, String> {
        let path = engine::db_path();
        if !path.try_exists().map_err(|e| e.to_string())? {
            return Ok(ExportedHistory {
                rows: 0,
                path: String::new(),
            });
        }
        let store = tm_store::Store::open_read_only(&path).map_err(|e| e.to_string())?;
        if store.row_count().map_err(|e| e.to_string())? == 0 {
            return Ok(ExportedHistory {
                rows: 0,
                path: String::new(),
            });
        }
        let stamp = store.local_now_stamp().map_err(|e| e.to_string())?;
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
                    Ok(rows) => {
                        return Ok(ExportedHistory {
                            rows,
                            path: tildify(&path.to_string_lossy()),
                        })
                    }
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
    match &result {
        Ok(report) => {
            log::info!(target: "tramoing::export", "导出完成：{} 行 → {}", report.rows, report.path)
        }
        Err(error) => log::error!(target: "tramoing::export", "导出失败：{error}"),
    }
    result
}

pub fn export_message(result: &Result<ExportedHistory, String>, lang: Lang) -> String {
    match result {
        Ok(report) if report.rows == 0 => lang.t("export.no_history").into(),
        Ok(report) => lang.format(
            "export.completed",
            &[("count", &report.rows.to_string()), ("path", &report.path)],
        ),
        Err(error) => lang.format("export.failed", &[("error", error)]),
    }
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

fn settings_tabs(ui: &mut egui::Ui, p: &Palette, lang: Lang, current: &mut SettingsTab) {
    let (bar, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::hover());
    ui.painter().rect_filled(bar, 11.0, p.chip);
    let options = [
        (
            SettingsTab::StatusBar,
            if cfg!(windows) {
                lang.t("settings.taskbar")
            } else {
                lang.t("settings.menu_bar")
            },
        ),
        (SettingsTab::Statistics, lang.t("settings.statistics")),
        (SettingsTab::General, lang.t("settings.general")),
    ];
    let width = (bar.width() - 6.0) / options.len() as f32;
    // 先处理选择，再画一个连续滑动的选中背景；快速连点从当前位置续接。
    let responses: Vec<_> = options
        .iter()
        .enumerate()
        .map(|(i, (tab, label))| {
            let rect = egui::Rect::from_min_size(
                bar.min + egui::vec2(3.0 + i as f32 * width, 3.0),
                Vec2::new(width, 28.0),
            );
            let response = ui
                .interact(
                    rect,
                    ui.id().with(("settings-tab", *tab)),
                    egui::Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                *current = *tab;
            }
            (rect, response, *tab, *label)
        })
        .collect();
    let position = ui.ctx().animate_value_with_time(
        ui.id().with("settings-tab-position"),
        current.index() as f32,
        SETTINGS_TAB_ANIM.as_secs_f32(),
    );
    for (rect, response, tab, _) in &responses {
        if *tab != *current && response.hovered() {
            ui.painter()
                .rect_filled(*rect, 8.0, mix(p.chip, p.surface, 0.5));
        }
    }
    let selection = egui::Rect::from_min_size(
        bar.min + egui::vec2(3.0 + position * width, 3.0),
        Vec2::new(width, 28.0),
    );
    ui.painter().rect(
        selection,
        8.0,
        p.surface,
        Stroke::new(0.7_f32, p.border),
        egui::StrokeKind::Inside,
    );
    for (rect, response, tab, label) in responses {
        let selected = *current == tab;
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, label)
        });
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            if selected { p.accent } else { p.subtext },
        );
        surface::focus(ui.painter(), rect, &response, p);
    }
}

fn theme_options(ui: &mut egui::Ui, p: &Palette, lang: Lang, current: &mut ThemeMode) -> bool {
    let (row, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 62.0), egui::Sense::hover());
    let width = (row.width() - 16.0) / 3.0;
    let before = *current;
    for (i, (mode, label)) in [
        (ThemeMode::System, lang.t("appearance.system")),
        (ThemeMode::Light, lang.t("appearance.light")),
        (ThemeMode::Dark, lang.t("appearance.dark")),
    ]
    .into_iter()
    .enumerate()
    {
        let rect = egui::Rect::from_min_size(
            row.min + egui::vec2(i as f32 * (width + 8.0), 0.0),
            Vec2::new(width, row.height()),
        );
        let response = ui
            .interact(
                rect,
                ui.id().with(("theme-preset", i)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let selected = *current == mode;
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, label)
        });
        ui.painter().rect(
            rect,
            9.0,
            if selected || response.hovered() {
                mix(p.surface, p.accent, 0.07)
            } else {
                mix(p.surface, p.chip, 0.35)
            },
            Stroke::new(
                if selected { 1.2_f32 } else { 0.7_f32 },
                if selected { p.accent } else { p.border },
            ),
            egui::StrokeKind::Inside,
        );
        let preview = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + 22.0),
            Vec2::new(38.0, 24.0),
        );
        let light = Color32::from_rgb(235, 241, 253);
        let dark = Color32::from_rgb(35, 49, 77);
        let base = if mode == ThemeMode::Dark { dark } else { light };
        ui.painter().rect_filled(preview, 4.0, base);
        if mode == ThemeMode::System {
            ui.painter()
                .with_clip_rect(egui::Rect::from_min_max(preview.center_top(), preview.max))
                .rect_filled(preview, 4.0, dark);
        }
        ui.painter().rect_stroke(
            preview,
            4.0,
            Stroke::new(0.6_f32, mix(p.border, p.accent, 0.25)),
            egui::StrokeKind::Inside,
        );
        for (offset, width) in [(7.0, 25.0), (13.0, 16.0)] {
            let line = egui::Rect::from_min_size(
                preview.min + egui::vec2(6.0, offset),
                Vec2::new(width, 3.0),
            );
            ui.painter().rect_filled(
                line,
                1.5,
                if mode == ThemeMode::Dark {
                    Color32::from_rgb(133, 170, 222)
                } else {
                    Color32::from_rgb(88, 143, 235)
                },
            );
        }
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 14.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(11.0),
            if selected { p.accent } else { p.text },
        );
        surface::focus(ui.painter(), rect, &response, p);
        if response.clicked() {
            *current = mode;
        }
    }
    before != *current
}

fn diagnostics_report_icon(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, mix(p.surface, p.accent, 0.10));
    let page = egui::Rect::from_center_size(rect.center(), Vec2::new(14.0, 18.0));
    let stroke = Stroke::new(1.1_f32, p.accent);
    painter.rect_stroke(page, 2.5, stroke, egui::StrokeKind::Inside);
    for (offset, width) in [(5.0, 7.0), (9.0, 7.0), (13.0, 4.5)] {
        let start = page.min + egui::vec2(3.5, offset);
        painter.line_segment([start, start + egui::vec2(width, 0.0)], stroke);
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn performance_readouts(
    ui: &mut egui::Ui,
    p: &Palette,
    lang: Lang,
    reading: Option<&crate::performance::Reading>,
    show_temperature: bool,
) {
    let usage_hint = if cfg!(windows) {
        lang.t("ui.cpu_is_the_system_average_gpu_shows_the")
    } else {
        lang.t("ui.cpu_is_the_system_average_gpu_shows_the_2")
    };
    let metrics = [
        (
            "CPU",
            reading.and_then(|r| r.cpu),
            reading.and_then(|r| r.temperatures.cpu),
        ),
        (
            "GPU",
            reading.and_then(|r| r.gpu),
            reading.and_then(|r| r.temperatures.gpu),
        ),
    ];
    ui.columns(2, |columns| {
        for (column, (name, usage, temperature)) in columns.iter_mut().zip(metrics) {
            column.horizontal_centered(|ui| {
                ui.set_height(34.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(name).size(11.0).color(p.subtext));
                let usage = crate::performance::menu_percent(usage);
                ui.label(
                    RichText::new(usage.trim())
                        .font(theme::display_font(15.0))
                        .color(p.text),
                )
                .on_hover_text(usage_hint);
                if show_temperature {
                    let temperature = crate::performance::menu_temperature(temperature);
                    ui.label(
                        RichText::new(temperature.trim())
                            .size(12.0)
                            .color(p.subtext),
                    )
                    .on_hover_text(
                        lang.t("ui.temperature_is_the_hottest_available_core_die_sensor"),
                    );
                }
            });
        }
    });
}

fn settings_action(ui: &mut egui::Ui, p: &Palette, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new((
            egui::Atom::grow(),
            RichText::new(text).size(11.0).color(p.accent),
            egui::Atom::grow(),
        ))
        .min_size(Vec2::new(66.0, 28.0))
        .fill(mix(p.surface, p.accent, 0.07))
        .stroke(Stroke::new(0.7_f32, mix(p.border, p.accent, 0.15)))
        .corner_radius(8),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn settings_note(ui: &mut egui::Ui, p: &Palette, text: &str) -> egui::Response {
    ui.add_space(4.0);
    let response = ui.add(egui::Label::new(RichText::new(text).size(10.5).color(p.subtext)).wrap());
    ui.add_space(4.0);
    response
}

fn settings_separator(ui: &mut egui::Ui, p: &Palette) {
    let (line, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().line_segment(
        [line.left_center(), line.right_center()],
        Stroke::new(0.7_f32, p.border),
    );
}

fn settings_group(
    ui: &mut egui::Ui,
    p: &Palette,
    id: &'static str,
    title: &str,
    body: impl FnOnce(&mut egui::Ui),
) {
    let (label, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), egui::Sense::hover());
    ui.painter().text(
        label.left_center() + egui::vec2(3.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        theme::display_font(12.0),
        p.subtext,
    );
    ui.add_space(2.0);
    let width = ui.available_width();
    egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(0.7_f32, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(width - 25.4);
            ui.spacing_mut().item_spacing.y = 0.0;
            // Keep widget identity and keyboard focus when the title is translated.
            ui.push_id(id, |ui| body(ui));
        });
}

/// The registry supplies the choices; adding a locale never changes this widget.
fn language_row(ui: &mut egui::Ui, p: &Palette, lang: Lang, current: &mut LangMode) -> bool {
    let width = ui.available_width();
    let (row, _) = ui.allocate_exact_size(Vec2::new(width, 40.0), egui::Sense::hover());
    let control_width = 218.0_f32.min(row.width() * 0.65);
    let control = egui::Rect::from_min_size(
        egui::pos2(row.right() - control_width, row.top() + 5.0),
        Vec2::new(control_width, 30.0),
    );
    let label_rect = egui::Rect::from_min_max(
        row.min + Vec2::new(2.0, 0.0),
        egui::pos2(control.left() - 8.0, row.bottom()),
    );
    let label = lang.t("settings.language");
    ellipsis_text(ui.painter(), label_rect, label, 12.0, p.text, Align::Min);
    ui.interact(
        label_rect,
        ui.id().with("language-label"),
        egui::Sense::hover(),
    )
    .on_hover_text(label);
    let system_label = lang.format(
        "settings.system_language",
        &[("language", crate::i18n::system_language().native_name())],
    );
    let selected_text = match *current {
        LangMode::System => system_label.as_str(),
        LangMode::Selected(selected) => selected.native_name(),
    };
    let before = *current;
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("language-selector")
            .max_rect(control)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.spacing_mut().button_padding = Vec2::new(10.0, 7.0);
    child.spacing_mut().interact_size.y = 30.0;
    let visuals = child.visuals_mut();
    visuals.override_text_color = Some(p.text);
    visuals.selection.bg_fill = mix(p.surface, p.accent, 0.16);
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    visuals.window_fill = p.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_corner_radius = CornerRadius::same(10);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(8);
        widget.weak_bg_fill = p.surface;
        widget.bg_stroke = Stroke::new(0.8_f32, p.border);
        widget.fg_stroke = Stroke::new(1.0_f32, p.text);
    }
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.accent);
    // Popup areas begin from the global style, so explicitly carry this palette
    // into the menu rather than inheriting egui's default black/white popup.
    let mut menu_style = child.style().as_ref().clone();
    menu_style.spacing.button_padding = Vec2::new(10.0, 6.0);
    menu_style.spacing.menu_margin = egui::Margin::same(6);
    menu_style.visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 5],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 58 } else { 24 }),
    };
    menu_style.visuals.selection.bg_fill =
        mix(p.surface, p.accent, if p.dark { 0.14 } else { 0.08 });
    menu_style.visuals.selection.stroke = Stroke::new(0.8_f32, p.accent);
    for widget in [
        &mut menu_style.visuals.widgets.inactive,
        &mut menu_style.visuals.widgets.hovered,
        &mut menu_style.visuals.widgets.active,
        &mut menu_style.visuals.widgets.open,
    ] {
        widget.bg_fill = Color32::TRANSPARENT;
        widget.weak_bg_fill = Color32::TRANSPARENT;
        widget.bg_stroke = Stroke::NONE;
        widget.corner_radius = CornerRadius::same(7);
        widget.expansion = 0.0;
    }
    menu_style.visuals.widgets.hovered.weak_bg_fill = mix(p.surface, p.accent, 0.06);
    menu_style.visuals.widgets.active.weak_bg_fill = mix(p.surface, p.accent, 0.10);
    menu_style.visuals.widgets.active.bg_stroke =
        Stroke::new(0.8_f32, mix(p.border, p.accent, 0.5));
    let combo_id = child.make_persistent_id(egui::Id::new("language"));
    let popup_id = combo_id.with("popup");
    let options: Vec<_> = std::iter::once((LangMode::System, system_label.as_str()))
        .chain(
            Lang::all()
                .iter()
                .map(|language| (LangMode::Selected(*language), language.native_name())),
        )
        .collect();
    let focus_id = combo_id.with("focus-selection");
    let open = egui::ComboBox::is_open(child.ctx(), combo_id);
    let escape = open && child.input(|input| input.key_pressed(egui::Key::Escape));
    if !open {
        child.data_mut(|data| data.insert_temp(focus_id, true));
    }
    let response = egui::ComboBox::from_id_salt("language")
        .popup_style(menu_style.into())
        .width(control.width())
        .height(210.0)
        .selected_text(RichText::new(selected_text).size(12.0))
        .truncate()
        .icon(|ui, rect, visuals, open| {
            let center = rect.center();
            let direction = if open { -1.0 } else { 1.0 };
            ui.painter().add(egui::Shape::line(
                vec![
                    center + Vec2::new(-3.5, -1.5 * direction),
                    center + Vec2::new(0.0, 2.0 * direction),
                    center + Vec2::new(3.5, -1.5 * direction),
                ],
                Stroke::new(1.3_f32, visuals.fg_stroke.color),
            ));
        })
        .show_ui(&mut child, |ui| {
            ui.set_width(control.width() - 16.0);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.spacing_mut().item_spacing.y = 3.0;
            let focus_selected = ui
                .data(|data| data.get_temp::<bool>(focus_id))
                .unwrap_or(false);
            ui.data_mut(|data| data.insert_temp(focus_id, false));
            for (value, text) in &options {
                let selected = *current == *value;
                let response = ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::selectable(
                            selected,
                            RichText::new(*text).size(12.0).color(if selected {
                                p.accent
                            } else {
                                p.text
                            }),
                        )
                        .right_text(if selected { "✓" } else { "  " })
                        .truncate(),
                    )
                    .on_hover_text(*text);
                if focus_selected && selected {
                    response.request_focus();
                    response.scroll_to_me(Some(Align::Center));
                }
                if response.clicked() {
                    *current = *value;
                    ui.close();
                    ui.memory_mut(|memory| memory.request_focus(combo_id));
                }
            }
        })
        .response;
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::ComboBox,
            true,
            format!("{label}: {selected_text}"),
        )
    });
    response.clone().on_hover_text(selected_text);
    if response.has_focus()
        && !open
        && child.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
    {
        child.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
        egui::Popup::open_id(child.ctx(), popup_id);
    }
    if escape {
        response.request_focus();
    }
    before != *current
}

/// 设置项：左边标签，右边一组互斥选项。返回是否被改动。
fn option_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    current: &mut T,
    options: &[(T, &str)],
) -> bool {
    const ROW_H: f32 = 38.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_H), egui::Sense::hover());
    let painter = ui.painter();

    // 各组选项共用起点；两档、三档、四档都在同一列内均分。
    let ctl_w = 218.0_f32.min(rect.width() * 0.65);
    let ctl = egui::Rect::from_min_max(
        egui::pos2(rect.right() - ctl_w, rect.top() + 5.0),
        egui::pos2(rect.right(), rect.bottom() - 5.0),
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
    painter.rect_filled(ctl, 8.0, mix(p.surface, p.chip, 0.7));

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
        ui.painter().rect(
            pill.shrink(2.0),
            6.0,
            p.surface,
            Stroke::new(0.7_f32, mix(p.border, p.accent, 0.12)),
            egui::StrokeKind::Inside,
        );
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
                    CornerRadius::same(6),
                    p.accent.gamma_multiply(0.10),
                );
            }
        }

        ellipsis_text(
            ui.painter(),
            r.shrink2(egui::vec2(3.0, 0.0)),
            text,
            10.5,
            if selected { p.accent } else { p.subtext },
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

/// 自定义色即时生效；十六进制输入未完成时保留草稿，合法后才保存。
fn color_option_row(
    ui: &mut egui::Ui,
    p: &Palette,
    lang: Lang,
    label: &str,
    current: &mut Option<tm_core::color::Rgb>,
    automatic: tm_core::color::Rgb,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), egui::Sense::hover());
    let ctl_w = 218.0_f32.min(rect.width() * 0.65);
    let ctl = egui::Rect::from_min_max(
        egui::pos2(rect.right() - ctl_w, rect.top() + 3.0),
        egui::pos2(rect.right(), rect.bottom() - 3.0),
    );
    let label_rect = egui::Rect::from_min_max(
        rect.min + egui::vec2(2.0, 0.0),
        egui::pos2(ctl.left() - 6.0, rect.bottom()),
    );
    ellipsis_text(ui.painter(), label_rect, label, 12.0, p.text, Align::Min);
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("color-row", label))
            .max_rect(ctl)
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.visuals_mut().selection.bg_fill = p.selection;
    row.visuals_mut().selection.stroke = Stroke::new(1.0_f32, p.on_selection);
    crate::color_picker::controls(&mut row, ctl, p, lang, label, current, automatic)
}

/// 设置项：左边标签，右边一个开关。返回是否被点击。
fn toggle_row(ui: &mut egui::Ui, p: &Palette, label: &str, on: &mut bool) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), egui::Sense::hover());
    toggle_row_at(ui, rect, p, label, on)
}

/// 图标、网速、性能可以独立显示，但须保留一个可点击的主面板入口。
fn visibility_toggle_row(
    ui: &mut egui::Ui,
    p: &Palette,
    lang: Lang,
    label: &str,
    on: &mut bool,
    other_visible: bool,
) -> bool {
    let enabled = !*on || other_visible;
    let changed = ui
        .add_enabled_ui(enabled, |ui| toggle_row(ui, p, label, on))
        .inner;
    if !enabled {
        settings_note(ui, p, lang.t("settings.keep_one_visible"));
    }
    changed
}

fn toggle_row_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    p: &Palette,
    label: &str,
    on: &mut bool,
) -> bool {
    // 整行都能点，比只点那个小圆点好按
    let resp = ui.interact(rect, ui.id().with(("toggle", label)), egui::Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });

    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left() + 2.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
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
            Stroke::new(1.4_f32, color),
        );
        // 滑块位置左右错开，看着像个均衡器而不是"汉堡菜单"
        let kx = rect.left() + 4.0 + ((i + 1) % 3) as f32 * 3.5;
        painter.circle_filled(egui::pos2(kx, y), 2.1, p.chip);
        painter.circle_stroke(egui::pos2(kx, y), 2.1, Stroke::new(1.2_f32, color));
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
    ellipsis_text(
        ui.painter(),
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 29.0, rect.top()),
            rect.max - Vec2::new(4.0, 0.0),
        ),
        label,
        12.5,
        color,
        Align::Min,
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    surface::focus(ui.painter(), rect, &resp, p);
    resp
}

fn paint_pin_button(ui: &egui::Ui, response: &egui::Response, pinned: bool, p: &Palette) {
    let rect = response.rect;
    let painter = ui.painter();
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    if pinned || hovered || pressed {
        let tint = if pressed {
            0.21
        } else if hovered {
            0.15
        } else {
            0.09
        };
        painter.rect(
            rect.shrink(2.0),
            CornerRadius::same(6),
            mix(p.surface, p.accent, tint),
            Stroke::new(
                0.7_f32,
                mix(p.border, p.accent, if pinned { 0.28 } else { 0.12 }),
            ),
            egui::StrokeKind::Inside,
        );
    }
    let color = if pinned || hovered || pressed {
        p.accent
    } else {
        mix(p.surface, p.subtext, 0.78)
    };
    let c = rect.center();
    let at = |x, y| c + egui::vec2(x, y);
    let stroke = Stroke::new(1.25_f32, color);

    // 正立、对称的图钉：圆头横帽、收窄的颈部和清楚的针尖。
    // 实心/描边也表达选中状态，不只依赖颜色；点击范围仍是 26pt。
    if pinned {
        painter.rect_filled(
            egui::Rect::from_min_max(at(-2.5, -5.2), at(2.5, -1.0)),
            0.0,
            color,
        );
        painter.add(egui::Shape::convex_polygon(
            vec![at(-2.5, -1.1), at(2.5, -1.1), at(4.4, 2.3), at(-4.4, 2.3)],
            color,
            Stroke::NONE,
        ));
    }
    painter.add(egui::Shape::line(
        vec![
            at(-2.5, -5.2),
            at(-2.5, -1.1),
            at(-4.4, 2.3),
            at(4.4, 2.3),
            at(2.5, -1.1),
            at(2.5, -5.2),
        ],
        stroke,
    ));
    painter.rect_filled(
        egui::Rect::from_center_size(at(0.0, -5.2), Vec2::new(7.6, 1.25)),
        0.6,
        color,
    );
    painter.line_segment([at(0.0, 2.3), at(0.0, 6.0)], stroke);
    painter.circle_filled(at(0.0, 6.0), 0.625, color);
    surface::focus(painter, rect, response, p);
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// 导出：一个开口朝上的托盘，中间是往上走的箭头。
///
/// 不用「下载」那个向下箭头：导出是把数据**交出去**，箭头朝上更贴近语义，
/// 也和老版本「导出」两个字的位置对得上。
fn paint_export_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let stroke = Stroke::new(1.35_f32, color);
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
    let stroke = Stroke::new(1.35_f32, color);
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
    let stroke = Stroke::new(1.6_f32, color);
    painter.add(egui::Shape::line(
        vec![
            egui::pos2(c.x + 3.4, c.y - 5.0),
            egui::pos2(c.x - 3.2, c.y),
            egui::pos2(c.x + 3.4, c.y + 5.0),
        ],
        stroke,
    ));
}

#[cfg(test)]
fn app_list_rates(
    snapshot: Option<&Snapshot>,
    window: Window,
    settings: &Settings,
) -> Vec<AppRate> {
    app_list_rates_with_restrictions(snapshot, window, settings, &[])
}

/// 展示层合并置顶项和受限应用，不改动采样快照和历史统计。
fn app_list_rates_with_restrictions(
    snapshot: Option<&Snapshot>,
    window: Window,
    settings: &Settings,
    restricted: &[&str],
) -> Vec<AppRate> {
    // 加载/采集失败时保留错误提示和受限应用的恢复入口，不展示普通置顶项的假零值。
    let snapshot = snapshot.filter(|s| s.error.is_none());
    let rates = snapshot.map_or(&[][..], |snapshot| match window {
        Window::Live => snapshot.rates.as_slice(),
        Window::Min1 => &snapshot.windowed.min1,
        Window::Min5 => &snapshot.windowed.min5,
        Window::Hour1 => &snapshot.windowed.hour1,
    });
    let pinned = settings
        .pinned_apps
        .iter()
        .filter(|_| snapshot.is_some())
        .map(String::as_str);
    let mut listed: Vec<AppRate> = Vec::new();
    // 必须从完整快照查找，然后应用展示上限；先截断会把受限应用的真实流量丢掉。
    // 置顶顺序始终优先，受限项紧随其后，两者都保留且不重复占用名额。
    for key in pinned.chain(restricted.iter().copied()) {
        if listed
            .iter()
            .any(|rate| crate::settings::same_app(rate.app.as_str(), key))
        {
            continue;
        }
        listed.push(
            rates
                .iter()
                .find(|rate| crate::settings::same_app(rate.app.as_str(), key))
                .cloned()
                .unwrap_or_else(|| AppRate {
                    app: AppKey::new(key),
                    rx_bps: 0.0,
                    tx_bps: 0.0,
                    rx_session: 0,
                    tx_session: 0,
                }),
        );
    }
    for rate in rates {
        if listed.len() >= settings.max_apps.max(1) {
            break;
        }
        if !listed
            .iter()
            .any(|existing| crate::settings::same_app(existing.app.as_str(), rate.app.as_str()))
        {
            listed.push(rate.clone());
        }
    }
    listed
}

fn pin_button_rect(row: egui::Rect) -> egui::Rect {
    #[cfg(windows)]
    let row = egui::Rect::from_min_max(
        row.min,
        row.max - Vec2::new(NETWORK_BUTTON_W + PIN_GAP, 0.0),
    );
    egui::Rect::from_center_size(
        egui::pos2(row.right() - ROW_PAD_X - PIN_SIZE / 2.0, row.center().y),
        Vec2::splat(PIN_SIZE),
    )
}

#[cfg(windows)]
fn network_row_action(ui: &mut egui::Ui, row: egui::Rect, app: &str) -> egui::Response {
    let rect = egui::Rect::from_center_size(
        egui::pos2(
            row.right() - ROW_PAD_X - NETWORK_BUTTON_W / 2.0,
            row.center().y,
        ),
        Vec2::new(NETWORK_BUTTON_W, PIN_SIZE),
    );
    ui.interact(
        rect,
        ui.id().with(("app-network", app)),
        egui::Sense::click(),
    )
}

/// 详情与置顶使用独立点击区域，状态都绑定完整应用键，排序改变后焦点仍跟着应用。
fn app_row_actions(
    ui: &mut egui::Ui,
    row: egui::Rect,
    app: &str,
    settings: &mut Settings,
) -> (egui::Response, egui::Response) {
    let pin_rect = pin_button_rect(row);
    let details_rect =
        egui::Rect::from_min_max(row.min, egui::pos2(pin_rect.left() - PIN_GAP, row.bottom()));
    let details = ui.interact(
        details_rect,
        ui.id().with(("app-details", app)),
        egui::Sense::click(),
    );
    let mut pin = ui.interact(
        pin_rect,
        ui.id().with(("app-pin", app)),
        egui::Sense::click(),
    );
    if pin.clicked() {
        settings.toggle_pin(app);
        pin.mark_changed();
    }
    (details, pin)
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
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), area_h),
        egui::Sense::hover(),
    );
    response.on_hover_text(text);
    let galley = ui.painter().layout(
        text.to_owned(),
        egui::FontId::proportional(12.0),
        p.subtext,
        (rect.width() - 24.0).max(1.0),
    );
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.top() + ((rect.height() - galley.size().y) * 0.42).max(0.0),
    );
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(pos, galley, p.subtext);
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
        Stroke::new(
            0.8_f32,
            mix(p.border, color, if p.dark { 0.20 } else { 0.08 }),
        ),
        egui::StrokeKind::Inside,
    );

    let down = arrow == "↓";
    let circle = egui::pos2(rect.left() + 26.0, rect.top() + 41.0);
    let disc = egui::Rect::from_center_size(circle, Vec2::splat(28.0));
    let disc_top = mix(p.surface, color, if p.dark { 0.82 } else { 0.20 });
    let disc_bottom = mix(p.surface, color, if p.dark { 0.53 } else { 0.40 });
    surface::gradient(painter, disc, 14.0, disc_top, disc_bottom);
    painter.circle_stroke(
        circle,
        14.0,
        Stroke::new(0.6_f32, mix(disc_top, disc_bottom, 0.5)),
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
        egui::pos2(text_left + 3.0, rect.top() + 18.0),
        down,
        3.8,
        color,
    );
    painter.text(
        egui::pos2(text_left + 12.0, rect.top() + 18.0),
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
            let bottom = rect.top() + 58.0;
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
                    egui::pos2(text_left, rect.top() + 37.0),
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

        let item =
            tm_macos::StatusItem::new_named_with_click("dev.guolongkang.tramoing.icon", |anchor| {
                if let Some(tx) = TRAY_TX.get() {
                    let _ = tx.send(TrayEvent::MenuClick {
                        item: MenuItem::Icon,
                        anchor,
                    });
                }
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            })
            .ok_or_else(|| "创建 NSStatusItem 失败（不在主线程？）".to_owned())?;
        item.set_icon(&rgba, w, h);
        item.clear_title();
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

/// 点了任务栏上的网速或性能贴片，两者都打开主面板。
///
/// 和点托盘图标共用同一套开合逻辑（`toggle`），只是**锚点用贴片自己**：
/// 贴片所在的位置和托盘图标可能差好几百像素，用托盘图标当锚点的话，
/// 点最左边的数字、面板却从右边冒出来，看着像点错了东西。
///
/// 贴片窗口拿不到 `&mut App`，所以只往 channel 里塞一条事件，真正的开合
/// 仍然在 `update()` 里按状态决定——两边各记一份「面板开没开」迟早对不上。
#[cfg(windows)]
pub(crate) fn badge_clicked(at: Option<tm_windows::screen::Rect>, _performance: bool) {
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

#[cfg(windows)]
pub(crate) fn badge_reordered(order: tm_core::taskbar::TaskbarOrder) {
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(TrayEvent::BadgeReordered { order });
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
    fn visibility_switch_preserves_the_last_panel_entry() {
        for (initial, other_visible, expected) in [
            (false, false, true),
            (true, false, true),
            (true, true, false),
        ] {
            let ctx = egui::Context::default();
            let mut on = initial;
            for frame in 0..3 {
                let pos = egui::pos2(100.0, 25.0);
                let events = if frame == 0 {
                    Vec::new()
                } else {
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: frame == 1,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]
                };
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(WINDOW_W, PANEL_H),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            visibility_toggle_row(
                                ui,
                                &theme::palette(true),
                                Lang::EN,
                                Lang::EN.t("settings.show_icon"),
                                &mut on,
                                other_visible,
                            );
                        });
                    },
                );
            }
            assert_eq!(on, expected, "initial={initial}, other={other_visible}");
        }
    }

    #[test]
    fn language_dropdown_supports_keyboard_selection_cancel_and_reopen() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            let p = theme::palette(dark);
            let mut mode = LangMode::System;
            let mut combo_id = egui::Id::NULL;
            let key_event = |key, pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            for frame in 0..12 {
                if frame == 1 {
                    ctx.memory_mut(|m| m.request_focus(combo_id));
                }
                let events = match frame {
                    1 | 3 | 5 | 7 | 9 => vec![key_event(egui::Key::Enter, true)],
                    2 | 6 => vec![
                        key_event(egui::Key::Enter, false),
                        key_event(egui::Key::ArrowDown, true),
                    ],
                    4 | 8 => vec![
                        key_event(egui::Key::Enter, false),
                        key_event(egui::Key::ArrowDown, false),
                    ],
                    10 => vec![
                        key_event(egui::Key::Enter, false),
                        key_event(egui::Key::Escape, true),
                    ],
                    11 => vec![
                        key_event(egui::Key::Escape, false),
                        key_event(egui::Key::ArrowDown, true),
                    ],
                    _ => Vec::new(),
                };
                let mut changed = false;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(WINDOW_W, PANEL_H),
                        )),
                        time: Some(frame as f64 / 30.0),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let lang = crate::i18n::resolve(mode);
                            settings_group(ui, &p, "appearance", lang.t("ui.appearance"), |ui| {
                                combo_id = ui
                                    .id()
                                    .with(egui::Id::new("language-selector"))
                                    .with(egui::Id::new("language"));
                                changed = language_row(ui, &p, lang, &mut mode);
                            });
                        });
                    },
                );
                assert_eq!(
                    egui::ComboBox::is_open(&ctx, combo_id),
                    matches!(frame, 1 | 2 | 5 | 6 | 9 | 11),
                    "popup at frame {frame}"
                );
                assert_eq!(
                    changed,
                    matches!(frame, 3 | 7),
                    "selection at frame {frame}"
                );
                assert_eq!(
                    mode,
                    match frame {
                        0..=2 => LangMode::System,
                        3..=6 => LangMode::Selected(Lang::EN),
                        _ => LangMode::Selected(Lang::ZH_CN),
                    },
                    "language at frame {frame}"
                );
                if matches!(frame, 4 | 8 | 10) {
                    assert_eq!(ctx.memory(|m| m.focused()), Some(combo_id));
                }
            }
        }
    }

    #[test]
    fn export_results_use_the_language_at_display_time() {
        let result = Ok(ExportedHistory {
            rows: 7,
            path: "/tmp/{count}.csv".into(),
        });
        assert_eq!(
            export_message(&result, Lang::EN),
            "Exported 7 rows → /tmp/{count}.csv"
        );
        assert_eq!(
            export_message(&result, Lang::ZH_CN),
            "已导出 7 行 → /tmp/{count}.csv"
        );
        assert_eq!(
            export_message(&Err("disk full".into()), Lang::EN),
            "Export failed: disk full"
        );
    }

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

    fn sample_rate(key: &str, bps: f64) -> AppRate {
        AppRate {
            app: AppKey::from(key),
            rx_bps: bps,
            tx_bps: bps / 10.0,
            rx_session: 1024,
            tx_session: 128,
        }
    }

    fn listed_keys(rates: &[AppRate]) -> Vec<&str> {
        rates.iter().map(|rate| rate.app.as_str()).collect()
    }

    #[test]
    fn pinned_apps_keep_their_order_and_actual_rates_when_rankings_change() {
        let mut settings = Settings::default();
        settings.toggle_pin("b");
        settings.toggle_pin("c");
        let mut snapshot = Snapshot {
            rates: vec![
                sample_rate("a", 100.0),
                sample_rate("b", 30.0),
                sample_rate("c", 20.0),
                sample_rate("d", 10.0),
            ],
            ..Default::default()
        };
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        assert_eq!(listed_keys(&listed), ["b", "c", "a", "d"]);
        assert_eq!(listed[0], snapshot.rates[1]);
        assert_eq!(listed[1], snapshot.rates[2]);

        snapshot.rates = vec![
            sample_rate("d", 200.0),
            sample_rate("a", 100.0),
            sample_rate("c", 25.0),
            sample_rate("b", 1.0),
        ];
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        assert_eq!(listed_keys(&listed), ["b", "c", "d", "a"]);
        assert_eq!(listed[0], snapshot.rates[3]);
        assert_eq!(listed[1], snapshot.rates[2]);
    }

    #[test]
    fn restricted_apps_preserve_pin_order_and_rates_beyond_the_display_limit() {
        let settings = Settings {
            max_apps: 1,
            pinned_apps: vec!["pinned".into()],
            ..Default::default()
        };
        let rates = vec![sample_rate("pinned", 100.0), sample_rate("blocked", 42.0)];
        let snapshot = Snapshot {
            rates: rates.clone(),
            windowed: engine::WindowedRates {
                min1: rates.clone(),
                min5: rates.clone(),
                hour1: rates.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        for window in Window::all() {
            let listed = app_list_rates_with_restrictions(
                Some(&snapshot),
                window,
                &settings,
                &["blocked", "pinned"],
            );
            assert_eq!(listed_keys(&listed), ["pinned", "blocked"]);
            assert_eq!(listed[1], rates[1], "受限应用必须保留所选窗口的真实流量");
            assert_eq!(snapshot.rates, rates);
        }
    }

    #[test]
    fn restricted_apps_keep_restore_entries_without_samples() {
        let settings = Settings {
            pinned_apps: vec!["pinned".into()],
            ..Default::default()
        };
        let error = Snapshot {
            error: Some("disconnected".into()),
            ..Default::default()
        };
        for snapshot in [None, Some(&error)] {
            let listed = app_list_rates_with_restrictions(
                snapshot,
                Window::Live,
                &settings,
                &["blocked", "blocked"],
            );
            assert_eq!(listed_keys(&listed), ["blocked"]);
        }
    }

    #[test]
    fn restricted_apps_share_the_display_limit_without_hiding_restore_entries() {
        let mut settings = Settings {
            max_apps: 3,
            pinned_apps: vec!["pinned".into()],
            ..Default::default()
        };
        let snapshot = Snapshot {
            rates: vec![sample_rate("fast", 100.0), sample_rate("slow", 50.0)],
            ..Default::default()
        };
        let listed = app_list_rates_with_restrictions(
            Some(&snapshot),
            Window::Live,
            &settings,
            &["blocked", "blocked"],
        );
        assert_eq!(listed_keys(&listed), ["pinned", "blocked", "fast"]);
        settings.max_apps = 1;
        let listed = app_list_rates_with_restrictions(
            Some(&snapshot),
            Window::Live,
            &settings,
            &["blocked", "blocked"],
        );
        assert_eq!(listed_keys(&listed), ["pinned", "blocked"]);
    }

    #[test]
    fn manually_entered_pin_uses_platform_path_identity() {
        let mut settings = Settings::default();
        settings.toggle_pin(r"c:\apps\APP.exe");
        let snapshot = Snapshot {
            rates: vec![sample_rate(r"C:\Apps\app.exe", 42.0)],
            ..Default::default()
        };
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        if cfg!(windows) {
            assert_eq!(listed, snapshot.rates);
            assert!(settings.is_pinned(listed[0].app.as_str()));
            assert!(!settings.toggle_pin(listed[0].app.as_str()));
            assert!(settings.pinned_apps.is_empty());
        } else {
            assert_eq!(listed.len(), 2, "其他平台保持大小写敏感的应用标识");
        }
    }

    #[test]
    fn pins_take_priority_over_the_limit_and_unpinning_restores_traffic_order() {
        let snapshot = Snapshot {
            rates: (0..8)
                .map(|i| sample_rate(&format!("app{i}"), (8 - i) as f64))
                .collect(),
            ..Default::default()
        };
        let mut settings = Settings {
            max_apps: 5,
            ..Default::default()
        };
        settings.toggle_pin("app7");
        settings.toggle_pin("idle");
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        assert_eq!(
            listed_keys(&listed),
            ["app7", "idle", "app0", "app1", "app2"]
        );
        assert_eq!(listed.len(), 5, "计数包含置顶项，不重复列出活跃的置顶应用");
        assert_eq!(listed[0], snapshot.rates[7]);

        settings.toggle_pin("app7");
        settings.toggle_pin("idle");
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        assert_eq!(
            listed_keys(&listed),
            ["app0", "app1", "app2", "app3", "app4"]
        );

        for i in (1..8).rev() {
            settings.toggle_pin(&format!("app{i}"));
        }
        let listed = app_list_rates(Some(&snapshot), Window::Live, &settings);
        assert_eq!(listed.len(), 7, "减少展示上限不能隐藏任何置顶项");
        assert_eq!(
            listed_keys(&listed),
            ["app7", "app6", "app5", "app4", "app3", "app2", "app1"]
        );
    }

    #[test]
    fn idle_pins_stay_visible_in_every_window_and_use_its_actual_rates() {
        let mut settings = Settings::default();
        settings.toggle_pin("idle");
        settings.toggle_pin("watched");
        let snapshot = Snapshot {
            rates: vec![sample_rate("watched", 11.0)],
            windowed: engine::WindowedRates {
                min1: vec![sample_rate("watched", 22.0)],
                min5: vec![sample_rate("watched", 33.0)],
                hour1: vec![sample_rate("watched", 44.0)],
                ..Default::default()
            },
            ..Default::default()
        };
        let idle = AppRate {
            app: AppKey::from("idle"),
            rx_bps: 0.0,
            tx_bps: 0.0,
            rx_session: 0,
            tx_session: 0,
        };
        for (window, expected) in [
            (Window::Live, &snapshot.rates[0]),
            (Window::Min1, &snapshot.windowed.min1[0]),
            (Window::Min5, &snapshot.windowed.min5[0]),
            (Window::Hour1, &snapshot.windowed.hour1[0]),
        ] {
            let listed = app_list_rates(Some(&snapshot), window, &settings);
            assert_eq!(listed_keys(&listed), ["idle", "watched"]);
            assert_eq!(listed[0], idle);
            assert_eq!(&listed[1], expected);
        }
        let empty = Snapshot::default();
        for window in Window::all() {
            let listed = app_list_rates(Some(&empty), window, &settings);
            assert_eq!(listed_keys(&listed), ["idle", "watched"]);
            assert!(listed.iter().all(|rate| rate.total_bps() == 0.0));
        }
        assert!(empty.rates.is_empty(), "置顶只改变展示，不改动采样快照");
        settings.toggle_pin("idle");
        settings.toggle_pin("watched");
        assert!(app_list_rates(Some(&empty), Window::Live, &settings).is_empty());
    }

    #[test]
    fn loading_and_errors_do_not_show_pins_as_zero_traffic() {
        let mut settings = Settings::default();
        settings.toggle_pin("watched");
        settings.toggle_pin("idle");
        let error = Snapshot {
            rates: vec![sample_rate("watched", 100.0)],
            error: Some("采集暂时中断，正在重新连接".into()),
            ..Default::default()
        };
        for window in Window::all() {
            assert!(app_list_rates(None, window, &settings).is_empty());
            assert!(app_list_rates(Some(&error), window, &settings).is_empty());
        }
        assert_eq!(settings.pinned_apps, ["watched", "idle"]);
    }

    #[test]
    fn pin_actions_accept_pointer_and_keyboard_without_opening_details() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let palette = theme::palette(dark);
            let snapshot = Snapshot {
                rates: vec![sample_rate("fast", 100.0), sample_rate("watched", 1.0)],
                ..Default::default()
            };
            let mut settings = Settings::default();
            let mut pin_at = egui::Pos2::ZERO;
            let mut details_at = egui::Pos2::ZERO;
            let mut pin_id = None;
            for frame in 0..8 {
                let mut input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(WINDOW_W, PANEL_H),
                    )),
                    time: Some(frame as f64 / 60.0),
                    ..Default::default()
                };
                if matches!(frame, 1 | 2 | 6 | 7) {
                    let pos = if frame < 3 { pin_at } else { details_at };
                    input.events.push(egui::Event::PointerMoved(pos));
                    input.events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: matches!(frame, 1 | 6),
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                if frame == 3 || frame == 4 {
                    ctx.memory_mut(|m| m.request_focus(pin_id.unwrap()));
                    input.events.push(egui::Event::Key {
                        key: if frame == 3 {
                            egui::Key::Space
                        } else {
                            egui::Key::Enter
                        },
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
                            let rates = app_list_rates(Some(&snapshot), Window::Live, &settings);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for rate in rates {
                                ui.scope(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width() - SCROLL_GUTTER, ROW_BOX_H),
                                        egui::Sense::hover(),
                                    );
                                    let (details, pin) =
                                        app_row_actions(ui, rect, rate.app.as_str(), &mut settings);
                                    assert!(!details.rect.intersects(pin.rect));
                                    paint_pin_button(
                                        ui,
                                        &pin,
                                        settings.is_pinned(rate.app.as_str()),
                                        &palette,
                                    );
                                    if rate.app.as_str() == "watched" {
                                        pin_at = pin.rect.center();
                                        details_at = details.rect.center();
                                        assert_eq!(*pin_id.get_or_insert(pin.id), pin.id);
                                    }
                                    egui::Popup::from_toggle_button_response(&details).show(|ui| {
                                        ui.label("App details");
                                    });
                                });
                                ui.add_space(ROW_GAP);
                            }
                        });
                });
                if frame < 7 {
                    assert!(!egui::Popup::is_any_open(&ctx), "置顶不能打开详情弹窗");
                } else {
                    assert!(egui::Popup::is_any_open(&ctx), "点击行主体仍可查看详情");
                }
                assert_eq!(settings.is_pinned("watched"), matches!(frame, 2 | 4..=7));
                assert!(!settings.is_pinned("fast"));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn network_button_accepts_click_and_keyboard_without_pin_or_details_actions() {
        let ctx = egui::Context::default();
        let mut settings = Settings::default();
        let mut location = egui::Pos2::ZERO;
        let mut network_id = None;
        for frame in 0..4 {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(WINDOW_W, PANEL_H),
                )),
                time: Some(frame as f64 / 60.0),
                ..Default::default()
            };
            if matches!(frame, 1 | 2) {
                input.events.push(egui::Event::PointerMoved(location));
                input.events.push(egui::Event::PointerButton {
                    pos: location,
                    button: egui::PointerButton::Primary,
                    pressed: frame == 1,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            if frame == 3 {
                ctx.memory_mut(|m| m.request_focus(network_id.unwrap()));
                input.events.push(egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let (row, _) =
                        ui.allocate_exact_size(Vec2::new(360.0, ROW_BOX_H), egui::Sense::hover());
                    let (details, pin) = app_row_actions(ui, row, "test.exe", &mut settings);
                    let network = network_row_action(ui, row, "test.exe");
                    assert!(!network.rect.intersects(pin.rect));
                    assert!(!network.rect.intersects(details.rect));
                    assert!(!details.clicked());
                    assert!(!pin.clicked());
                    assert_eq!(network.clicked(), frame >= 2);
                    location = network.rect.center();
                    network_id = Some(network.id);
                });
            });
            assert!(settings.pinned_apps.is_empty());
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
    fn empty_and_error_states_reserve_their_full_height_at_windows_scales() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            ctx.set_pixels_per_point(scale);
            for message in [
                "当前没有流量",
                "采集暂时中断，正在重新连接",
                "未连接到采集助手，请重新运行安装程序以修复采集助手；连接命名管道失败（Win32 错误 2）",
                "Collector disconnected. Reinstall the collector and retry. Windows error 5: Access denied.",
            ] {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(WINDOW_W, PANEL_H),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                        .show(ctx, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            let bounds = ui.max_rect();
                            ui.add_space(246.0);
                            let expected_alert_top = bounds.bottom() - FOOTER_BAND - ALERT_H;
                            let height = expected_alert_top - ui.cursor().top();
                            empty_note(ui, message, height, &theme::palette(false));
                            assert!((ui.cursor().top() - expected_alert_top).abs() < 1.1);
                            let (alert, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), ALERT_H),
                                egui::Sense::hover(),
                            );
                            assert!(alert.bottom() <= bounds.bottom() - FOOTER_BAND + 1.1);
                            assert!(ui.min_rect().bottom() <= bounds.bottom());
                        });
                });
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
                            segmented_at(ui, bar, &mut window, &palette, Lang::EN);
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
    fn settings_tabs_theme_tiles_and_switches_accept_pointer_and_keyboard() {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let p = theme::palette(dark);
            let mut tab = SettingsTab::default();
            let mut appearance = ThemeMode::System;
            let mut enabled = false;
            let mut tab_at = egui::Pos2::ZERO;
            let mut theme_at = egui::Pos2::ZERO;
            let mut tab_id = egui::Id::NULL;
            let mut theme_id = egui::Id::NULL;
            let mut toggle_id = egui::Id::NULL;
            for frame in 0..10 {
                let mut events = Vec::new();
                if matches!(frame, 1 | 2 | 4 | 5) {
                    let pos = if frame < 3 { tab_at } else { theme_at };
                    events.push(egui::Event::PointerMoved(pos));
                    events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: matches!(frame, 1 | 4),
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                if matches!(frame, 3 | 6 | 8) {
                    ctx.memory_mut(|m| {
                        m.request_focus(match frame {
                            3 => tab_id,
                            6 => theme_id,
                            _ => toggle_id,
                        })
                    });
                }
                if matches!(frame, 3 | 4 | 6..=9) {
                    events.push(egui::Event::Key {
                        key: egui::Key::Space,
                        physical_key: None,
                        pressed: matches!(frame, 3 | 6 | 8),
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let mut appearance_changed = false;
                let mut toggle_changed = false;
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(WINDOW_W, PANEL_H),
                        )),
                        time: Some(frame as f64 / 60.0),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default()
                            .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                            .show(ctx, |ui| {
                                tab_at =
                                    egui::pos2(ui.max_rect().center().x, ui.cursor().top() + 17.0);
                                tab_id = ui.id().with(("settings-tab", SettingsTab::General));
                                settings_tabs(ui, &p, Lang::EN, &mut tab);
                                ui.add_space(12.0);
                                theme_at = egui::pos2(
                                    ui.max_rect().right() - 20.0,
                                    ui.cursor().top() + 31.0,
                                );
                                theme_id = ui.id().with(("theme-preset", 1_usize));
                                appearance_changed =
                                    theme_options(ui, &p, Lang::EN, &mut appearance);
                                toggle_id = ui.id().with(("toggle", "Show CPU / GPU"));
                                toggle_changed = toggle_row(ui, &p, "Show CPU / GPU", &mut enabled);
                            });
                    },
                );
                assert_eq!(
                    tab,
                    match frame {
                        0 | 1 => SettingsTab::StatusBar,
                        2 => SettingsTab::Statistics,
                        _ => SettingsTab::General,
                    }
                );
                assert_eq!(
                    appearance,
                    match frame {
                        0..=4 => ThemeMode::System,
                        5 => ThemeMode::Dark,
                        _ => ThemeMode::Light,
                    }
                );
                assert_eq!(appearance_changed, matches!(frame, 5 | 6));
                assert_eq!(enabled, frame >= 8);
                assert_eq!(toggle_changed, frame == 8);
            }
        }
    }

    #[test]
    fn color_rows_render_in_both_languages_without_changing_automatic_values() {
        use tm_core::color::Rgb;
        for dark in [false, true] {
            for lang in [Lang::ZH_CN, Lang::EN] {
                let ctx = egui::Context::default();
                theme::install_cjk(&ctx);
                let mut automatic = None;
                let mut custom = Some(Rgb(255, 128, 0));
                for _ in 0..3 {
                    let _ = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                Vec2::new(WINDOW_W, PANEL_H),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                settings_group(
                                    ui,
                                    &theme::palette(dark),
                                    "colors",
                                    "Colors",
                                    |ui| {
                                        assert!(!color_option_row(
                                            ui,
                                            &theme::palette(dark),
                                            lang,
                                            lang.t("color.cpu"),
                                            &mut automatic,
                                            tm_core::color::cpu(dark)
                                        ));
                                        assert!(!color_option_row(
                                            ui,
                                            &theme::palette(dark),
                                            lang,
                                            lang.t("color.gpu"),
                                            &mut custom,
                                            tm_core::color::gpu(dark)
                                        ));
                                    },
                                );
                            });
                        },
                    );
                }
                assert_eq!(automatic, None);
                assert_eq!(custom, Some(Rgb(255, 128, 0)));
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
