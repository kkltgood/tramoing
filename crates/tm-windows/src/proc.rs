//! 把 pid 解析成可执行文件路径。
//!
//! 和 macOS 的 bundle 归并不同，Windows 直接用 exe 全路径当归并键：
//! Chrome 的多进程共享同一个 `chrome.exe` 路径，天然就合并成一行。
//!
//! 这里跑在**提权的助手进程**里，所以能给系统进程也查到路径；
//! 普通权限的界面进程查 `svchost.exe` 这类会拿不到。

use std::collections::{HashMap, HashSet};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 查一次路径。失败返回 `None`（进程已退出、或权限不够）。
pub fn executable_path(pid: u32) -> Option<String> {
    Process::open(pid)?.path()
}

struct Process {
    handle: HANDLE,
    created: u64,
}

impl Process {
    fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        // 创建时间与路径都通过同一个句柄查询，期间 PID 即使复用也不会串到另一个进程。
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut created: FILETIME = std::mem::zeroed();
            let mut exited: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
                CloseHandle(handle);
                return None;
            }
            Some(Self {
                handle,
                created: (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64,
            })
        }
    }

    fn path(&self) -> Option<String> {
        let mut short = [0u16; 512];
        match self.query_path(&mut short) {
            Ok(path) => Some(path),
            Err(ERROR_INSUFFICIENT_BUFFER) => self.query_path(&mut vec![0u16; 32768]).ok(),
            Err(_) => None,
        }
    }

    fn query_path(&self, buf: &mut [u16]) -> Result<String, u32> {
        let mut size = buf.len() as u32;
        // SAFETY: 句柄由 Self 持有，缓冲区长度按字符数传入。
        unsafe {
            if QueryFullProcessImageNameW(
                self.handle,
                PROCESS_NAME_WIN32,
                buf.as_mut_ptr(),
                &mut size,
            ) == 0
            {
                return Err(GetLastError());
            }
        }
        Ok(String::from_utf16_lossy(&buf[..size as usize]))
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// 带缓存的路径解析器。
///
/// 每秒几百个进程的路径如果每次都 syscall，光这一项就能吃掉可观 CPU。
/// 进程运行期间路径不会变，所以按 pid 缓存一次即可。
#[derive(Debug, Default)]
pub struct PathCache {
    map: HashMap<u32, (u64, String)>,
}

impl PathCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析失败时返回 `pid N` 这种占位名，绝不返回空串 ——
    /// 否则界面上一堆没名字的行，看起来像程序坏了。
    pub fn resolve(&mut self, pid: u32) -> String {
        // 每批验证创建时间，PID 被复用时不能沿用旧路径；退出后的最后事件
        // 可暂时使用缓存归属，下一批无事件时由 retain_pids 清理。
        if let Some(process) = Process::open(pid) {
            return self.resolve_identity(pid, process.created, || process.path());
        }
        if let Some((_, path)) = self.map.get(&pid) {
            return path.clone();
        }
        let path = format!("pid {pid}");
        self.map.insert(pid, (0, path.clone()));
        path
    }

    fn resolve_identity(
        &mut self,
        pid: u32,
        created: u64,
        load_path: impl FnOnce() -> Option<String>,
    ) -> String {
        if let Some((cached_created, path)) = self.map.get(&pid) {
            if *cached_created == created {
                return path.clone();
            }
        }
        // 只有首次出现或 PID 换了身份才查路径。新身份查询失败时不能沿用旧身份。
        let (identity, path) = match load_path() {
            Some(path) => (created, path),
            None => (0, format!("pid {pid}")), // 下次继续尝试，不永久缓存失败结果
        };
        self.map.insert(pid, (identity, path.clone()));
        path
    }

    pub fn retain_pids(&mut self, alive: &[u32]) {
        let alive: HashSet<_> = alive.iter().copied().collect();
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
    fn cached_identity_skips_path_query_but_pid_reuse_refreshes_it() {
        let mut cache = PathCache::new();
        assert_eq!(
            cache.resolve_identity(7, 100, || Some("a.exe".into())),
            "a.exe"
        );
        assert_eq!(
            cache.resolve_identity(7, 100, || panic!("cached path must not be queried again")),
            "a.exe"
        );
        assert_eq!(
            cache.resolve_identity(7, 200, || Some("b.exe".into())),
            "b.exe"
        );
    }

    #[test]
    fn a_reused_pid_never_inherits_old_path_when_query_fails() {
        let mut cache = PathCache::new();
        cache.resolve_identity(7, 100, || Some("old.exe".into()));
        assert_eq!(cache.resolve_identity(7, 200, || None), "pid 7");
        assert_eq!(
            cache.resolve_identity(7, 200, || Some("new.exe".into())),
            "new.exe"
        );
    }

    #[test]
    fn resolves_a_live_process_and_reuses_its_path() {
        let mut cache = PathCache::new();
        let pid = std::process::id();
        let expected = executable_path(pid).unwrap();
        assert_eq!(cache.resolve(pid), expected);
        assert_eq!(cache.resolve(pid), expected);
    }

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
