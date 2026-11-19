//! 托盘流量监控（macOS / Windows）。
//!
//! 运行时形态：状态栏一个图标 + 一串实时速率数字，点一下弹出面板。
//! 窗口本身就是面板 —— 没有主窗口，隐藏时不占任何界面。
//!
//! 用法：
//!   cargo run -p tray-app              # 真实采样
//!   cargo run -p tray-app -- --mock    # 假数据，用于没有采样权限时验证界面

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
// Windows 任务栏的网速贴片。macOS 的状态栏能直接写文字，用不到这个窗口。
#[cfg(windows)]
mod badge_window;
mod engine;
mod export;
mod icon;
mod icons;
mod instance;
mod settings;
mod theme;
mod update;

use eframe::egui;

/// `TM_LOG=1` 时把 eframe/winit 的内部日志接到 stderr。
///
/// 平时不能接：release 是 GUI 子系统，这些日志没人看还多一层开销。
/// 排查「窗口不对/画不出来」这类问题时它是第一现场——eframe 把
/// `swap_buffers failed`、`was not created on main thread` 这类关键信息
/// 全打在 log 里，没装 logger 就等于没发生过。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn main() -> eframe::Result<()> {
    if std::env::var_os("TM_LOG").is_some() {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Info);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--mock") {
        std::env::set_var("TM_FORCE_MOCK", "1");
    }
    if args.iter().any(|a| a == "--show") {
        std::env::set_var("TM_SHOW_ON_START", "1");
    }
    // 直接打开设置页，省得每次截图都要手动点一下
    if args.iter().any(|a| a == "--settings") {
        std::env::set_var("TM_SHOW_ON_START", "1");
        std::env::set_var("TM_VIEW", "settings");
    }
    // 立刻查一次更新并弹出面板（跳过启动延迟和 24 小时门槛）。
    // 结果画在页脚；release 是 GUI 子系统，println 没有控制台可去。
    if args.iter().any(|a| a == "--check-update") {
        std::env::set_var("TM_FORCE_UPDATE", "1");
        std::env::set_var("TM_SHOW_ON_START", "1");
    }
    // 无界面导出，方便脚本或定时任务调用：tramoing --export
    if args.iter().any(|a| a == "--export") {
        println!("{}", app::export_history_csv(true));
        return Ok(());
    }
    // 开机自启的真实开关。命令行的好处是不用点界面就能验证，
    // 单测里不能碰 launchctl（见 autostart 的注释）。
    if let Some(pos) = args.iter().position(|a| a == "--autostart") {
        let action = args.get(pos + 1).map(String::as_str).unwrap_or("status");
        let r = match action {
            "on" => autostart::set_enabled(true).map(|_| "已开启开机自启".to_owned()),
            "off" => autostart::set_enabled(false).map(|_| "已关闭开机自启".to_owned()),
            _ => Ok(format!(
                "当前状态：{}",
                if autostart::is_enabled() {
                    "已开启"
                } else {
                    "已关闭"
                }
            )),
        };
        match r {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let _instance = match instance::Instance::acquire() {
        Ok(Some(instance)) => instance,
        Ok(None) => return Ok(()),
        Err(e) => {
            update::log_line(&format!("无法取得界面实例锁：{e}"));
            return Err(eframe::Error::AppCreation(Box::new(e)));
        }
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("Tramoing")
        // 高度固定：面板不再跟着内容伸缩，列表装不下就滚动
        .with_inner_size([app::WINDOW_W, app::PANEL_H])
        .with_min_inner_size([app::WINDOW_W, app::PANEL_H])
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_taskbar(false)
        // active(false)：不抢焦点，否则点完图标切到别的应用时
        // 这个窗口还占着 key window，交互会发涩。
        .with_active(false);

    // 「启动先收起来」两个平台的实现不一样：
    // - macOS：直接隐藏窗口，原生状态项点击不依赖 eframe 的重绘循环；
    // - Windows：**不能隐藏**。`WS_VISIBLE` 一去掉，winit 就停了
    //   `RedrawRequested`，eframe 的 `update()` 跟着停摆 —— 托盘点击虽然进了
    //   channel，但没人消费，表现就是「点图标没反应」。所以改成一开始就
    //   摆在屏幕外（保持可见），点图标时再搬回来。
    #[cfg(windows)]
    let viewport = viewport
        .with_position([-32000.0, -32000.0])
        .with_visible(true);
    #[cfg(not(windows))]
    let viewport = viewport.with_visible(false);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "tramoing",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
