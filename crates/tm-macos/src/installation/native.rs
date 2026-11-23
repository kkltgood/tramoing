use super::{app_bundle, is_distribution_copy, replace_bundle, supports_cpu};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSProgressIndicator, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

const BUNDLE_ID: &str = tm_core::identity::MACOS_BUNDLE_ID;
const LEGACY_BUNDLE_ID: &str = tm_core::identity::LEGACY_MACOS_BUNDLE_ID;
const FINISH_FLAG: &str = "--finish-macos-install";
const STAGE_PREFIX: &str = ".tramoing-install-";

fn command_output(command: &mut Command, action: &str) -> Result<std::process::Output, String> {
    let output = command.output().map_err(|e| format!("{action}：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{action}：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn plist_value(bundle: &Path, key: &str) -> Result<String, String> {
    let output = command_output(
        Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(bundle.join("Contents/Info.plist")),
        "读取应用信息失败",
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn bundle_version(bundle: &Path) -> Result<String, String> {
    let identifier = plist_value(bundle, "CFBundleIdentifier")?;
    if identifier != BUNDLE_ID && identifier != LEGACY_BUNDLE_ID {
        return Err("目标不是 Tramoing，已停止安装".into());
    }
    if plist_value(bundle, "CFBundleExecutable")? != "tramoing" {
        return Err("应用入口与安装包不一致，已停止安装".into());
    }
    let version = plist_value(bundle, "CFBundleShortVersionString")?;
    semver::Version::parse(&version).map_err(|e| format!("应用版本无效：{e}"))?;
    Ok(version)
}

fn verify_bundle(bundle: &Path) -> Result<String, String> {
    // 新安装包必须使用当前标识；旧标识只用于替换目标和失败回滚。
    let version = verify_existing_bundle(bundle)?;
    if plist_value(bundle, "CFBundleIdentifier")? != BUNDLE_ID {
        return Err("安装包未使用当前 Tramoing 应用标识".into());
    }
    Ok(version)
}

fn verify_existing_bundle(bundle: &Path) -> Result<String, String> {
    for path in [
        bundle.to_path_buf(),
        bundle.join("Contents/Info.plist"),
        bundle.join("Contents/MacOS/tramoing"),
    ] {
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| format!("安装包不完整：{e}"))?;
        if metadata.file_type().is_symlink() {
            return Err("安装包中的关键文件不能是符号链接".into());
        }
    }
    let version = bundle_version(bundle)?;
    command_output(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle),
        "应用签名校验失败",
    )?;
    let mut header = Vec::new();
    File::open(bundle.join("Contents/MacOS/tramoing"))
        .and_then(|file| file.take(4096).read_to_end(&mut header))
        .map_err(|e| e.to_string())?;
    let cpu = if cfg!(target_arch = "aarch64") {
        0x0100_000c
    } else {
        0x0100_0007
    };
    if !supports_cpu(&header, cpu) {
        return Err("安装包不支持当前 Mac 架构".into());
    }
    Ok(version)
}

fn team_identifier(bundle: &Path) -> Result<Option<String>, String> {
    let output = command_output(
        Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=4"])
            .arg(bundle),
        "读取签名身份失败",
    )?;
    Ok(String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .filter(|team| *team != "not set")
        .map(str::to_owned))
}

fn dialog(title: &str, detail: &str, confirm: &str, cancel: bool) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let application = NSApplication::sharedApplication(mtm);
    // activate() requires macOS 14; the app still supports macOS 11.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str(confirm));
    if cancel {
        alert
            .addButtonWithTitle(&NSString::from_str("取消"))
            .setKeyEquivalent(&NSString::from_str("\u{1b}"));
    }
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn show_error(error: &str) {
    dialog("未能完成 Tramoing 安装", error, "好", false);
}

/// Keep a native progress window responsive while copying and verifying bundles.
fn with_progress<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let mtm = MainThreadMarker::new().ok_or("安装界面必须在主线程打开")?;
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("正在安装 Tramoing…"));
    alert.setInformativeText(&NSString::from_str(
        "正在校验并复制应用。完成后自动打开，设置和历史记录会保留。",
    ));
    alert
        .addButtonWithTitle(&NSString::from_str("请稍候"))
        .setEnabled(false);
    let progress = NSProgressIndicator::initWithFrame(
        NSProgressIndicator::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(300.0, 16.0)),
    );
    progress.setIndeterminate(true);
    unsafe {
        progress.startAnimation(None);
    }
    alert.setAccessoryView(Some(&progress));
    alert.layout();
    let window = alert.window();
    window.center();
    window.orderFrontRegardless();
    let application = NSApplication::sharedApplication(mtm);
    let session = application.beginModalSessionForWindow(&window);
    let worker = std::thread::Builder::new()
        .name("macos-install".into())
        .spawn(work)
        .map_err(|e| format!("无法启动安装：{e}"));
    let result = match worker {
        Ok(worker) => {
            while !worker.is_finished() {
                // The session belongs to this window and remains valid until endModalSession.
                unsafe {
                    application.runModalSession(session);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            worker
                .join()
                .unwrap_or_else(|_| Err("安装准备意外中断，原应用未改动".into()))
        }
        Err(error) => Err(error),
    };
    unsafe {
        progress.stopAnimation(None);
        application.endModalSession(session);
    }
    window.close();
    result
}

fn installed_destination(source: &Path) -> Result<PathBuf, String> {
    for identifier in [BUNDLE_ID, LEGACY_BUNDLE_ID] {
        let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(identifier),
        );
        for app in applications.iter() {
            if let Some(path) = app.bundleURL().and_then(|url| url.path()) {
                let path = PathBuf::from(path.to_string());
                if path != source && !is_distribution_copy(&path) {
                    return Ok(path);
                }
            }
        }
        if let Some(path) = NSWorkspace::sharedWorkspace()
            .URLForApplicationWithBundleIdentifier(&NSString::from_str(identifier))
            .and_then(|url| url.path())
        {
            let path = PathBuf::from(path.to_string());
            if path != source && path.is_dir() && !is_distribution_copy(&path) {
                return Ok(path);
            }
        }
    }
    let user_applications =
        PathBuf::from(std::env::var_os("HOME").ok_or("无法找到用户目录")?).join("Applications");
    let system = PathBuf::from("/Applications/Tramoing.app");
    let personal = user_applications.join("Tramoing.app");
    if system.exists() {
        return Ok(system);
    }
    if personal.exists() {
        return Ok(personal);
    }
    // A standard account can install into its own Applications without an admin password.
    if tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .tempdir_in("/Applications")
        .is_ok()
    {
        return Ok(system);
    }
    std::fs::create_dir_all(&user_applications)
        .map_err(|e| format!("创建应用程序目录失败：{e}"))?;
    Ok(personal)
}

