//! Windows 任务栏的网速贴片。
//!
//! ## 为什么数字不画进托盘图标
//!
//! `Shell_NotifyIcon` 只接受**方形小图标**：150% DPI 下就是 24×24。把
//! `↓1.2M` 这种宽内容塞进去，系统会把它等比缩成一小团糊——数字还没一笔
//! 汉字笔画粗，根本读不出来。撑开图标也没用：图标槽的 rect 是壳定死的，
//! 跟 HICON 尺寸无关。
//!
//! 所以数字单独开一个窗口，落在**任务栏条内、托盘区左侧**，再把背景抠成
//! 透明（见 `tm_windows::badge::tune` 的颜色键），看起来就是任务栏自己的
//! 一部分。宽度不受图标槽限制，想画多宽画多宽；托盘图标本身只留那条波形。
//!
//! ## 为什么是 deferred 而不是 immediate
//!
//! 一开始用的是 `show_viewport_immediate`（父窗口这一帧里顺手渲染），结果
//! 位置、样式、Z 序全对，就是**一个像素都不显示**——连不透明的洋红测试块
//! 都看不见。immediate viewport 的 `swap_buffers` 走的是父窗口那一帧的 GL
//! 上下文切换，画完屏幕上什么都没有。换 deferred viewport 之后走的是和主
//! 窗口完全相同的渲染通道，正常显示。
//!
//! 代价：回调必须是 `'static + Send + Sync`，拿不到 `&mut self`，所以数据
//! 走 `Arc<Mutex<BadgeData>>`，父窗口每帧写、子窗口每次重绘读。
//!
//! ## 另外两个坑
//!
//! 1. **落位和尺寸走 `ViewportBuilder`**，别在外面 `send_viewport_cmd_to`：
//!    窗口是第一帧渲染时才建的，从外面发的命令直接丢掉。
//! 2. **缩放系数按实际拿到的窗口尺寸反算**，不假设「1 逻辑点 = 1 物理像素」，
//!    也不假设它等于主窗口的 ppp。
//!
//! ## 点击
//!
//! 贴片是**能点的**：点它和点托盘图标完全等价（面板没开就打开、开着就收起）。
//!
//! 但不能指望窗口自己收鼠标消息：颜色键分层窗口的命中测试是按像素走的，
//! 抠掉的地方在系统眼里不属于这个窗口，只有笔画本身能点（`WM_NCHITTEST`
//! 返回 `HTCLIENT` 也拦不住，那一步在窗口过程之前）。所以点击是
//! `tm_windows::badge` 里的轮询线程认的：按下和抬起都落在贴片矩形里就算
//! 一次点击，回调走 [`crate::app::badge_clicked`]，进的是托盘图标那个
//! channel —— 两边共用一套开合状态，不会各记一份。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{Color32, Sense, Stroke, Vec2};

use crate::app::{menu_speed, IconRect};

/// 设计尺寸（逻辑点）。绘制前整体乘缩放系数 `s`。
///
/// 高度只有两行文字：贴片是嵌在任务栏条里的，任务栏在 150% 下也就 72px 高，
/// 再高就顶到边了。宽度 = 左右内边距 + 箭头 + 间隙 + 四位数，按 10.5pt
/// 等宽字（一个字符 0.55em）量出来的，间隙 4pt 出头。
const W: f32 = 44.0;
const H: f32 = 32.0;
/// 一行的高度
const LINE_H: f32 = 14.0;
/// 左右内边距
const PAD: f32 = 5.0;
/// 速率数字字号
const NUM_SIZE: f32 = 10.5;
/// 方向箭头的宽、高。
///
/// 高度跟数字的**字高**（10.5pt 等宽字约 12px @150%）对齐，不要比它更高：
/// 箭头一旦超过字高，视觉重心就被它抢走，数字显得又小又灰（11pt 试过，
/// 一眼看过去全是两个箭头）。
const ARROW_W: f32 = 6.2;
const ARROW_H: f32 = 8.6;
/// 贴片与托盘区之间的空隙
const GAP: f32 = 5.0;
/// 贴片离工作区边缘至少留多少
const EDGE: f32 = 4.0;

/// 贴片窗口的标题。除了调试，也用来按标题找回 HWND 补窗口样式
/// （见 `tm_windows::badge`）。
const TITLE: &str = "Tramoing Speed";

