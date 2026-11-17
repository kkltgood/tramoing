//! 采集助手（仅 Windows）。
//!
//! Windows 拿「每进程流量」必须提权，所以把提权的那部分单独做成一个不带界面的
//! 小程序：开机登录时以最高权限静默运行，开 ETW 会话，把每秒一帧的数据写进命名管道。
//! 托盘程序始终保持普通权限，只读管道。
//!
//! 用法（需要在管理员终端里执行一次）：
//!   tm-helper.exe --install       注册为登录自启（最高权限）并立即启动
//!   tm-helper.exe --uninstall     停止并移除
//!   tm-helper.exe --dump-events   诊断：把 ETW 原始事件写进日志，用于校准事件表
//!
//! 所有输出写日志文件而不是控制台 —— 它是以隐藏窗口方式在后台跑的，
//! 打印到 stdout 没人看得到。

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    win::main();

    #[cfg(not(windows))]
    {
        eprintln!("tm-helper 只用于 Windows。macOS 上采集由 nettop 完成，不需要助手进程。");
    }
}

#[cfg(windows)]
mod win {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tm_windows::agg::Aggregator;
    use tm_windows::etw::{self, EtwSession};
    use tm_windows::pipe::PipeServer;
    use tm_windows::proc::PathCache;

    pub const TASK_NAME: &str = "TrafficMonitorHelper";
    pub const LOG_HINT: &str = r"%LOCALAPPDATA%\TrafficMonitor\helper.log";

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has = |f: &str| args.iter().any(|a| a == f);

        if has("--help") || has("-h") {
            log("用法: tm-helper [--install|--uninstall|--dump-events]");
            return;
        }
        if has("--install") {
            std::process::exit(install());
        }
        if has("--uninstall") {
            std::process::exit(uninstall());
        }

        run(has("--dump-events"));
    }

    /// 主循环：ETW → 累计器 → 管道。客户端断开就重建管道继续等。
    fn run(dump_events: bool) -> ! {
        if dump_events {
            etw::set_dump_events(true);
            log("已开启事件诊断模式（每条 ETW 事件写到 %LOCALAPPDATA%\\TrafficMonitor\\etw-dump.log）");
        }
        log("助手启动");

        let agg = Arc::new(Mutex::new(Aggregator::new()));
        let _session = match EtwSession::start(agg.clone()) {
            Ok(s) => {
                log("ETW 会话已启动（Microsoft-Windows-Kernel-Network）");
                s
            }
            Err(e) => {
                log(&format!("ETW 启动失败：{e}"));
                log("请确认本程序以管理员权限运行（--install 注册的任务会自动带最高权限）");
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
        };

        let mut cache = PathCache::new();
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

            loop {
                let frame = agg.lock().unwrap().snapshot_with(|pid| cache.resolve(pid));
                if let Err(e) = server.send_frame(&frame, &mut scratch) {
                    log(&format!("推送中断：{e}"));
                    break;
                }

                // 帧里没出现的进程视为已退出，清掉缓存防止长跑时膨胀
                let alive: Vec<u32> = frame.procs.iter().map(|p| p.pid).collect();
                agg.lock().unwrap().retain_pids(&alive);
                cache.retain_pids(&alive);

                std::thread::sleep(Duration::from_secs(1));
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

        // 先清掉可能存在的旧任务，避免 /Create 撞名
        let _ = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output();

        let tr = format!("\"{}\"", exe.display());
        let out = std::process::Command::new("schtasks")
            .args([
                "/Create", "/TN", TASK_NAME, "/TR", &tr, "/SC", "ONLOGON", "/RL", "HIGHEST",
                "/F",
            ])
            .output();

        match out {
            Ok(o) if o.status.success() => {
                log("已注册登录自启任务");
        log(&format!("日志：{LOG_HINT}"));
                let _ = std::process::Command::new("schtasks")
                    .args(["/Run", "/TN", TASK_NAME])
                    .output();
                log("已启动助手");
                0
            }
            Ok(o) => {
                log(&format!(
                    "注册失败（多半是没有管理员权限）：{}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ));
                log("请右键「以管理员身份运行」后重试");
                1
            }
            Err(e) => {
                log(&format!("调用 schtasks 失败：{e}"));
                1
            }
        }
    }

    fn uninstall() -> i32 {
        let _ = std::process::Command::new("schtasks")
            .args(["/End", "/TN", TASK_NAME])
            .output();
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output();
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "tm-helper.exe", "/F"])
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

    /// 追加一行日志。窗口是隐藏的，只能写文件。
    pub fn log(msg: &str) {
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "[{secs}] {msg}");
        }
        // 开发时从终端直接跑也能看到
        eprintln!("[tm-helper] {msg}");
    }

    fn log_path() -> std::path::PathBuf {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_owned());
        std::path::Path::new(&base)
            .join("TrafficMonitor")
            .join("helper.log")
    }
}
