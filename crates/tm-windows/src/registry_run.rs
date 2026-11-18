//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 里的自启项。
//!
//! 为什么不用 `reg.exe`：GUI 进程 spawn 控制台程序会闪一下黑框；更要紧的是
//! 用户环境里 reg.exe 可能被组策略 / 安全软件禁掉，而读写自启状态发生在
//! `App::new()` 里 —— 那会让整个程序起不来。直接调 advapi32 没有这些问题。

use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

/// 子键路径。**不带** `HKCU\` 前缀：根键是单独传的。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// ERROR_FILE_NOT_FOUND：要删的值本来就不存在，按成功处理
const ERROR_FILE_NOT_FOUND: u32 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 这个值名在 Run 键下是否存在。
pub fn value_exists(name: &str) -> bool {
    let subkey = wide(RUN_KEY);
    let value = wide(name);
    // SAFETY: 两个指针都指向有效的、以 NUL 结尾的宽字符串；
    // 不取数据（pvdata = null）只探存在性，pcbdata 也允许为 null。
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    rc == 0
}

/// 写入（`on = true`）或删除（`on = false`）自启项。
///
/// 写入的是带引号的可执行文件全路径 —— 路径里有空格时，
/// 登录时会被拆成两个参数。
pub fn set_value(name: &str, on: bool, exe: &str) -> Result<(), u32> {
    let subkey = wide(RUN_KEY);
    let value = wide(name);

    // SAFETY: 同上；写入时 data 的生命周期覆盖整个调用，长度按字节数传。
    let rc = unsafe {
        if on {
            let quoted = format!("\"{exe}\"");
            let data = wide(&quoted);
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                data.as_ptr() as *const core::ffi::c_void,
                (data.len() * std::mem::size_of::<u16>()) as u32,
            )
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), value.as_ptr())
        }
    };

    match rc {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        code => Err(code),
    }
}
