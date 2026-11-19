//! 使用 Shell 的提权启动流程，允许安装包正常弹出 UAC。
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn elevated(path: &Path) -> Result<(), String> {
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of_val(&info) as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            let code = GetLastError();
            return Err(if code == ERROR_CANCELLED {
                "已取消安装授权".into()
            } else {
                format!("启动安装包失败（Windows 错误 {code}）")
            });
        }
        if !info.hProcess.is_null() {
            CloseHandle(info.hProcess);
        }
    }
    Ok(())
}
