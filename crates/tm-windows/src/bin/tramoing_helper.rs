//! 采集助手（仅 Windows）。
//!
//! Windows 拿「每进程流量」必须提权，所以把提权的那部分单独做成一个不带界面的
//! 小程序：开机登录时以最高权限静默运行，开 ETW 会话，把每秒一帧的数据写进命名管道。
//! 托盘程序始终保持普通权限；统计管道只读，网络控制使用独立的受限管道。
//!
//! 用法（需要在管理员终端里执行一次）：
//!   tramoing-helper.exe --install       注册为登录自启（最高权限）并立即启动
//!   tramoing-helper.exe --uninstall     停止并移除
//!   tramoing-helper.exe --dump-events   诊断：把 ETW 原始事件写进日志，用于校准事件表
//!   tramoing-helper.exe --clear-network-blocks  移除 Tramoing 创建的全部断网规则
//!
//! 所有输出写日志文件而不是控制台 —— 它是以隐藏窗口方式在后台跑的，
//! 打印到 stdout 没人看得到。

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    win::main();

    #[cfg(not(windows))]
    {
        eprintln!("tramoing-helper 只用于 Windows。macOS 上采集由 nettop 完成，不需要助手进程。");
    }
}

#[cfg(windows)]
mod win {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use tm_windows::agg::{Aggregator, AppAccumulator};
    use tm_windows::etw::{self, EtwSession};
    use tm_windows::pipe::PipeServer;
    use tm_windows::proc::PathCache;
    use tm_windows::scheduled_task::{self, TASK_NAME};

    pub const LOG_HINT: &str = r"%LOCALAPPDATA%\Tramoing\helper.log";

