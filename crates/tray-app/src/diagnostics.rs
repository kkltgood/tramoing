//! User initiated, local diagnostic reports. No upload and no traffic database access.
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LOG_TAIL: u64 = 128 * 1024;
const COMMAND_LIMIT: u64 = 128 * 1024;

pub fn export_report(status: &str) -> Result<PathBuf, String> {
    log::info!(target: "tramoing::diagnostics", "开始导出诊断报告");
    let directory = tm_store::data_dir();
    let mut report = format!(
        "Tramoing diagnostic report / 诊断报告\nFormat: 1\nVersion: {}\nUTC: {}\nOS: {}\nArchitecture: {}\nBuild: {}\n\n[用户补充 / Please describe]\n发生时间：\n操作步骤：\n预期与实际结果：\n\n[当前状态 / Current status]\n{status}\n",
        crate::update::CURRENT, tm_diagnostics::timestamp(), std::env::consts::OS,
        std::env::consts::ARCH, if cfg!(debug_assertions) { "debug" } else { "release" },
    );
    report.push_str(&format!(
        "\n[日志写入 / Logging]\nPath: {}\nLast write error: {}\n",
        tm_diagnostics::log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unavailable".into()),
        tm_diagnostics::write_error().unwrap_or_else(|| "none".into())
    ));
    if let Ok(executable) = std::env::current_exe() {
        report.push_str(&format!("Executable: {}\n", executable.display()));
    }
    report.push_str("\n[系统检查 / System checks]\n");
    report.push_str(&system_checks());
    append_settings(&mut report, &directory);
    append_logs(&mut report, &directory, "data directory");
    let fallback = tm_diagnostics::fallback_dir();
    if tm_diagnostics::log_path().is_some_and(|p| p.starts_with(&fallback)) && fallback != directory
    {
        append_logs(&mut report, &fallback, "temporary fallback");
    }
    report.push_str("\n[说明 / Notes]\n此报告不包含流量数据库、完整设置或网络规则缓存；日志可能含程序路径和错误上下文，发送前可以查看。仅保存在本机，不会自动上传。\nMissing / unavailable diagnostics are evidence, not a successful check. A missing helper log can mean the helper never started or ran under another Windows account.\n");
    let report = redact(&report);
    // TM_DATA_DIR/TM_DB_PATH development and test instances stay isolated.
    let destinations =
        if std::env::var_os("TM_DATA_DIR").is_some() || std::env::var_os("TM_DB_PATH").is_some() {
            vec![directory.join("diagnostics")]
        } else {
            vec![
                tm_store::default_export_dir(),
                directory.join("diagnostics"),
                fallback,
            ]
        };
    let path = save_report(&report, &destinations).map_err(|e| e.to_string())?;
    log::info!(target: "tramoing::diagnostics", "诊断报告已保存：{}", path.display());
    Ok(path)
}

fn append_settings(report: &mut String, directory: &Path) {
    report.push_str("\n[设置摘要 / Settings summary]\n");
    const KEYS: &[&str] = &[
        "theme",
        "lang",
        "tray_mode",
        "tray_badge",
        "performance_enabled",
        "performance_temperature",
        "performance_interval",
        "max_apps",
        "retention_days",
        "update_check",
    ];
    match read_tail(&directory.join("settings.conf"), 64 * 1024) {
        Ok(text) => {
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    if KEYS.contains(&key.trim())
                        && value.len() <= 32
                        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        report.push_str(&format!("{}={}\n", key.trim(), value));
                    }
                }
            }
        }
        Err(error) => report.push_str(&format!("Unavailable: {error}\n")),
    }
}

fn append_logs(report: &mut String, directory: &Path, label: &str) {
    for name in ["ui.log", "helper.log", "reporter.log"] {
        let path = directory.join(name);
        for index in (0..=tm_diagnostics::BACKUPS).rev() {
            let path = if index == 0 {
                path.clone()
            } else {
                tm_diagnostics::backup_path(&path, index)
            };
            if index != 0 && !path.exists() {
                continue;
            }
            report.push_str(&format!(
                "\n[{} / {label}]\n",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            match read_tail(&path, LOG_TAIL) {
                Ok(text) => report.push_str(&text),
                Err(error) => report.push_str(&format!("Unavailable: {error}\n")),
            }
        }
    }
}

fn read_tail(path: &Path, limit: u64) -> io::Result<String> {
    // Do not follow a log replaced by a link to unrelated user files.
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(limit)))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    if length > limit {
        Ok(format!(
            "[earlier content omitted]\n{}",
            text.split_once('\n').map_or("", |(_, rest)| rest)
        ))
    } else {
        Ok(text.into_owned())
    }
}

