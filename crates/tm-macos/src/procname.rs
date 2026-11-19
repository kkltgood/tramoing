//! 按 pid 取进程全名。
//!
//! nettop 会把进程名截断到约 20 字符（`Google Chrome H.`、`Codex (Aperitif`），
//! 直接拿来展示既难读又容易重名。这里走 `libproc` 的 `proc_name(3)` 拿全名。
//!
//! 不引入 `libc` crate：`proc_name` 就在 libSystem 里，直接声明 FFI 即可，
//! 少一个依赖也少一份编译时间。取不到（进程已退出、权限受限）时返回 `None`，
//! 调用方回落到 nettop 给的名字，不影响主流程。

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    /// `int proc_name(int pid, void * buffer, uint32_t buffersize)`
    /// 返回值 <= 0 表示失败。
    fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;

    /// `int proc_pidpath(int pid, void * buffer, uint32_t buffersize)`
    /// 返回路径长度，<= 0 表示失败。需要 `PROC_PIDPATHINFO_MAXSIZE` 大小的缓冲区。
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
}

/// `PROC_PIDPATHINFO_MAXSIZE`
const PATH_BUF_LEN: usize = 4096;

/// `MAXCOMLEN` 的两倍够放全名，这里给足余量。
const BUF_LEN: usize = 256;

/// 取进程可执行名（不含路径）。失败返回 `None`。
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let mut buf = [0u8; BUF_LEN];
    // SAFETY: buf 是本栈上的连续内存，长度如实传入；proc_name 只会写入不超过该长度的字节。
    let n = unsafe {
        proc_name(
            pid as c_int,
            buf.as_mut_ptr() as *mut c_void,
            BUF_LEN as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    // SAFETY: proc_name 成功时保证写入以 NUL 结尾的字符串。
    let name = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
        .to_string_lossy()
        .trim()
        .to_owned();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 取进程可执行文件的完整路径，例如
/// `/Applications/Google Chrome.app/Contents/Frameworks/.../Google Chrome Helper (Renderer)`。
/// 失败返回 `None`（进程已退出，或调用了不属于当前用户的进程）。
pub fn executable_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let mut buf = [0u8; PATH_BUF_LEN];
    // SAFETY: buf 是连续栈内存，长度如实传入；proc_pidpath 不会写入超过该长度的字节。
    let n = unsafe {
        proc_pidpath(
            pid as c_int,
            buf.as_mut_ptr() as *mut c_void,
            PATH_BUF_LEN as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    // SAFETY: 成功时写入以 NUL 结尾的字符串。
    let path = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
        .to_string_lossy()
        .trim()
        .to_owned();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// 带缓存的解析器。进程表每帧都会重复出现，缓存能把 syscall 次数从
/// 「每帧 × 每进程」压到「每个进程一生一次」。
#[derive(Debug, Default)]
pub struct NameCache {
    map: std::collections::HashMap<u32, String>,
}

impl NameCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 查缓存，未命中则解析；解析失败时用 `fallback`（nettop 给的截断名）兜底。
    pub fn resolve(&mut self, pid: u32, fallback: &str) -> String {
        if let Some(name) = self.map.get(&pid) {
            return name.clone();
        }
        let name = process_name(pid).unwrap_or_else(|| fallback.to_owned());
        self.map.insert(pid, name.clone());
        name
    }

    /// 清掉已经不在进程表里的 pid，避免长时间运行后缓存无限增长。
    pub fn retain_only(&mut self, alive: &[u32]) {
        self.map.retain(|pid, _| alive.contains(pid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_own_process_name() {
        // 这条能过，说明 FFI 声明和调用约定是对的
        let name = process_name(std::process::id());
        println!("自身进程名 = {name:?}");
        assert!(name.is_some(), "至少应能解析自己");
    }

    #[test]
    fn invalid_pid_returns_none() {
        assert!(process_name(0).is_none());
        // 一个几乎肯定不存在的 pid
        assert!(process_name(999_999).is_none());
    }

    #[test]
    fn cache_falls_back_when_resolution_fails() {
        let mut cache = NameCache::new();
        let name = cache.resolve(999_999, "Google Chrome H");
        assert_eq!(name, "Google Chrome H");
        // 第二次走缓存
        assert_eq!(cache.resolve(999_999, "ignored"), "Google Chrome H");
    }

    #[test]
    fn resolves_own_executable_path() {
        let path = executable_path(std::process::id());
        println!("自身可执行路径 = {path:?}");
        let path = path.expect("至少应能解析自己的路径");
        assert!(std::path::Path::new(&path).exists(), "{path} 应真实存在");
    }
}