/// 父窗口写、子窗口读的一帧内容。
#[derive(Clone, Copy, Default)]
struct BadgeData {
    /// 目标落位（物理像素）。子 viewport 建窗时用的位置换算和尺寸不是同一套
    /// 基准（尺寸乘了缩放比、位置没有），所以每次重绘都要自己对一次。
    pos: Option<(f32, f32)>,
    /// 落位的变更序号：一变就让子窗口重发位置命令（见 [`draw`]）
    epoch: u32,
    rx: f64,
    tx: f64,
    /// 任务栏是不是浅色。贴片的底色和文字要跟着它走，
    /// 否则深色任务栏上会浮出一块白石膏。
    light: bool,
    /// 上下行用不同颜色
    colored: bool,
}

/// 这一帧要显示的内容。
pub struct BadgeState {
    /// 托盘图标矩形，**物理像素**（跟 `App::tray_icon_rect` 同一套单位）
    pub icon: IconRect,
    /// 下行速率（字节/秒）
    pub rx: f64,
    /// 上行速率（字节/秒）
    pub tx: f64,
    pub light: bool,
    pub colored: bool,
}

pub struct Badge {
    id: egui::ViewportId,
    data: Arc<Mutex<BadgeData>>,
}

impl Badge {
    pub fn new() -> Self {
        // 点贴片 = 点托盘图标。真正的开合在 `App::update` 里按状态决定，
        // 这里只是把事件塞进它已经在监听的那个 channel。
        tm_windows::badge::on_click(crate::app::badge_clicked);
        // 颜色键抠掉的像素收不到鼠标消息（见模块注释「点击」），
        // 所以自己起一个线程看光标 + 左键状态。
        tm_windows::badge::start_click_poller();
        Self {
            id: egui::ViewportId::from_hash_of("tramoing-speed-badge"),
            data: Arc::new(Mutex::new(BadgeData::default())),
        }
    }

    /// 每帧调用一次。`state` 为 `None` 表示这一刻不该显示（面板展开、
    /// 图标被折叠进溢出区、用户关了贴片……），窗口只是隐藏，不销毁——
    /// 反复建窗会闪，宽高也要重新算一遍。
    pub fn show(&mut self, ctx: &egui::Context, state: Option<BadgeState>) {
        let ppp = ctx.pixels_per_point().max(1.0);
        let target = state.as_ref().and_then(|st| place(st.icon, ppp));

        if let Ok(mut data) = self.data.lock() {
            // 落位取整后再比：亚像素抖动不该触发重发位置命令
            let new_pos = target.map(|(x, y)| (x.round(), y.round()));
            if new_pos != data.pos.map(|(x, y)| (x.round(), y.round())) {
                data.epoch = data.epoch.wrapping_add(1);
            }
            data.pos = new_pos;
            if let Some(st) = &state {
                data.rx = st.rx;
                data.tx = st.tx;
                data.light = st.light;
                data.colored = st.colored;
            }
        }

        let mut builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([W, H])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            // 不上任务栏、不抢焦点。
            //
            // **不要**用 `.with_mouse_passthrough(true)` 做穿透：egui 会转成
            // winit 的 `set_cursor_hittest(false)`，而 winit 在 Windows 上给它
            // 加的是 `WS_EX_TRANSPARENT | WS_EX_LAYERED`。一个没设分层属性的
            // 分层窗口根本不会被合成——贴片会彻底隐形（不透明色块也一样看不
            // 见，排查了半天）。穿透改在 `tm_windows::badge::as_overlay` 里
            // 只补 `WS_EX_TRANSPARENT`。
            .with_taskbar(false)
            .with_active(false)
            .with_always_on_top()
            .with_visible(target.is_some());
        if let Some((x, y)) = target {
            builder = builder.with_position([x / ppp, y / ppp]);
        }

        let data = self.data.clone();
        ctx.show_viewport_deferred(self.id, builder, move |vctx, _class| {
            draw(vctx, &data);
        });

        if target.is_some() {
            // 数据每秒变一次。deferred viewport 不会跟着父窗口重绘，
            // 得显式点名让子窗口也画一帧。
            ctx.request_repaint_of(self.id);
        }

        // 每帧都补一次窗口样式，**不能只补一次**：egui 每次给这个窗口下发
        // `Visible` / `WindowLevel` 之类的命令，winit 都会按它自己记的
        // `WindowFlags` 重算一遍 `WS_EX_*`，把这里加的位覆盖掉（收一次面板
        // 再打开，贴片就变回会抢焦点、会上 Alt+Tab 的普通窗口）。
        // 内部先读一次现有样式再比，已经对了就什么都不做，开销只是一次
        // FindWindow + GetWindowLong。
        //
        // `tune` 顺手把窗口句柄交给点击轮询线程；「该不该响应点击」由它自己
        // 看窗口可见性判断，不依赖这里传入的状态（主循环空闲只有 1Hz，状态
        // 传过去会慢一拍）。
        tm_windows::badge::tune(TITLE);
    }
}

