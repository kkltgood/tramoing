//! 开机自启。
//!
//! macOS 用 LaunchAgent（`~/Library/LaunchAgents/*.plist`），Windows 用
//! `HKCU\...\Run` 注册表项。两边都是「当前用户」级别，不需要管理员权限，
//! 也不会在系统里留下需要卸载的服务。

#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// 自启状态。
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

/// 打开或关闭自启。失败时返回可以直接展示给用户的中文原因。
pub fn set_enabled(on: bool) -> Result<(), String> {
    imp::set_enabled(on)
}

/// 正式启动时把已有登录项移到当前名称；没有旧登录项时不自动开启自启。
pub fn migrate_legacy() -> Result<(), String> {
    if imp::legacy_enabled() {
        imp::set_enabled(true)?;
    }
    Ok(())
}

// ---------- macOS ----------

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Stdio};

    const LABEL: &str = tm_core::identity::MACOS_BUNDLE_ID;
    const LEGACY_LABEL: &str = tm_core::identity::LEGACY_MACOS_BUNDLE_ID;

    extern "C" {
        fn getuid() -> u32;
    }

    fn plist_path(label: &str) -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
        )
    }

    pub fn is_enabled() -> bool {
        plist_path(LABEL).is_some_and(|p| p.exists()) || legacy_enabled()
    }

    pub fn legacy_enabled() -> bool {
        plist_path(LEGACY_LABEL).is_some_and(|p| p.exists())
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        let path = plist_path(LABEL).ok_or("读不到 HOME，无法定位 LaunchAgents")?;
        if on {
            let exe = std::env::current_exe().map_err(|e| format!("取不到自身路径：{e}"))?;
            write_plist(&path, &launch_argv(&exe))?;
            remove_agent(LEGACY_LABEL)?;
            // 让它在本次会话立即生效。失败不影响「下次登录自启」——
            // launchd 登录时会自己扫描 LaunchAgents 目录。
            // 某些环境（远程会话、受限会话）根本不允许 launchctl 加载，
            // 所以这里只把错误吞掉，不拿它当失败。
            if !launchctl(&["bootstrap", &domain(), &path.to_string_lossy()]) {
                // 老系统只认 load/unload
                let _ = launchctl(&["load", "-w", &path.to_string_lossy()]);
            }
        } else {
            remove_agent(LABEL)?;
            remove_agent(LEGACY_LABEL)?;
        }
        Ok(())
    }

    fn remove_agent(label: &str) -> Result<(), String> {
        let path = plist_path(label).ok_or("读不到 HOME，无法定位 LaunchAgents")?;
        if path.exists() {
            let _ = launchctl(&["bootout", &format!("{}/{label}", domain())]);
            let _ = launchctl(&["unload", "-w", &path.to_string_lossy()]);
            std::fs::remove_file(&path).map_err(|e| format!("删除 LaunchAgent 失败：{e}"))?;
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
        tm_store::files::atomic_write(path, plist_xml(argv).as_bytes())
            .map_err(|e| format!("写入 LaunchAgent 失败：{e}"))
    }

    /// 从可执行文件路径反推 `.app` 包路径。
    fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
        tm_macos::installation::app_bundle(exe)
    }

    /// LaunchAgent 的启动命令。
    ///
    /// 跑在 `.app` 里时要通过 `open -g` 拉起整个包：直接执行
    /// `Contents/MacOS/tramoing` 会丢掉 bundle 身份，
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
            let xml = plist_xml(&["/Applications/Tramoing.app".to_owned()]);
            assert!(xml.contains("<string>dev.guolongkang.tramoing</string>"));
            assert!(xml.contains("/Applications/Tramoing.app"));
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
                    "/Applications/Tramoing.app/Contents/MacOS/tramoing"
                )),
                Some(PathBuf::from("/Applications/Tramoing.app"))
            );
            assert_eq!(
                app_bundle_of(Path::new("/Users/x/code/target/debug/tramoing")),
                None
            );
        }

        #[test]
        fn launch_uses_open_inside_bundle() {
            let argv = launch_argv(Path::new(
                "/Applications/Tramoing.app/Contents/MacOS/tramoing",
            ));
            assert_eq!(argv[0], "/usr/bin/open");
            assert_eq!(argv[2], "/Applications/Tramoing.app");

            let dev = launch_argv(Path::new("/Users/x/code/target/debug/tramoing"));
            assert_eq!(dev.len(), 1, "开发时直接执行自身，不要绕 open");
        }

        /// 只验证「写文件 → 状态成立 → 删文件 → 状态消失」这条链。
        ///
        /// 这里**不能**调 `set_enabled`：它会跑 `launchctl bootstrap`，
        /// 而测试进程的 `current_exe` 是测试二进制本身——真注册上去，
        /// 下次登录会跑一遍整个测试套件。要真机验证请用
        /// `tramoing --autostart on`。
        #[test]
        fn plist_write_remove_roundtrip() {
            let dir = std::env::temp_dir().join("tm-test-launchagent");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("dev.guolongkang.tramoing.plist");
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
    // 注册表读写放在 tm-windows 里：那个 crate 才有 windows-sys 依赖，
    // 也符合「Windows 系统调用集中在 tm-windows」的分层。
    const VALUE: &str = tm_core::identity::APP_NAME;
    const LEGACY_VALUE: &str = tm_core::identity::LEGACY_APP_NAME;

    pub fn is_enabled() -> bool {
        tm_windows::registry_run::value_exists(VALUE) || legacy_enabled()
    }

    pub fn legacy_enabled() -> bool {
        tm_windows::registry_run::value_exists(LEGACY_VALUE)
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        let exe = if on {
            std::env::current_exe()
                .map_err(|e| format!("取不到自身路径：{e}"))?
                .display()
                .to_string()
        } else {
            String::new()
        };
        tm_windows::registry_run::set_value(VALUE, on, &exe)
            .map_err(|code| format!("写入注册表失败（Win32 错误 {code}）"))?;
        tm_windows::registry_run::set_value(LEGACY_VALUE, false, "")
            .map_err(|code| format!("写入注册表失败（Win32 错误 {code}）"))
    }
}

// ---------- 其他平台 ----------

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn legacy_enabled() -> bool {
        false
    }

    pub fn is_enabled() -> bool {
        false
    }

    pub fn set_enabled(_on: bool) -> Result<(), String> {
        Err("当前平台暂不支持开机自启".to_owned())
    }
}
