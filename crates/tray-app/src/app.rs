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
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 状态栏项的平台类型
#[cfg(target_os = "macos")]
type TrayHandle = tm_macos::StatusItem;
#[cfg(not(target_os = "macos"))]
type TrayHandle = tray_icon::TrayIcon;

use crate::autostart;
use crate::engine::{self, Snapshot};
use crate::icon;
use crate::icons::{self, IconSet};
use crate::settings::{LangMode, Settings, ThemeMode, TrayMode};
use crate::theme::{self, Lang, Palette};

pub const WINDOW_W: f32 = 352.0;
/// 首帧的初始高度。只是「还没量过内容」时的占位，实际高度每帧按内容算。
/// 取值贴着默认 8 行的真实高度，免得第一次展开时窗口先高一下再缩回去。
pub const WINDOW_H: f32 = 372.0;

/// 列表行距（行高 18 + 间距 6）
const ROW_H: f32 = 24.0;
/// 行与行之间的空隙
const ROW_GAP: f32 = 6.0;
/// 一行里真正放内容的高度（行高减去行间距）。画布上直接落字时要它。
const ROW_BODY_H: f32 = ROW_H - ROW_GAP;
/// 行内「下行速率」列的固定宽度。数字列必须是定宽的：不定宽时一行显示
/// `0`、另一行显示 `1.2 MB/s`，整列数字会左右跳，扫一眼对不齐。
const NUM_W_RX: f32 = 74.0;
/// 行内「上行速率」列的固定宽度（字小一档，所以窄一些）
const NUM_W_TX: f32 = 60.0;
/// 两个数字列之间的空隙
const NUM_GAP: f32 = 8.0;
/// 右侧给浮动滚动条留的过道，否则滚动的把手会压住上行数字
const SCROLL_GUTTER: f32 = 10.0;
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
    Click,
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
    /// 调试用：启动后立刻把面板摆出来（`--show` / `TM_SHOW_ON_START`）
    show_on_start: bool,
    /// 上一帧内容实际高度。窗口高度跟着内容走，不留空白。
    content_height: f32,
    /// 展开时刻，用于开头那段淡入 + 逐行浮现。
    ///
    /// 为什么不用 `Context::animate_bool_with_time`：那个按归一化进度推进，
    /// 做逐行错峰时最后几行永远到不了 1，会一直半透明。这里直接用真实经过时间算。
    opened_at: Option<Instant>,
    /// 平滑后的窗口高度。窗口高度直接跳变很生硬，用指数逼近过渡。
    anim_height: f32,
    /// 上一次真正下发给系统的窗口高度，避免每帧都发 InnerSize。
    sent_height: f32,
    /// 状态栏图标当前是否挂着。切到「仅网速」时要摘掉，摘/挂都得有状态，
    /// 否则每帧都重建一次 NSImage。
    tray_icon_on: bool,
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

        Self {
            rx,
            tray_rx,
            latest: None,
            visible: false,
            shown_at: Instant::now(),
            auto_hidden_at: None,
            tray: None,
            tray_title: String::new(),
            lang,
            // 调试时可以固定某个窗口，方便截图核对
            window: match std::env::var("TM_WINDOW").unwrap_or_default().as_str() {
                "1m" => Window::Min1,
                "5m" => Window::Min5,
                "1h" => Window::Hour1,
                _ => Window::Live,
            },
            // 等事件循环转两圈再建，见字段注释
            tray_build_at: Instant::now() + Duration::from_millis(600),
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
            show_on_start: std::env::var_os("TM_SHOW_ON_START").is_some(),
            content_height: 0.0,
            startup_hide: 3,
            opened_at: None,
            anim_height: WINDOW_H,
            sent_height: 0.0,
            // build_tray 里已经挂了图标，这里按「已挂」起步
            tray_icon_on: true,
        }
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

    pub fn open(&mut self, ctx: &egui::Context, rect: IconRect) {
        place_below(ctx, rect);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.visible = true;
        self.shown_at = Instant::now();
        self.auto_hidden_at = None;
        self.opened_at = Some(Instant::now());
        // 从略矮一点的高度长出来，配合淡入读起来像弹出而不是「啪一下出现」。
        // 锚点在图标下方，所以窗口是向下生长的，方向和弹出方向一致。
        //
        // content_height 为 0 表示还没有测量过（首次展开）：这时不能拿占位值
        // 去算，否则会从 470 一路缩到真实高度，看起来是「先撑开后收缩」。
        if self.content_height > 0.0 {
            self.anim_height = (self.content_height * 0.88).max(90.0);
        }
    }

    pub fn close(&mut self, ctx: &egui::Context) {
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

    /// 状态栏上的内容（macOS 才有）。
    fn sync_tray_title(&mut self) {
        // Windows 的状态栏项没有文字标题，不做无用功
        if !cfg!(target_os = "macos") {
            return;
        }

        #[cfg(target_os = "macos")]
        {
            let Some(tray) = &self.tray else { return };

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
            let (up, down) = (compact(tx), compact(rx));

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

                // 菜单栏位置窄，只放得下缩写；完整数值丢进悬停提示
                let tip = format!("↓ {}    ↑ {}", format_speed(rx), format_speed(tx));
                tray.set_tooltip(Some(&tip));
            }
        }
    }

    /// 状态栏图标在屏幕上的位置，单位是物理像素（`place_below` 的约定）。
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
        self.ensure_tray();
        self.drain_engine();
        self.sync_tray_title();
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
        if !self.visible && self.startup_hide > 0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.startup_hide -= 1;
        }

        while self.tray_rx.try_recv().is_ok() {
            match self.tray_icon_rect(ctx) {
                Some(rect) => self.toggle(ctx, rect),
                // 状态项还没被布局（刚创建的头几帧）就拿不到位置，这次点击直接忽略
                None => eprintln!("[ui] 状态栏项还没有位置，忽略这次点击"),
            }
        }

        // 失焦自动收起，模拟原生 popover
        if self.visible {
            let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
            if !focused && self.shown_at.elapsed() > HIDE_DEBOUNCE {
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

        let mut content_h = if self.content_height > 0.0 {
            self.content_height
        } else {
            WINDOW_H
        };
        egui::CentralPanel::default()
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                ui.set_width(WINDOW_W - 28.0);
                // 用「最后一个控件的底边 - 内容起始 y」量真实内容高度。
                // 不能用 ui.min_rect().height()：CentralPanel 的 Ui 一开始就拿到
                // 整块可用空间，min_rect 会把空白也算进去，结果窗口永远缩不下来。
                let start_y = ui.min_rect().top();
                let bottom = match self.view {
                    View::Main => self.main_body(ui, &palette, lang),
                    View::Settings => self.settings_body(ui, &palette, lang),
                };
                content_h = bottom - start_y;
            });

        // 窗口高度跟着内容走，但不要瞬移：应用数变化、切换设置页时都平滑过渡。
        let target = (content_h + 28.0).max(120.0);
        let first_measure = self.content_height <= 0.0;
        self.content_height = target;

        if first_measure {
            // 首帧没有可信的旧高度，直接定下来，免得开场从占位高度滑一下
            self.anim_height = target;
        } else {
            let dt = ctx.input(|i| i.stable_dt).clamp(0.001, 0.1);
            // 时间常数 55ms，大约 160ms 收敛到位
            let k = 1.0 - (-dt / 0.055).exp();
            self.anim_height += (target - self.anim_height) * k;
            if (target - self.anim_height).abs() < 0.4 {
                self.anim_height = target;
            } else {
                ctx.request_repaint();
            }
        }

        let want = self.anim_height.ceil();
        if (want - self.sent_height).abs() >= 1.0 {
            self.sent_height = want;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                WINDOW_W, want,
            )));
        }
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

    /// 主面板。返回最后一个控件的底边，供窗口高度自适应。
    ///
    /// 版面从上到下只有四段：标题 / 两个大数字 / 列表（含时间窗口）/ 页脚。
    /// 段与段之间用一条 1px 淡线分开，不再用满宽实线——面板只有 350px 宽，
    /// 线一重就变成「被切成三块」。
    fn main_body(&mut self, ui: &mut egui::Ui, palette: &Palette, lang: Lang) -> f32 {
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
        self.list(ui, p, lang, reveal);
        self.alert_line(ui, p, lang);

        ui.add_space(8.0);
        self.toast_line(ui, p);
        hairline(ui, p);
        ui.add_space(4.0);
        let footer = self.footer(ui, p, lang);
        footer.rect.bottom()
    }

    /// 设置页。返回最后一个控件的底边。
    fn settings_body(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) -> f32 {
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

        // ---- 菜单栏 ----
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
        if option_row(
            ui,
            p,
            // 它现在是「可视行数」：超出的部分可以滚动，不是只显示这么多
            lang.t("可视行数", "Visible rows"),
            &mut self.settings.row_limit,
            &[(5, "5"), (8, "8"), (12, "12")],
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

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(6.0);

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

        // 只有真的有提示时才画这条分隔线，否则设置页底部会多出一道悬空的横线
        if self.toast.is_some() {
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
        }
        self.toast_line(ui, p);

        if changed {
            self.apply_settings(ui.ctx());
        }
        // 下一行的起始 y 就是内容的底边（含一点点行距，不影响外观）
        ui.cursor().top()
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

    fn list(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang, reveal: f32) {
        // 采样器要两个采样点才能差分出速率，第一个可用数据大约在 1–2 秒后。
        // 这段时间画骨架而不是留白：空面板看起来像坏了，骨架看起来像在加载。
        let Some(snap) = &self.latest else {
            self.skeleton(ui, p);
            return;
        };
        if snap.rates.is_empty() {
            ui.label(
                RichText::new(lang.t("当前没有流量", "No traffic right now"))
                    .size(12.0)
                    .color(p.subtext),
            );
            return;
        }

        // 按选中的时间窗口取对应的一份速率。
        // 这里拷一份：下面每行要取图标，需要 `&mut self.icons`，而 `&self.latest`
        // 的不可变借用会一直延续到循环里。最多十几行，拷贝比拆借用便宜得多。
        let rates: Vec<AppRate> = match self.window {
            Window::Live => snap.rates.clone(),
            Window::Min1 => snap.windowed.min1.clone(),
            Window::Min5 => snap.windowed.min5.clone(),
            Window::Hour1 => snap.windowed.hour1.clone(),
        };

        if rates.is_empty() {
            ui.label(
                RichText::new(lang.t("这段时间没有流量", "No traffic in this window"))
                    .size(12.0)
                    .color(p.subtext),
            );
            return;
        }

        // 可视高度只由「行数 × 行高」和设置里的上限决定，**不能**取
        // `ui.available_height()`：窗口高度本身是根据内容量出来的，
        // 两个方向互相依赖会来回震荡（表现为窗口抽动）。
        // 内容高度 = n 行 × 行高 + (n-1) 个间距。注意末尾没有间距，
        // 所以正好 n 行时高度比 n*ROW_H 少一个 gap——用 n*ROW_H 当视口
        // 会让下一行从底边漏出半截（egui 的裁剪会多给 1px 余量）。
        let gap = ROW_GAP;
        let rows = rates.len() as f32;
        let content_h = rows * ROW_H - gap;
        let cap = self.settings.row_limit as f32 * ROW_H - gap;
        let list_h = content_h.min(cap).max(ROW_H - gap);

        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), list_h),
            Layout::top_down(Align::Min),
            |ui| {
                let mut sa = egui::ScrollArea::vertical().id_salt("app-list");
                // 调试用：TM_SCROLL=140 直接把列表滚到指定位置，方便截图核对
                if let Some(v) = std::env::var("TM_SCROLL").ok().and_then(|v| v.parse().ok()) {
                    sa = sa.vertical_scroll_offset(v);
                }
                sa
                    .max_height(list_h)
                    // 关掉自动收缩，否则内容少时窗口会再缩一圈
                    .auto_shrink([false, false])
                    // 只在真的能滚动时出现；但把手不许淡出（见 style 里的
                    // dormant_handle_opacity），否则用户发现不了「下面还有」
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                    )
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // 行距自己控制，不用默认 item_spacing，行高才是确定的 24
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
            },
        );
    }

    /// 数据还没到时的占位骨架。三个灰条缓慢呼吸，表示"正在采集"。
    fn skeleton(&self, ui: &mut egui::Ui, p: &Palette) {
        let t = ui.ctx().input(|i| i.time) as f32;
        // 0.22..0.5 之间呼吸，太亮会喧宾夺主
        let pulse = 0.36 + 0.14 * (t * 2.6).sin();
        let color = p.subtext.gamma_multiply(pulse);
        let width = ui.available_width();

        for i in 0..3usize {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), egui::Sense::hover());
            let painter = ui.painter();
            let bar = |x: f32, w: f32| {
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.center().y - 4.5),
                    Vec2::new(w, 9.0),
                )
            };
            painter.rect_filled(
                egui::Rect::from_min_size(rect.left_top(), Vec2::splat(18.0)),
                CornerRadius::same(5),
                color,
            );
            // 名字长度错开一点，三行看起来才不像复制粘贴
            let name_w = width * (0.40 - i as f32 * 0.07);
            painter.rect_filled(bar(rect.left() + 26.0, name_w), CornerRadius::same(3), color);
            // 两根灰条的位置和真实行里的两个速率列对齐，
            // 骨架换成本体时不会有东西横着挪一下。
            let tx_x = rect.right() - SCROLL_GUTTER - NUM_W_TX;
            let rx_x = tx_x - NUM_GAP - NUM_W_RX;
            painter.rect_filled(bar(rx_x, NUM_W_RX - 12.0), CornerRadius::same(3), color);
            painter.rect_filled(bar(tx_x, NUM_W_TX - 14.0), CornerRadius::same(3), color);
            if i < 2 {
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
    fn row(&mut self, ui: &mut egui::Ui, r: &AppRate, p: &Palette) {
        let name = pretty_name(r.app.as_str());
        let tex = self.icons.get(r.app.as_str());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            app_avatar(ui, tex, &name);

            // 名字吃掉数字列之外的所有宽度。
            //
            // 这里必须自己 allocate + painter 落字，不能用 `allocate_ui_with_layout`：
            // 那个会按子 ui 的**实际内容**推进游标（而不是期望宽度），短名字后面的
            // 数字就会往左跑，几行下来整列参差不齐。
            let name_w = (ui.available_width()
                - (NUM_W_RX + NUM_GAP + NUM_W_TX + SCROLL_GUTTER))
                .max(48.0);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(name_w, ROW_BODY_H), egui::Sense::hover());
            ui.painter().text(
                egui::pos2(rect.left(), rect.center().y),
                egui::Align2::LEFT_CENTER,
                fit_name(&name, NAME_UNITS),
                egui::FontId::proportional(12.5),
                p.text,
            );

            num_cell(ui, format_speed(r.rx_bps), NUM_W_RX, 12.0, p.accent);
            ui.add_space(NUM_GAP);
            num_cell(ui, format_speed(r.tx_bps), NUM_W_TX, 10.5, p.subtext);
            ui.add_space(SCROLL_GUTTER);
        });
    }

    /// 近 10 分钟单应用超过阈值时给一行提醒。阈值没配就什么都不画。
    ///
    /// 用「窗口平均速率 × 600 秒」算字节数，而不是另存一份计数器：
    /// 时间窗口本来就是这么用的，省掉一套状态。
    fn alert_line(&self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let threshold = self.settings.alert_mb;
        if threshold == 0 {
            return;
        }
        let Some(snap) = &self.latest else { return };
        let limit = threshold.saturating_mul(1024 * 1024);

        let worst = snap
            .windowed
            .min10
            .iter()
            .map(|r| (r.app.as_str(), (r.total_bps() * 600.0) as u64))
            .filter(|(_, bytes)| *bytes >= limit)
            .max_by_key(|(_, bytes)| *bytes);

        let Some((key, bytes)) = worst else { return };
        let name = fit_name(&pretty_name(key), 16);
        let text = match lang {
            Lang::Zh => format!("⚠ 近 10 分钟 {name} 已用 {}", format_bytes(bytes)),
            Lang::En => format!("⚠ {name} used {} in 10 min", format_bytes(bytes)),
        };
        ui.add_space(6.0);
        ui.label(RichText::new(text).size(10.5).color(p.warn));
    }

    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) -> egui::Response {
        let zh = matches!(lang, Lang::Zh);
        let resp = ui.horizontal(|ui| {
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
        });
        resp.response
    }

    /// 底部临时提示。过期后自动消失，面板高度跟着缩回去。
    fn toast_line(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL)
        {
            self.toast = None;
        }
        if let Some((msg, _)) = &self.toast {
            ui.label(
                RichText::new(fit_name(msg, 54))
                    .size(10.0)
                    .color(p.subtext),
            );
            ui.add_space(4.0);
        }
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
        let name = format!("TrafficMonitor-{stamp}.csv");

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
    const ROW_H: f32 = 28.0;
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
    const ROW_H: f32 = 28.0;
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

