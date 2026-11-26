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
mod color_picker;
mod device_ip;
mod diagnostics;
mod engine;
mod error_dialog;
mod export;
mod i18n;
mod icon;
mod icons;
mod instance;
#[cfg(windows)]
mod network_access;
#[cfg(windows)]
mod network_panel;
#[cfg(any(target_os = "macos", windows))]
mod performance;
mod settings;
mod startup;
mod surface;
mod theme;
#[cfg(debug_assertions)]
mod ui_capture;
mod update;
#[cfg(windows)]
mod windows_panel;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let reporter = args
        .first()
        .is_some_and(|arg| arg == error_dialog::REPORT_FLAG);
    if let Err(error) = tm_diagnostics::init(
        &tm_store::data_dir(),
        if reporter { "reporter" } else { "ui" },
        update::CURRENT,
    ) {
        let lang = i18n::resolve(settings::Settings::load().lang);
        eprintln!(
            "{}",
            lang.format("cli.logging_failed", &[("error", &error.to_string())])
        );
        tm_diagnostics::report_error("tramoing::logging", &format!("无法初始化诊断日志：{error}"));
    }
    // 报告进程绕过采样、安装、数据迁移和单实例锁，不再安装崩溃启动器。
    if reporter {
        let pending = error_dialog::take_pending_crash();
        let lang = i18n::resolve(settings::Settings::load().lang);
        error_dialog::report_fatal(
            args.get(1)
                .map(String::as_str)
                .or(pending.as_deref())
                .unwrap_or(lang.t("errors.application_failed")),
        );
        return Ok(());
    }
    let interactive = !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--export" | "--export-diagnostics" | "--autostart"
        )
    });
    if interactive {
        error_dialog::install_crash_reporter();
        if let Some(message) = error_dialog::take_pending_crash() {
            tm_diagnostics::report_error("tramoing::panic", &message);
        }
    }
    let result = run(&args);
    match &result {
        Ok(()) => {
            log::info!(target: "tramoing::startup", "正常退出");
            if interactive {
                let errors = tm_diagnostics::take_errors();
                if !errors.is_empty() {
                    error_dialog::report_fatal(&error_dialog::Prompt::new(errors).context());
                }
            }
        }
        Err(error) => {
            let message = format!("程序启动或运行失败：{error}");
            log::error!(target: "tramoing::startup", "{message}");
            if interactive {
                let lang = i18n::resolve(settings::Settings::load().lang);
                error_dialog::report_fatal(
                    &lang.format("errors.startup_failed", &[("error", &error.to_string())]),
                );
            }
        }
    }
    result
}

fn run(args: &[String]) -> eframe::Result<()> {
    // This path works even if migration, the collector, or window creation fails.
    if args.iter().any(|arg| arg == "--export-diagnostics") {
        let lang = i18n::resolve(settings::Settings::load().lang);
        match diagnostics::export_report("命令行诊断；实时状态请参考下方日志") {
            Ok(path) => println!(
                "{}",
                lang.format(
                    "cli.diagnostic_report",
                    &[("path", &path.display().to_string())]
                )
            ),
            Err(error) => {
                log::error!(target: "tramoing::diagnostics", "导出失败：{error}");
                eprintln!(
                    "{}",
                    lang.format("cli.diagnostic_export_failed", &[("error", &error)])
                );
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if let Some(result) = tm_macos::installation::finish_if_requested(args) {
        if let Err(error) = result {
            eprintln!("{error}");
            update::log_line(&error);
            // Before readiness the launching app is still alive and displays the error.
            if args
                .get(1)
                .is_some_and(|path| std::path::Path::new(path).join("ready").is_file())
            {
                error_dialog::report_fatal(&error);
            }
            std::process::exit(1);
        }
        return Ok(());
    }
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
        if let Err(error) = tm_store::prepare_data_dir() {
            log::error!(target: "tramoing::migration", "导入旧数据失败：{error}");
            let lang = i18n::resolve(settings::Settings::load().lang);
            eprintln!(
                "{}",
                lang.format("cli.migration_failed", &[("error", &error.to_string())])
            );
            std::process::exit(1);
        }
        // Resolve after migration, so an existing user's saved language applies.
        let lang = i18n::resolve(settings::Settings::load().lang);
        let result = app::export_history_csv();
        let message = app::export_message(&result, lang);
        if result.is_ok() {
            println!("{message}");
        } else {
            eprintln!("{message}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // 开机自启的真实开关。命令行的好处是不用点界面就能验证，
    // 单测里不能碰 launchctl（见 autostart 的注释）。
    if let Some(pos) = args.iter().position(|a| a == "--autostart") {
        let lang = i18n::resolve(settings::Settings::load().lang);
        let action = args.get(pos + 1).map(String::as_str).unwrap_or("status");
        let r = match action {
            "on" => {
                autostart::set_enabled(true).map(|_| lang.t("cli.autostart_enabled").to_owned())
            }
            "off" => {
                autostart::set_enabled(false).map(|_| lang.t("cli.autostart_disabled").to_owned())
            }
            _ => Ok(lang.format(
                "cli.autostart_status",
                &[(
                    "status",
                    lang.t(if autostart::is_enabled() {
                        "cli.enabled"
                    } else {
                        "cli.disabled"
                    }),
                )],
            )),
        };
        match r {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                log::error!(target: "tramoing::autostart", "登录自启操作失败：{e}");
                eprintln!("{}", lang.format("cli.autostart_failed", &[("error", &e)]));
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if !args.iter().any(|a| a == "--mock") {
        match tm_macos::installation::offer_install() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                update::log_line(&error);
                error_dialog::report_fatal(&error);
                return Ok(());
            }
        }
    }

    if std::env::var_os("TM_FORCE_MOCK").is_none() {
        if let Err(error) = tm_store::prepare_data_dir() {
            update::log_line(&format!("导入旧数据失败：{error}"));
            return Err(eframe::Error::AppCreation(Box::new(error)));
        }
    }

    let record_launch = cfg!(target_os = "macos") && std::env::var_os("TM_FORCE_MOCK").is_none();
    let first_launch =
        record_launch && !startup::has_launched(&tm_store::data_dir(), &engine::db_path());
    let launch = startup::Launch::resolve(
        args,
        std::env::var_os("TM_SHOW_ON_START").is_some(),
        std::env::var_os("TM_AFTER_INSTALL").is_some(),
        first_launch,
    );

    let activate_existing = launch.keep_open
        || (!args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--after-install" | "--after-update"))
            && std::env::var_os("TM_AFTER_INSTALL").is_none());
    let _instance = match instance::Instance::acquire(activate_existing) {
        Ok(Some(instance)) => instance,
        Ok(None) => return Ok(()),
        Err(e) => {
            update::log_line(&format!("无法取得界面实例锁：{e}"));
            return Err(eframe::Error::AppCreation(Box::new(e)));
        }
    };

    if std::env::var_os("TM_FORCE_MOCK").is_none()
        && std::env::var_os("TM_DATA_DIR").is_none()
        && std::env::var_os("TM_DB_PATH").is_none()
    {
        if let Err(error) = autostart::migrate_legacy() {
            log::error!(target: "tramoing::autostart", "迁移登录自启项失败：{error}");
        }
    }

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
        Box::new(move |cc| {
            let app = app::App::new(cc, launch);
            if record_launch {
                if let Err(error) = startup::record_launch(&tm_store::data_dir()) {
                    log::warn!(target: "tramoing::startup", "无法保存首次启动标记：{error}");
                }
            }
            Ok(Box::new(app))
        }),
    )
}