    pub fn main() {
        let directory = std::env::var_os("TM_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(
                    std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| ".".into()),
                )
                .join(tm_core::identity::APP_NAME)
            });
        if let Err(error) = tm_diagnostics::init(&directory, "helper", env!("CARGO_PKG_VERSION")) {
            eprintln!("无法初始化助手日志：{error}");
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has = |f: &str| args.iter().any(|a| a == f);

        if has("--help") || has("-h") {
            log("用法: tramoing-helper [--install|--uninstall|--dump-events|--clear-network-blocks]");
            return;
        }
        if has("--install") {
            std::process::exit(install());
        }
        if has("--uninstall") {
            std::process::exit(uninstall());
        }
        if has("--clear-network-blocks") {
            let result = tm_windows::firewall::Firewall::open().and_then(|f| f.remove_all());
            if let Err(error) = result {
                log(&error);
                std::process::exit(1);
            }
            return;
        }

        run(has("--dump-events"));
    }

    /// 主循环：ETW → 累计器 → 管道。客户端断开就重建管道继续等。
    fn run(dump_events: bool) -> ! {
        if dump_events {
            etw::set_dump_events(true);
            log("已开启事件诊断模式（每条 ETW 事件写到 %LOCALAPPDATA%\\Tramoing\\etw-dump.log）");
        }
        log("助手启动");

        let agg = Arc::new(Mutex::new(Aggregator::new()));
        let _session = match EtwSession::start(agg.clone()) {
            Ok(s) => {
                log("ETW 会话已启动（Microsoft-Windows-Kernel-Network）");
                s
            }
            Err(e @ tm_windows::etw::EtwError::AlreadyRunning) => {
                // 计划任务和手动启动可能撞车，这时不该报错刷日志：
                // 已经有一个在服务了，本进程安静退出即可。
                log(&format!("{e}，本进程退出"));
                std::process::exit(0);
            }
            Err(e) => {
                log::error!(target: "tramoing::helper", "ETW 启动失败：{e}");
                if matches!(e, etw::EtwError::NeedAdmin) {
                    log("请确认本程序以管理员权限运行（--install 注册的任务会自动带最高权限）");
                }
                std::process::exit(1);
            }
        };

        // Sensor calls cannot block ETW or firewall control.
        std::thread::spawn(|| tm_windows::temperature_service::run());
        // Graphics tracing has its own opt-in lease and cannot block network sampling.
        std::thread::spawn(|| tm_windows::fps::run());

        std::thread::spawn(|| loop {
            match tm_windows::network_control::Server::create() {
                Ok(server) => server.run(log),
                Err(error) => log(&error),
            }
            std::thread::sleep(Duration::from_secs(5));
        });

        // 采样不随 UI 是否连接而停止。原始 PID 表每秒取走，避免无人连接时无限积累。
        let latest = Arc::new((
            Mutex::new(tm_windows::proto::Frame::new(0, Vec::new())),
            Condvar::new(),
        ));
        let output = latest.clone();
        std::thread::spawn(move || {
            let mut cache = PathCache::new();
            let mut apps = AppAccumulator::default();
            let source_clock = Instant::now();
            loop {
                let tick = Instant::now();
                let mut batch = agg.lock().unwrap().drain_with(|_| String::new());
                batch.sampled_at_us = Some(source_clock.elapsed().as_micros() as u64);
                let active: Vec<u32> = batch.procs.iter().map(|p| p.pid).collect();
                // 进程查询在 ETW 锁外，回调不等待系统 API。
                for entry in &mut batch.procs {
                    entry.name = cache.resolve(entry.pid);
                }
                cache.retain_pids(&active);
                *output.0.lock().unwrap() = apps.update(batch);
                output.1.notify_one();
                std::thread::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed()));
            }
        });
        let mut scratch = Vec::with_capacity(64 * 1024);

        loop {
            let server = match PipeServer::create() {
                Ok(s) => s,
                Err(e) => {
                    log(&format!("创建管道失败：{e}，5 秒后重试"));
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };

            if let Err(e) = server.accept() {
                log(&format!("等待客户端失败：{e}"));
                continue;
            }
            log("界面已连接，开始推送");

            let mut last_sent = None;
            loop {
                // 新采样到达即发送，避免两个独立定时器重复发送旧帧。
                let frame = {
                    let guard = latest.0.lock().unwrap();
                    latest
                        .1
                        .wait_while(guard, |frame| last_sent == Some(frame.seq))
                        .unwrap()
                        .clone()
                };
                if let Err(e) = server.send_frame(&frame, &mut scratch) {
                    log(&format!("推送中断：{e}"));
                    break;
                }

                last_sent = Some(frame.seq);
            }
        }
    }

    /// 注册成登录自启任务（最高权限）。
    ///
    /// 这里用计划任务而不是真正的 Windows 服务：效果等价（登录即以最高权限静默运行），
    /// 但不用写 SCM 胶水代码、不用处理服务恢复策略，卸载也更干净。
    /// 后续要做得更正式，可以平移到 `windows-service`。
    fn install() -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                log(&format!("取自身路径失败：{e}"));
                return 1;
            }
        };

        match scheduled_task::install(&exe) {
            Ok(()) => {
                log("已注册登录自启任务（允许电池供电，无运行时限）");
                log(&format!("日志：{LOG_HINT}"));
                log("任务调度器已接受助手启动请求；采集就绪以 ETW 和管道日志为准");
                0
            }
            Err(error) => {
                log(&error);
                log("请右键「以管理员身份运行」后重试");
                1
            }
        }
    }

    fn uninstall() -> i32 {
        let _ = std::process::Command::new("schtasks")
            .args(["/End", "/TN", scheduled_task::LEGACY_TASK_NAME])
            .output();
        let _ = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", scheduled_task::LEGACY_TASK_NAME, "/F"])
            .output();
        let _ = std::process::Command::new("schtasks")
            .args(["/End", "/TN", TASK_NAME])
            .output();
        if let Err(error) = tm_windows::firewall::Firewall::open().and_then(|f| f.remove_all()) {
            log(&format!("断网规则清理失败，保留程序以便恢复联网：{error}"));
            return 1;
        }
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                log("已移除自启任务");
                0
            }
            Ok(o) => {
                log(&format!(
                    "移除失败：{}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
                1
            }
            Err(e) => {
                log(&format!("调用 schtasks 失败：{e}"));
                1
            }
        }
    }

    /// Shared bounded file logger; helper and UI have separate files.
    pub fn log(msg: &str) {
        log::info!(target: "tramoing::helper", "{msg}");
    }
}
