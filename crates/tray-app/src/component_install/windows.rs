use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED, WAIT_OBJECT_0};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

const INSTALL_FLAG: &str = "--install-sensor-components";
const UNSAFE_INSTALLATION: u32 = 20;

fn install_root() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Cannot locate the application installation".into())
}

fn powershell() -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("Cannot locate Windows PowerShell".into());
    }
    Ok(
        PathBuf::from(String::from_utf16(&buffer[..length]).map_err(|e| e.to_string())?)
            .join("WindowsPowerShell/v1.0/powershell.exe"),
    )
}

fn run_script(
    script: &str,
    data: &serde_json::Value,
    timeout: Option<Duration>,
) -> Result<std::process::Output, String> {
    let payload = STANDARD.encode(serde_json::to_vec(data).map_err(|e| e.to_string())?);
    let script = format!("$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $cfg=ConvertFrom-Json ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{payload}')));\n{}\n{script}", include_str!("security.ps1"));
    // Keep the Windows command line small. The embedded script/data travel over
    // a private pipe, not a script in a writable temporary directory.
    let bootstrap = "$ErrorActionPreference='Stop'; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); $source=[Console]::In.ReadToEnd(); try { & ([ScriptBlock]::Create($source)) } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }";
    let encoded = STANDARD.encode(
        bootstrap
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let executable = powershell()?;
    let output = tempfile::tempfile().map_err(|e| e.to_string())?;
    let errors = tempfile::tempfile().map_err(|e| e.to_string())?;
    let mut command = Command::new(&executable);
    tm_windows::temperature_service::clean_runtime_environment(&mut command);
    command.env("PSModulePath", executable.parent().unwrap().join("Modules"));
    let mut child = command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .creation_flags(0x08000000)
        .stdin(Stdio::piped())
        .stdout(output.try_clone().map_err(|e| e.to_string())?)
        .stderr(errors.try_clone().map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(error) = child
        .stdin
        .take()
        .ok_or("PowerShell input pipe unavailable")?
        .write_all(script.as_bytes())
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.to_string());
    }
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if timeout.is_some_and(|limit| start.elapsed() >= limit) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("依赖检测超时 / Component operation timed out".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    use std::io::{Seek, SeekFrom};
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut output = output;
    let mut errors = errors;
    output.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    errors.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    output
        .take(65536)
        .read_to_end(&mut stdout)
        .map_err(|e| e.to_string())?;
    errors
        .take(65536)
        .read_to_end(&mut stderr)
        .map_err(|e| e.to_string())?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

pub(super) fn inspect_system(config: &Configuration) -> Result<SystemState, String> {
    let data = serde_json::json!({ "root": install_root()?, "manifest": config });
    let output = run_script(
        include_str!("probe.ps1"),
        &data,
        Some(Duration::from_secs(25)),
    )?;
    if !output.status.success() {
        return Err(format!(
            "依赖检测失败 / Dependency check failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(700)
                .collect::<String>()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid component inspection: {e}"))
}

fn elevated_wait(path: &Path, arguments: &str) -> Result<u32, String> {
    let wide = |text: &OsStr| text.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let path = wide(path.as_os_str());
    let arguments = wide(OsStr::new(arguments));
    let verb = wide(OsStr::new("runas"));
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of_val(&info) as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = path.as_ptr();
        info.lpParameters = arguments.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            let code = GetLastError();
            return Err(if code == ERROR_CANCELLED {
                "已取消安装授权，可再次点击安装 / Installation authorization cancelled".into()
            } else {
                format!("Cannot start component installation (Windows {code})")
            });
        }
        if info.hProcess.is_null() {
            return Err("Installer process handle unavailable".into());
        }
        let wait = WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1;
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if wait != WAIT_OBJECT_0 || ok == 0 {
            return Err("Cannot obtain component installation result".into());
        }
        Ok(code)
    }
}

pub(super) fn install(prepared: &Prepared) -> Result<InstallOutcome, String> {
    let inspection = inspect()?;
    if !inspection.supported {
        return Err("Unsupported sensor platform".into());
    }
    if inspection.ready {
        return Ok(InstallOutcome {
            needs_restart: inspection.needs_restart,
        });
    }
    let root = prepared.root.canonicalize().map_err(|e| e.to_string())?;
    for item in &inspection.items {
        let path = crate::update::download::component_path(&item.version, &root.join(&item.id))?;
        let valid = std::fs::metadata(&path)
            .is_ok_and(|meta| meta.is_file() && meta.len() == item.asset.size)
            && crate::update::sha256_file(&path).is_ok_and(|hash| hash == item.asset.sha256);
        if !valid {
            return Err(format!("{} 的下载文件缺失或已损坏，请重新检查并下载 / Cached installer is missing or damaged; recheck and download", item.name));
        }
    }
    // The only caller-controlled value is a cache directory. The elevated binary
    // independently reloads embedded hashes and checks the machine again.
    let encoded = STANDARD.encode(
        root.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let code = elevated_wait(&executable, &format!("{INSTALL_FLAG} {encoded}"))?;
    if code == UNSAFE_INSTALLATION {
        return Err("安装目录权限不安全，请将 Tramoing 重新安装到系统 Program Files 后重试 / Reinstall Tramoing in the protected Program Files directory".into());
    }
    if ![0, 3010].contains(&code) {
        let detail = std::fs::read_to_string(install_root()?.join("component-install.log"))
            .unwrap_or_default();
        return Err(format!(
            "组件安装失败，可重试 / Component installation failed ({code}): {}",
            detail
                .chars()
                .rev()
                .take(1200)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    let _ = tm_windows::temperature_service::refresh();
    let state = inspect()?;
    if !state.ready {
        return Err("组件安装后验证失败，请重新检查并重试 / Component verification failed after installation".into());
    }
    Ok(InstallOutcome {
        needs_restart: code == 3010 || state.needs_restart,
    })
}

pub fn handle_install_command() -> Option<i32> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(OsStr::new(INSTALL_FLAG)) {
        return None;
    }
    let result = (|| -> Result<i32, String> {
        let argument = arguments
            .next()
            .and_then(|s| s.into_string().ok())
            .ok_or("Missing component cache")?;
        if arguments.next().is_some() || argument.len() > 90000 {
            return Err("Invalid component arguments".into());
        }
        let bytes = STANDARD.decode(argument).map_err(|e| e.to_string())?;
        if bytes.len() % 2 != 0 {
            return Err("Invalid component cache path".into());
        }
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if units.contains(&0) {
            return Err("Invalid component cache path".into());
        }
        use std::os::windows::ffi::OsStringExt;
        let cache = PathBuf::from(std::ffi::OsString::from_wide(&units));
        if !cache.is_absolute() || !tm_windows::temperature_service::supported_platform() {
            return Err("Unsupported component installation".into());
        }
        let config = configuration()?;
        let checked = run_script(
            "Assert-InstallRoot $cfg.root",
            &serde_json::json!({"root": install_root()?}),
            Some(Duration::from_secs(20)),
        )?;
        if !checked.status.success() {
            return Ok(UNSAFE_INSTALLATION as i32);
        }
        let inspection = plan(&config, &inspect_system(&config)?, true)?;
        if inspection.ready {
            return Ok(if inspection.needs_restart { 3010 } else { 0 });
        }
        let files: Vec<_> = inspection.items.iter().map(|item| {
            let path = crate::update::download::component_path(&item.version, &cache.join(&item.id))?;
            let publisher = config.dependencies.iter().find(|d| d.id == item.id).map(|d| d.publisher.as_str());
            Ok(serde_json::json!({ "id":item.id, "path":path, "size":item.asset.size, "sha256":item.asset.sha256, "publisher":publisher }))
        }).collect::<Result<_,String>>()?;
        let data = serde_json::json!({ "root": install_root()?, "files": files });
        // Wait for official installers to finish; never kill an in-flight system
        // installation or remove its staging files because of a UI timeout.
        let output = run_script(include_str!("install.ps1"), &data, None)?;
        Ok(output.status.code().unwrap_or(1))
    })();
    match result {
        Ok(code) => Some(code),
        Err(error) => {
            eprintln!("{error}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_powershell_accepts_large_unicode_payload_without_command_line_quoting() {
        let text = "路径 with spaces ' $ ` \" ".repeat(1000);
        let value = serde_json::json!({"text": text});
        let output = run_script(
            "$cfg.text | ConvertTo-Json -Compress",
            &value,
            Some(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: String = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(actual, text);
    }

    #[test]
    fn powershell_failures_are_returned_without_claiming_success() {
        let output = run_script(
            "throw 'component-test-error'",
            &serde_json::json!({}),
            Some(Duration::from_secs(20)),
        )
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("component-test-error"));
    }
}