fn redact(text: &str) -> String {
    let mut text = text.to_owned();
    let mut homes: Vec<_> = ["USERPROFILE", "HOME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .filter(|home| home.len() > 1)
        .collect();
    homes.sort_by_key(|home| std::cmp::Reverse(home.len()));
    for home in homes {
        text = text.replace(&home, "<HOME>");
    }
    // Strip credentials/query strings from URLs embedded in transport errors.
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            let start = word.find("https://").or_else(|| word.find("http://"));
            let Some(start) = start else {
                return word.to_owned();
            };
            let trailing = &word[word.trim_end().len()..];
            let Ok(mut url) = url::Url::parse(word[start..].trim_end()) else {
                return word.to_owned();
            };
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            format!("{}{}{trailing}", &word[..start], url)
        })
        .collect()
}

fn save_report(report: &str, destinations: &[PathBuf]) -> io::Result<PathBuf> {
    let stamp = tm_diagnostics::timestamp().replace([':', '-'], "");
    let mut last = io::Error::other("no output directory");
    for directory in destinations {
        let attempt = || -> io::Result<PathBuf> {
            fs::create_dir_all(directory)?;
            let mut file = tempfile::Builder::new()
                .prefix(&format!("Tramoing-diagnostics-{stamp}-"))
                .suffix(".txt")
                .tempfile_in(directory)?;
            // BOM makes UTF-8 readable in Windows Notepad and PowerShell 5.1.
            file.write_all(b"\xef\xbb\xbf")?;
            file.write_all(report.as_bytes())?;
            file.as_file().sync_all()?;
            let (_, path) = file.keep().map_err(|e| e.error)?;
            Ok(path)
        };
        match attempt() {
            Ok(path) => return Ok(path),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// No pipes to deadlock on, bounded output read, and a deadline for system tools.
fn capture(command: &mut Command, timeout: Duration) -> String {
    let mut result = || -> io::Result<String> {
        let output = tempfile::tempfile()?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .spawn()?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break format!("exit={status}"),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(40))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break match result {
                        Err(error) => format!("wait failed: {error}"),
                        _ => "timed out; checks incomplete".into(),
                    };
                }
            }
        };
        let mut output = output;
        output.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        output.take(COMMAND_LIMIT).read_to_end(&mut bytes)?;
        Ok(format!("{status}\n{}\n", String::from_utf8_lossy(&bytes)))
    };
    result().unwrap_or_else(|error| format!("Unavailable: {error}\n"))
}

fn system_checks() -> String {
    #[cfg(windows)]
    {
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut command =
            Command::new(system.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            include_str!("../../../scripts/collect-windows-diagnostics.ps1"),
        ]);
        if let Ok(executable) = std::env::current_exe() {
            command.env("TRAMOING_DIAGNOSTIC_EXE", executable);
        }
        capture(&mut command, Duration::from_secs(20))
    }
    #[cfg(target_os = "macos")]
    {
        capture(
            &mut Command::new("/usr/bin/sw_vers"),
            Duration::from_secs(3),
        )
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        "Unsupported platform\n".into()
    }
}

pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let result = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
    #[cfg(windows)]
    let result = Command::new("explorer.exe")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    return Err("Unsupported platform".into());
    #[cfg(any(windows, target_os = "macos"))]
    result.map(|_| ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_reads_only_bounded_logs_and_safe_settings() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("ui.log"),
            "old\nERROR pipe failed: Win32 2\n",
        )
        .unwrap();
        fs::write(
            root.path().join("settings.conf"),
            "theme=dark\npinned_apps=secret.exe\ntoken=secret\nmax_apps=10\n",
        )
        .unwrap();
        fs::write(root.path().join("history.db"), "private traffic").unwrap();
        let mut report = String::new();
        append_logs(&mut report, root.path(), "test");
        append_settings(&mut report, root.path());
        assert!(report.contains("Win32 2"));
        assert!(report.contains("theme=dark"));
        assert!(report.contains("Unavailable:"));
        assert!(!report.contains("secret"));
        assert!(!report.contains("private traffic"));
        let tail = read_tail(&root.path().join("ui.log"), 26).unwrap();
        assert!(tail.contains("earlier content omitted"));
        assert!(!tail.contains("old\n"));
    }

    #[test]
    fn exporting_preserves_older_reports_and_falls_back_to_a_writable_directory() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("file");
        fs::write(&blocked, "keep").unwrap();
        let dirs = [blocked.clone(), root.path().join("reports")];
        let first = save_report("first", &dirs).unwrap();
        let second = save_report("second", &dirs).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(first).unwrap(), "\u{feff}first");
        assert_eq!(fs::read_to_string(blocked).unwrap(), "keep");
    }

    #[test]
    fn report_removes_url_credentials_and_query_secrets() {
        assert_eq!(
            redact("failed https://user:password@example.com/update?token=secret#fragment\n"),
            "failed https://example.com/update\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_probe_timeout_returns_a_report_instead_of_hanging() {
        let start = Instant::now();
        let text = capture(
            Command::new("/bin/sleep").arg("10"),
            Duration::from_millis(100),
        );
        assert!(text.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