fn app_avatar(ui: &mut egui::Ui, tex: Option<egui::TextureHandle>, name: &str) {
    match tex {
        Some(t) => {
            ui.add(egui::Image::new(&t).fit_to_exact_size(Vec2::splat(icons::ICON_PT)));
            // 真实图标是方的，但和文字基线之间留一点呼吸感
            ui.add_space(3.0);
        }
        None => {
            // 拿不到图标（系统守护进程、Windows 尚未支持）就画首字母色块，
            // 比一个通用占位图标更有辨识度，也避免行首出现一片空白。
            let (rect, _) =
                ui.allocate_exact_size(Vec2::splat(icons::ICON_PT), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(5), icons::tile_color(name));
            let initial = name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial,
                egui::FontId::proportional(10.0),
                Color32::WHITE,
            );
            ui.add_space(3.0);
        }
    }
}

/// 按显示宽度截断名字：中日韩字符算 2 个宽度单位，其余算 1。
///
/// 列表宽度固定，名字过长会把右侧两个速率挤出面板，所以从源头掐掉。
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

/// 一行里的速率格子：定宽、右对齐、等宽字体。
///
/// 用 painter 直接落字而不是 `Label`：`Label` 的宽度随内容变，一列数字就
/// 对不齐了。这里宽度写死，右边缘固定，小数点自然成列。
fn num_cell(ui: &mut egui::Ui, text: String, w: f32, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, ROW_BODY_H), egui::Sense::hover());
    ui.painter().text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        text,
        egui::FontId::monospace(size),
        color,
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

