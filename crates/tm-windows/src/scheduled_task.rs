//! 常驻采集助手的计划任务。显式配置电池策略和运行时限，避免系统默认值停止采集。
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, CREATE_NO_WINDOW,
};

pub const TASK_NAME: &str = "TrafficMonitorHelper";

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn definition(executable: &str, user_sid: &str) -> String {
    let executable = xml_escape(executable);
    let user_sid = xml_escape(user_sid);
    // 1.2 schema 在 Win10/Win11 均可用；仅本账户登录时启动，不改变现有权限模型。
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user_sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Collector">
    <UserId>{user_sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel>
  </Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Collector"><Exec><Command>{executable}</Command></Exec></Actions>
</Task>"#
    )
}

fn write_definition(writer: &mut impl Write, xml: &str) -> std::io::Result<()> {
    writer.write_all(&[0xff, 0xfe])?;
    for unit in xml.encode_utf16() {
        writer.write_all(&unit.to_le_bytes())?;
    }
    writer.flush()
}

fn current_user_sid() -> Result<String, String> {
    // SID 来自当前进程令牌，避免本地化账户名、域账户和环境变量造成注册到错误账户。
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!(
                "读取账户令牌失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut bytes = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
        if bytes == 0 {
            return Err(format!(
                "读取账户 SID 失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        // TOKEN_USER 含指针，缓冲区按 usize 对齐。
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            return Err(format!(
                "读取账户 SID 失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(format!(
                "转换账户 SID 失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(sid)
    }
}

fn check_output(action: &str, output: std::io::Result<Output>) -> Result<(), String> {
    let output = output.map_err(|error| format!("{action}失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{action}失败（退出码 {:?}）：{} {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    ))
}

/// 更新同名任务并请求立即启动；任何阶段失败均传回安装器，不先删除可用的旧任务。
pub fn install(executable: &Path) -> Result<(), String> {
    let sid = current_user_sid()?;
    let xml = definition(&executable.to_string_lossy(), &sid);
    let mut file = tempfile::Builder::new()
        .prefix("tramoing-task-")
        .suffix(".xml")
        .tempfile()
        .map_err(|error| format!("创建任务定义失败：{error}"))?;
    write_definition(&mut file, &xml).map_err(|error| format!("写入任务定义失败：{error}"))?;
    // Windows 的临时文件句柄带删除共享标记；先关闭句柄，schtasks 才能正常读取。
    let path = file.into_temp_path();
    check_output(
        "注册采集助手任务",
        Command::new("schtasks.exe")
            .args(["/Create", "/TN", TASK_NAME, "/XML"])
            .arg(&path)
            .arg("/F")
            .creation_flags(CREATE_NO_WINDOW)
            .output(),
    )?;
    check_output(
        "启动采集助手任务",
        Command::new("schtasks.exe")
            .args(["/Run", "/TN", TASK_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_keeps_collecting_on_battery_and_after_three_days() {
        let xml = definition(
            r#"C:\测试 & apps\Tramoing\tramoing-helper.exe"#,
            "S-1-5-21-123",
        );
        assert!(xml.contains(r#"C:\测试 &amp; apps\Tramoing\tramoing-helper.exe"#));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert_eq!(xml.matches("<UserId>S-1-5-21-123</UserId>").count(), 2);
    }

    #[test]
    fn task_launch_failure_is_not_reported_as_success() {
        use std::os::windows::process::ExitStatusExt;
        let output = Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: b"Access is denied".to_vec(),
        };
        let error = check_output("启动采集助手任务", Ok(output)).unwrap_err();
        assert!(error.contains("启动采集助手任务失败"));
        assert!(error.contains("Access is denied"));
    }

    #[test]
    #[ignore = "requires Windows Task Scheduler and PowerShell; validates only, never registers"]
    fn task_definition_passes_windows_scheduler_validation() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
        let xml = definition(
            r"C:\Program Files\测试 & Tramoing\tramoing-helper.exe",
            &sid,
        );
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_definition(&mut file, &xml).unwrap();
        let path = file.into_temp_path();
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new(); $service=New-Object -ComObject Schedule.Service; $service.Connect(); $xml=[IO.File]::ReadAllText($env:TRAMOING_TEST_TASK_XML); $null=$service.GetFolder('\').RegisterTask('Tramoing-Validate-Only', $xml, 1, $null, $null, 3, $null)",
            ])
            .env("TRAMOING_TEST_TASK_XML", &path)
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        check_output("验证任务 XML", output).unwrap();
    }
}
