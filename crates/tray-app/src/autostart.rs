//! 开机自启。
//!
//! macOS 用 LaunchAgent（`~/Library/LaunchAgents/*.plist`），Windows 用
//! `HKCU\...\Run` 注册表项。两边都是「当前用户」级别，不需要管理员权限，
//! 也不会在系统里留下需要卸载的服务。

use std::path::PathBuf;

/// 自启状态。
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// 打开或关闭自启。失败时返回可以直接展示给用户的中文原因。
pub fn set_enabled(on: bool) -> Result<(), String> {
    imp::set_enabled(on)
}

// ---------- macOS ----------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};

    const LABEL: &str = "dev.guolongkang.trafficmonitor";

    extern "C" {
        fn getuid() -> u32;
    }

    fn plist_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        )
    }

    pub fn is_enabled() -> bool {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        let path = plist_path().ok_or("读不到 HOME，无法定位 LaunchAgents")?;
        if on {
            let exe = std::env::current_exe().map_err(|e| format!("取不到自身路径：{e}"))?;
            write_plist(&path, &launch_argv(&exe))?;
            // 让它在本次会话立即生效。失败不影响「下次登录自启」——
            // launchd 登录时会自己扫描 LaunchAgents 目录。
            // 某些环境（远程会话、受限会话）根本不允许 launchctl 加载，
            // 所以这里只把错误吞掉，不拿它当失败。
            if !launchctl(&["bootstrap", &domain(), &path.to_string_lossy()]) {
                // 老系统只认 load/unload
                let _ = launchctl(&["load", "-w", &path.to_string_lossy()]);
            }
        } else {
            let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
            let _ = launchctl(&["unload", "-w", &path.to_string_lossy()]);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("删除 LaunchAgent 失败：{e}")),
            }
        }
        Ok(())
    }

    fn domain() -> String {
        format!("gui/{}", unsafe { getuid() })
    }

    /// 跑一条 launchctl 子命令，成功返回 true。
    /// 输出重定向到 null：失败原因是环境限制，没必要糊到用户终端上。
    fn launchctl(args: &[&str]) -> bool {
        Command::new("launchctl")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn write_plist(path: &Path, argv: &[String]) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 目录失败：{e}"))?;
        }
        std::fs::write(path, plist_xml(argv)).map_err(|e| format!("写入 LaunchAgent 失败：{e}"))
    }

    /// 从可执行文件路径反推 `.app` 包路径。
    fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
        let s = exe.to_string_lossy();
        let idx = s.find(".app/Contents/MacOS/")?;
        Some(PathBuf::from(&s[..idx + 4]))
    }

    /// LaunchAgent 的启动命令。
    ///
    /// 跑在 `.app` 里时要通过 `open -g` 拉起整个包：直接执行
    /// `Contents/MacOS/traffic-monitor` 会丢掉 bundle 身份，
    /// LSUIElement（不进 Dock）、图标、容器目录全都不生效，
    /// 系统还会弹一条「App 后台活动」。
    fn launch_argv(exe: &Path) -> Vec<String> {
        match app_bundle_of(exe) {
            Some(app) => vec![
                "/usr/bin/open".to_owned(),
                "-g".to_owned(),
                app.to_string_lossy().into_owned(),
            ],
            // 开发时跑 target/debug 里的裸二进制，只能直接执行
            None => vec![exe.to_string_lossy().into_owned()],
        }
    }

    /// LaunchAgent 内容。`KeepAlive` 一定要 false：崩溃后不停重启会让用户
    /// 以为中了病毒，托盘工具挂了就挂了，下次登录再说。
    fn plist_xml(argv: &[String]) -> String {
        let args = argv
            .iter()
            .map(|a| format!("        <string>{}</string>", xml_escape(a)))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
        )
    }

    /// 路径里出现 `&` 或 `<`（比如目录名带 &）会让 plist 解析失败。
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plist_contains_label_and_args() {
            let xml = plist_xml(&["/Applications/Traffic Monitor.app".to_owned()]);
            assert!(xml.contains("<string>dev.guolongkang.trafficmonitor</string>"));
            assert!(xml.contains("/Applications/Traffic Monitor.app"));
            assert!(xml.contains("<key>RunAtLoad</key>"));
        }

        #[test]
        fn xml_escapes_special_chars() {
            assert_eq!(xml_escape("/a&b/<c>"), "/a&amp;b/&lt;c&gt;");
        }

        #[test]
        fn bundle_path_is_recovered() {
            assert_eq!(
                app_bundle_of(Path::new(
                    "/Applications/TrafficMonitor.app/Contents/MacOS/traffic-monitor"
                )),
                Some(PathBuf::from("/Applications/TrafficMonitor.app"))
            );
            assert_eq!(
                app_bundle_of(Path::new("/Users/x/code/target/debug/traffic-monitor")),
                None
            );
        }

        #[test]
        fn launch_uses_open_inside_bundle() {
            let argv = launch_argv(Path::new(
                "/Applications/TrafficMonitor.app/Contents/MacOS/traffic-monitor",
            ));
            assert_eq!(argv[0], "/usr/bin/open");
            assert_eq!(argv[2], "/Applications/TrafficMonitor.app");

            let dev = launch_argv(Path::new("/Users/x/code/target/debug/traffic-monitor"));
            assert_eq!(dev.len(), 1, "开发时直接执行自身，不要绕 open");
        }

        /// 只验证「写文件 → 状态成立 → 删文件 → 状态消失」这条链。
        ///
        /// 这里**不能**调 `set_enabled`：它会跑 `launchctl bootstrap`，
        /// 而测试进程的 `current_exe` 是测试二进制本身——真注册上去，
        /// 下次登录会跑一遍整个测试套件。要真机验证请用
        /// `traffic-monitor --autostart on`。
        #[test]
        fn plist_write_remove_roundtrip() {
            let dir = std::env::temp_dir().join("tm-test-launchagent");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("dev.guolongkang.trafficmonitor.plist");
            let _ = std::fs::remove_file(&path);

            write_plist(&path, &["/tmp/tm".to_owned()]).unwrap();
            assert!(path.exists(), "写完后 plist 应该存在");
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("<key>KeepAlive</key>"));

            std::fs::remove_file(&path).unwrap();
            assert!(!path.exists(), "删掉后应该视为已关闭");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------- Windows ----------

#[cfg(windows)]
mod imp {
    use super::*;
    use std::process::Command;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "TrafficMonitor";

    pub fn is_enabled() -> bool {
        Command::new("reg")
            .args(["query", RUN_KEY, "/v", VALUE])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        if on {
            let exe = std::env::current_exe().map_err(|e| format!("取不到自身路径：{e}"))?;
            let quoted = format!("\"{}\"", exe.display());
            run(&[
                "add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &quoted, "/f",
            ])
        } else {
            let _ = run(&["delete", RUN_KEY, "/v", VALUE, "/f"]);
            Ok(())
        }
    }

    fn run(args: &[&str]) -> Result<(), String> {
        let out = Command::new("reg")
            .args(args)
            .output()
            .map_err(|e| format!("调用 reg.exe 失败：{e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }
}

// ---------- 其他平台 ----------

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn set_enabled(_on: bool) -> Result<(), String> {
        Err("当前平台暂不支持开机自启".to_owned())
    }
}