/// 菜单栏用的紧凑写法：`0`、`820K`、`1.2M`。
fn compact(bps: f64) -> String {
    const K: f64 = 1024.0;
    if bps < 1.0 {
        "0".to_owned()
    } else if bps < K {
        format!("{bps:.0}B")
    } else if bps < K * K {
        format!("{:.0}K", bps / K)
    } else if bps < K * K * K {
        format!("{:.1}M", bps / (K * K))
    } else {
        format!("{:.1}G", bps / (K * K * K))
    }
}

// ---------- 托盘 ----------

fn build_tray() -> Result<TrayHandle, String> {
    let (rgba, w, h) = icon::waveform();

    #[cfg(target_os = "macos")]
    {
        let item = tm_macos::StatusItem::new(|| {
            if let Some(tx) = TRAY_TX.get() {
                let _ = tx.send(TrayEvent::Click);
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
        let icon = Icon::from_rgba(rgba, w, h).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Traffic Monitor")
            .with_icon(icon)
            .with_icon_as_template(true)
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
            ..
        } = event
        {
            if let Some(tx) = TRAY_TX.get() {
                let _ = tx.send(TrayEvent::Click);
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
        }
    }));
}

/// 把窗口摆到状态栏图标正下方，并夹在屏幕内。
fn place_below(ctx: &egui::Context, rect: IconRect) {
    let ppp = ctx.pixels_per_point().max(1.0);
    let center_pt = (rect.x + rect.w / 2.0) / ppp;
    let mut x = center_pt - WINDOW_W / 2.0;
    let y = (rect.y + rect.h + 6.0) / ppp;

    if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
        let max_x = (monitor.x - WINDOW_W - 8.0).max(8.0);
        x = x.clamp(8.0, max_x);
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_formats_for_menu_bar() {
        assert_eq!(compact(0.0), "0");
        assert_eq!(compact(512.0), "512B");
        assert_eq!(compact(1024.0 * 820.0), "820K");
        assert_eq!(compact(1024.0 * 1024.0 * 1.25), "1.2M");
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
}