fn stop_previous_app(destination: &Path) -> Result<(), String> {
    let mut processes = Vec::new();
    for identifier in [BUNDLE_ID, LEGACY_BUNDLE_ID] {
        let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(
            &NSString::from_str(identifier),
        );
        for app in applications.iter() {
            let path = app
                .bundleURL()
                .and_then(|url| url.path())
                .map(|p| PathBuf::from(p.to_string()));
            if path
                .as_deref()
                .and_then(|path| path.canonicalize().ok())
                .as_deref()
                == Some(destination)
            {
                let pid = app.processIdentifier() as u32;
                if pid != std::process::id() {
                    if let Some(start) = crate::procname::start_time(pid) {
                        if !app.terminate() {
                            return Err("请先退出已安装的 Tramoing，再重试安装".into());
                        }
                        processes.push((pid, start));
                    }
                }
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while processes
        .iter()
        .any(|(pid, start)| crate::procname::start_time(*pid) == Some(*start))
    {
        if Instant::now() >= deadline {
            return Err("旧程序尚未退出，安装已停止；原应用未改动".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

struct Prepared {
    directory: tempfile::TempDir,
    destination: PathBuf,
}

fn prepare(
    source: &Path,
    destination: &Path,
    expected_version: Option<&str>,
) -> Result<Prepared, String> {
    let version = verify_bundle(source)?;
    if expected_version.is_some_and(|expected| expected != version) {
        return Err("安装包版本与更新清单不一致".into());
    }
    let parent = destination
        .parent()
        .ok_or("安装目录无效")?
        .canonicalize()
        .map_err(|e| format!("读取安装目录失败：{e}"))?;
    let destination = parent.join(destination.file_name().ok_or("应用名称无效")?);
    if destination
        .extension()
        .is_none_or(|extension| extension != "app")
        || std::fs::symlink_metadata(&destination).is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err("安装目标必须是实际的 .app 应用".into());
    }
    let previous = if destination.exists() {
        bundle_version(&destination)?
    } else {
        String::new()
    };
    if !previous.is_empty() {
        let old = semver::Version::parse(&previous).map_err(|e| e.to_string())?;
        let new = semver::Version::parse(&version).map_err(|e| e.to_string())?;
        if new.cmp_precedence(&old).is_lt() {
            return Err("已安装的版本更新，无需安装这个旧版本".into());
        }
        if let Some(team) = team_identifier(&destination)? {
            if team_identifier(source)?.as_deref() != Some(team.as_str()) {
                return Err("更新包的开发者签名与已安装版本不一致".into());
            }
        }
    }
    let directory = tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .tempdir_in(&parent)
        .map_err(|e| {
            format!(
                "无法写入 {}。请通过 Finder 更新此应用。详情：{e}",
                parent.display()
            )
        })?;
    let staged = directory.path().join("Tramoing.app");
    command_output(
        Command::new("/usr/bin/ditto")
            .args(["--rsrc", "--extattr"])
            .arg(source)
            .arg(&staged),
        "复制应用失败",
    )?;
    verify_bundle(&staged)?;
    std::fs::write(directory.path().join("previous-version"), previous)
        .map_err(|e| e.to_string())?;
    Ok(Prepared {
        directory,
        destination,
    })
}

impl Prepared {
    fn handoff(self) -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let helper = self.directory.path().join("apply-install");
        std::fs::copy(executable, &helper).map_err(|e| format!("准备安装助手失败：{e}"))?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let pid = std::process::id();
        let (seconds, micros) = crate::procname::start_time(pid).ok_or("无法确认当前进程身份")?;
        let log =
            File::create(self.directory.path().join("installer.log")).map_err(|e| e.to_string())?;
        let mut child = Command::new(helper)
            .arg(FINISH_FLAG)
            .arg(self.directory.path())
            .arg(&self.destination)
            .arg(pid.to_string())
            .arg(seconds.to_string())
            .arg(micros.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| format!("启动安装助手失败：{e}"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.directory.path().join("ready").is_file() {
                // The worker now owns cleanup and keeps the backup on any failure.
                let _ = self.directory.keep();
                return Ok(());
            }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                let error = std::fs::read_to_string(self.directory.path().join("installer.log"))
                    .unwrap_or_default();
                return Err(format!("安装助手未能启动：{}", error.trim()));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("安装助手启动超时，原应用未改动，请重试".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Called before taking the normal per-user UI lock, so a DMG copy can update a
/// running installation. Cancel exits the DMG copy instead of running from it.
pub fn offer_install() -> Result<bool, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let Some(source) = app_bundle(&executable).filter(|app| is_distribution_copy(app)) else {
        return Ok(false);
    };
    let destination = installed_destination(&source)?;
    let updating = destination.exists();
    let action = if updating {
        "更新并打开"
    } else {
        "安装并打开"
    };
    let detail = format!("将 Tramoing {}到：\n{}\n\n完成后自动打开。你可以在屏幕顶部的菜单栏查看网速，设置和历史记录会保留。", if updating { "更新" } else { "安装" }, destination.display());
    if !dialog("欢迎使用 Tramoing", &detail, action, true) {
        return Ok(true);
    }
    with_progress(move || {
        let prepared = prepare(&source, &destination, None)?;
        stop_previous_app(&prepared.destination)?;
        prepared.handoff()
    })?;
    Ok(true)
}

struct MountedImage {
    directory: PathBuf,
}
impl Drop for MountedImage {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(&self.directory)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // Never recursively clean a mountpoint if detach failed or is delayed.
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Mount privately and prepare a complete verified replacement while the old
/// application is still running. The caller exits only after this succeeds.
pub fn start_update(image: &Path, version: &str) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let destination = app_bundle(&executable)
        .filter(|app| !is_distribution_copy(app))
        .ok_or("请先将 Tramoing 安装到应用程序目录，再进行更新")?;
    let directory = tempfile::Builder::new()
        .prefix("tramoing-dmg-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let mount = MountedImage {
        directory: directory.keep(),
    };
    command_output(
        Command::new("/usr/bin/hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
            .arg(&mount.directory)
            .arg(image),
        "打开更新安装包失败",
    )?;
    let prepared = prepare(
        &mount.directory.join("Tramoing.app"),
        &destination,
        Some(version),
    )?;
    drop(mount);
    prepared.handoff()
}

fn open_application(app: &Path, reveal: bool) -> Result<(), String> {
    let mut command = Command::new("/usr/bin/open");
    command.arg("-n").arg(app);
    if reveal {
        command.args(["--args", "--after-install"]);
    }
    command_output(&mut command, "打开 Tramoing 失败")?;
    Ok(())
}

fn finish(directory: &Path, destination: &Path, pid: u32, start: (u64, u64)) -> Result<(), String> {
    let directory = directory.canonicalize().map_err(|e| e.to_string())?;
    if !directory
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(STAGE_PREFIX))
        || destination.parent() != directory.parent()
        || std::env::current_exe().ok().as_deref()
            != Some(directory.join("apply-install").as_path())
        || destination
            .extension()
            .is_none_or(|extension| extension != "app")
    {
        return Err("安装助手参数无效".into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(
            directory
                .parent()
                .ok_or("安装目录无效")?
                .join(".Tramoing.install.lock"),
        )
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| "另一次 Tramoing 安装正在进行，请稍后重试".to_owned())?;
    let staged = directory.join("Tramoing.app");
    verify_bundle(&staged)?;
    let previous =
        std::fs::read_to_string(directory.join("previous-version")).map_err(|e| e.to_string())?;
    std::fs::write(directory.join("ready"), "ready").map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while crate::procname::start_time(pid) == Some(start) {
        if Instant::now() >= deadline {
            return Err("旧程序尚未退出，安装已停止；原应用未改动".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let current = if destination.exists() {
        bundle_version(destination)?
    } else {
        String::new()
    };
    if current != previous {
        return Err("安装目录中的版本已改变，请重新检查更新".into());
    }
    let result = replace_bundle(
        &staged,
        destination,
        &directory.join("previous.app"),
        |app| open_application(app, true),
    );
    if let Err(error) = result {
        if !previous.is_empty()
            && verify_existing_bundle(destination).is_ok_and(|version| version == previous)
        {
            let _ = open_application(destination, false);
        }
        return Err(format!("{error}\n安装恢复文件：{}", directory.display()));
    }
    // Nothing in the user data directory is part of this cleanup.
    let _ = std::fs::remove_dir_all(&directory);
    Ok(())
}

pub fn finish_if_requested(args: &[String]) -> Option<Result<(), String>> {
    if args.first().map(String::as_str) != Some(FINISH_FLAG) {
        return None;
    }
    Some((|| {
        if args.len() != 6 {
            return Err("安装助手参数不完整".into());
        }
        let pid = args[3].parse::<u32>().map_err(|e| e.to_string())?;
        let seconds = args[4].parse::<u64>().map_err(|e| e.to_string())?;
        let micros = args[5].parse::<u64>().map_err(|e| e.to_string())?;
        if pid == 0 || seconds == 0 {
            return Err("安装进程身份无效".into());
        }
        finish(
            Path::new(&args[1]),
            Path::new(&args[2]),
            pid,
            (seconds, micros),
        )
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_fixture(path: &Path, version: &str) {
        signed_fixture_with_id(path, version, BUNDLE_ID);
    }

    fn signed_fixture_with_id(path: &Path, version: &str, identifier: &str) {
        std::fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        let source = path.parent().unwrap().join("fixture.c");
        std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
        command_output(
            Command::new("/usr/bin/xcrun")
                .args([
                    "clang",
                    "-arch",
                    if cfg!(target_arch = "aarch64") {
                        "arm64"
                    } else {
                        "x86_64"
                    },
                ])
                .arg(&source)
                .arg("-o")
                .arg(path.join("Contents/MacOS/tramoing")),
            "compile fixture",
        )
        .unwrap();
        std::fs::write(path.join("Contents/Info.plist"), format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{identifier}</string><key>CFBundleExecutable</key><string>tramoing</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>{version}</string><key>CFBundleVersion</key><string>{version}</string></dict></plist>"
        )).unwrap();
        command_output(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(path),
            "sign fixture",
        )
        .unwrap();
    }

    #[test]
    fn current_bundle_can_replace_a_legacy_install_but_legacy_payload_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Download.app");
        let destination = root.path().join("Tramoing.app");
        signed_fixture(&source, "1.1.0");
        signed_fixture_with_id(&destination, "1.0.0", LEGACY_BUNDLE_ID);
        assert!(verify_bundle(&destination).is_err());
        assert_eq!(verify_existing_bundle(&destination).unwrap(), "1.0.0");
        let prepared = prepare(&source, &destination, Some("1.1.0")).unwrap();
        assert_eq!(
            verify_bundle(&prepared.directory.path().join("Tramoing.app")).unwrap(),
            "1.1.0"
        );
        assert_eq!(
            plist_value(&destination, "CFBundleIdentifier").unwrap(),
            LEGACY_BUNDLE_ID
        );
    }

    #[test]
    fn preparing_a_signed_update_preserves_the_installed_bundle_until_handoff() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Download.app");
        let destination = root.path().join("Tramoing.app");
        signed_fixture(&source, "1.1.0");
        signed_fixture(&destination, "1.0.0");
        let prepared = prepare(&source, &destination, Some("1.1.0")).unwrap();
        assert_eq!(
            verify_bundle(&prepared.directory.path().join("Tramoing.app")).unwrap(),
            "1.1.0"
        );
        assert_eq!(verify_bundle(&destination).unwrap(), "1.0.0");
        assert_eq!(
            prepared.directory.path().parent(),
            destination.parent().unwrap().canonicalize().ok().as_deref()
        );
    }

    #[test]
    fn manifest_version_mismatches_and_downgrades_leave_the_old_app_intact() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Download.app");
        let destination = root.path().join("Tramoing.app");
        signed_fixture(&source, "1.0.0");
        signed_fixture(&destination, "2.0.0");
        assert!(prepare(&source, &destination, Some("3.0.0")).is_err());
        assert!(prepare(&source, &destination, None).is_err());
        assert_eq!(verify_bundle(&destination).unwrap(), "2.0.0");
    }

    #[test]
    fn internal_worker_rejects_unrelated_launches_and_incomplete_parameters() {
        assert!(finish_if_requested(&["--show".into()]).is_none());
        assert!(finish_if_requested(&[FINISH_FLAG.into()]).unwrap().is_err());
    }

    #[test]
    fn unsigned_or_unrelated_payloads_are_rejected_before_installation() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_bundle(dir.path()).is_err());
        let contents = dir.path().join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(contents.join("Info.plist"), "not a property list").unwrap();
        std::fs::write(contents.join("MacOS/tramoing"), "not an executable").unwrap();
        assert!(verify_bundle(dir.path()).is_err());
    }
}