/// 算贴片左上角，单位**物理像素**。
///
/// 贴片是**嵌在任务栏条里**的，不是浮在任务栏上方：纵向按任务栏高度居中，
/// 横向贴在托盘区（`TrayNotifyWnd`）左边——那是任务栏里唯一稳定空着的地方。
/// 托盘图标本身是壳按顺序排的，插不进自己的图标旁边（左边紧挨着的可能
/// 是「显示隐藏图标」的箭头）。
///
/// 量不到任务栏（explorer 重启中）就退回「贴着托盘图标左侧、和图标同高居
/// 中」，至少不会飞到屏幕中间去。
fn place(icon: IconRect, ppp: f32) -> Option<(f32, f32)> {
    if icon.w < 1.0 || icon.h < 1.0 {
        return None;
    }
    let (w, h) = (W * ppp, H * ppp);
    let (gap, edge) = (GAP * ppp, EDGE * ppp);

    let tb = tm_windows::badge::taskbar();
    let band = tb.as_ref().map(|t| t.band);

    // 横向：从托盘区左缘往左找位置。任务栏上如果已经插着别的程序
    // （TrafficMonitor 这类），让到它左边去——两个网速贴片叠在一起
    // 只会互相盖字，谁都看不清。
    let mut right = tb
        .as_ref()
        .map(|t| t.tray.x as f32 - gap)
        .unwrap_or(icon.x - gap);
    if let Some(t) = &tb {
        for _ in 0..8 {
            let mut moved = false;
            for p in &t.plugins {
                let (pl, pr) = (p.x as f32, (p.x + p.w) as f32);
                if pr > right - w && pl < right {
                    right = pl - gap;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
    let x = right - w;

    // 纵向：任务栏条内居中；拿不到任务栏就对齐图标中心
    let y = match &band {
        Some(b) => b.y as f32 + (b.h as f32 - h) / 2.0,
        None => icon.y + icon.h / 2.0 - h / 2.0,
    };

    // 夹进任务栏（拿不到就夹进图标附近，防止离谱的位置）
    let (lo_x, hi_x, lo_y, hi_y) = match &band {
        Some(b) => (
            b.x as f32 + edge,
            (b.x + b.w) as f32 - w - edge,
            b.y as f32 + edge / 2.0,
            (b.y + b.h) as f32 - h - edge / 2.0,
        ),
        None => (x, x, y, y),
    };
    let x = x.clamp(lo_x, hi_x.max(lo_x));
    let y = y.clamp(lo_y, hi_y.max(lo_y));
    Some((x, y))
}

fn draw(ctx: &egui::Context, data: &Arc<Mutex<BadgeData>>) {
    let Ok(data) = data.lock() else { return };
    let data = *data;

    let screen = ctx.content_rect();
    let ppp = ctx.pixels_per_point().max(1.0);

    // 位置自校正。
    //
    // 子 viewport 建窗时，尺寸按缩放比换算了（54pt → 81px），位置**没有**
    // （传 2245，窗口就真落在 2245 物理像素上，本该是 3367）。builder 里的
    // 值不变 egui 就不会再下发位置命令，`outer_rect` 又只是回放它以为的位置
    // （同样是错的），所以只能自己补：落位一变就重发几条，连画三帧足够。
    // 这条命令走的是本 viewport 的上下文，ppp 是准的，乘回去正好落对。
    if let Some((tx, ty)) = data.pos {
        static PUSHED: Mutex<(u32, u8)> = Mutex::new((0, 0));
        if let Ok(mut pushed) = PUSHED.lock() {
            if pushed.0 != data.epoch {
                *pushed = (data.epoch, 0);
            }
            if pushed.1 < 3 {
                pushed.1 += 1;
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    tx / ppp,
                    ty / ppp,
                )));
                // 搬过去之后立刻再画一帧，别让错位的那一帧停在屏幕上
                ctx.request_repaint();
            }
        }
    }

    // 缩放系数由实际尺寸反算（见模块注释第 2 条）
    let s = if screen.width() > 1.0 {
        (screen.width() / W).clamp(0.5, 4.0)
    } else {
        1.0
    };

    if std::env::var_os("TM_TRACE_BADGE").is_some() && trace_once() {
        eprintln!(
            "[badge] screen={screen:?} ppp={ppp:.2} inner={:?} outer={:?} s={s:.2}",
            ctx.input(|i| i.viewport().inner_rect),
            ctx.input(|i| i.viewport().outer_rect),
        );
    }

    // 贴片里不画底板：它就是任务栏的一部分，任务栏自己已经有底色
    // （Win11 还是亚克力）。再叠一块半透明圆角反而像浮在上面的小部件，
    // 和右边的时钟、左边的系统图标都不搭。
    let fg = if data.light {
        Color32::from_rgb(24, 24, 28)
    } else {
        Color32::from_rgb(238, 238, 242)
    };
    // 彩色档跟面板里的强调色是同一套：下行蓝、上行绿
    let down = if data.colored {
        if data.light {
            Color32::from_rgb(20, 110, 200)
        } else {
            Color32::from_rgb(110, 184, 255)
        }
    } else {
        fg
    };
    let up = if data.colored {
        if data.light {
            Color32::from_rgb(40, 130, 80)
        } else {
            Color32::from_rgb(134, 214, 164)
        }
    } else {
        fg
    };

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Color32::TRANSPARENT))
        .show(ctx, |ui| {
            // 先占位再画：裸 painter 画到 widget 之外会被裁剪掉
            let (rect, _) = ui.allocate_exact_size(Vec2::new(W * s, H * s), Sense::hover());
            let p = ui.painter();

            let top = rect.center().y - LINE_H * s;
            for (i, (is_down, bps, color)) in [(true, data.rx, down), (false, data.tx, up)]
                .into_iter()
                .enumerate()
            {
                // 箭头整体比文字基线略偏下 0.5pt：文字的量出来中心是
                // galley 的中心，含了下沉部，箭头不补这半格会显得往下坠
                let cy = top + LINE_H * s * (i as f32 + 0.5) - 0.5 * s;

                arrow(
                    p,
                    rect.left() + PAD * s + ARROW_W * s / 2.0,
                    cy,
                    ARROW_W * s,
                    ARROW_H * s,
                    is_down,
                    color,
                );

                // 数值右对齐：`menu_speed` 的输出恒为 4 个字符，所以两行的
                // 右边缘是同一条竖线，数字不会随单位左右跳。
                let galley = p.layout_no_wrap(
                    menu_speed(bps),
                    egui::FontId::monospace(NUM_SIZE * s),
                    color,
                );
                p.galley(
                    egui::pos2(
                        rect.right() - PAD * s - galley.size().x,
                        cy - galley.size().y / 2.0,
                    ),
                    galley,
                    color,
                );
            }
        });
}

