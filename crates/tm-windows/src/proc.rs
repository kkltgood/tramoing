//! 把 pid 解析成可执行文件路径。
//!
//! 和 macOS 的 bundle 归并不同，Windows 直接用 exe 全路径当归并键：
//! Chrome 的多进程共享同一个 `chrome.exe` 路径，天然就合并成一行。
//!
//! 这里跑在**提权的助手进程**里，所以能给系统进程也查到路径；
//! 普通权限的界面进程查 `svchost.exe` 这类会拿不到。

use std::collections::HashMap;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 查一次路径。失败返回 `None`（进程已退出、或权限不够）。
pub fn executable_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: 只申请查询权限，拿到句柄后立即关闭；缓冲区放在栈上，长度如实传入。
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }

        // MAX_PATH 是 260，长路径可能到 32767；先用 512 试，不够再放大到 1024
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);

        if ok == 0 || size == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// 带缓存的路径解析器。
///
/// 每秒几百个进程的路径如果每次都 syscall，光这一项就能吃掉可观 CPU。
/// 进程运行期间路径不会变，所以按 pid 缓存一次即可。
#[derive(Debug, Default)]
pub struct PathCache {
    map: HashMap<u32, String>,
}

impl PathCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析失败时返回 `pid N` 这种占位名，绝不返回空串 ——
    /// 否则界面上一堆没名字的行，看起来像程序坏了。
    pub fn resolve(&mut self, pid: u32) -> String {
        if let Some(p) = self.map.get(&pid) {
            return p.clone();
        }
        let path = executable_path(pid).unwrap_or_else(|| format!("pid {pid}"));
        self.map.insert(pid, path.clone());
        path
    }

    pub fn retain_pids(&mut self, alive: &[u32]) {
        self.map.retain(|pid, _| alive.contains(pid));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_pid_gets_placeholder() {
        let mut c = PathCache::new();
        let name = c.resolve(999_999);
        assert!(name.starts_with("pid "), "得到 {name}");
        // 第二次走缓存
        assert_eq!(c.resolve(999_999), name);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn retain_drops_dead_pids() {
        let mut c = PathCache::new();
        c.resolve(1);
        c.resolve(2);
        c.retain_pids(&[2]);
        assert_eq!(c.len(), 1);
    }
}