/// `TM_TRACE_BADGE=1` 时前几帧各打一行，够核对尺寸和缩放就够了。
fn trace_once() -> bool {
    static N: AtomicU8 = AtomicU8::new(0);
    N.fetch_add(1, Ordering::Relaxed) < 4
}

/// 一个细箭头：竖杆 + 三角头。朝下是下行，朝上是上行。
///
/// 不用 `↓`/`↑` 字符：字体缺字形就是个方块，手绘的形状永远画得出来。
/// 也不能用一个多边形拼出箭头——箭头是**非凸**的，`convex_polygon` 会把
/// 三角头底部那两个凹角填平，看起来像团三角。拆成「矩形竖杆 + 三角头」两个
/// 同色形状，竖杆往头里多探一点，避免交界处留出发丝缝。
fn arrow(p: &egui::Painter, cx: f32, cy: f32, w: f32, h: f32, down: bool, color: Color32) {
    let stem = (w * 0.24).max(1.1);
    let head_h = h * 0.44;
    let half = w / 2.0;
    let (top, bottom) = (cy - h / 2.0, cy + h / 2.0);
    let eat = (h * 0.08).max(0.4);

    let (stem_top, stem_bottom) = if down {
        (top, bottom - head_h + eat)
    } else {
        (top + head_h - eat, bottom)
    };
    p.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(cx - stem / 2.0, stem_top),
            egui::pos2(cx + stem / 2.0, stem_bottom),
        ),
        0.0,
        color,
    );

    let (base_y, tip_y) = if down {
        (bottom - head_h, bottom)
    } else {
        (top + head_h, top)
    };
    p.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(cx - half, base_y),
            egui::pos2(cx + half, base_y),
            egui::pos2(cx, tip_y),
        ],
        color,
        Stroke::NONE,
    ));
}
