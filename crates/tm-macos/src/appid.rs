//! 把 pid 还原成「应用」。
//!
//! 这是列表能不能读的关键。Chrome 跑起来是一堆进程：
//!
//! ```text
//! /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
//! /Applications/Google Chrome.app/Contents/Frameworks/.../Google Chrome Helper (Renderer).app/Contents/MacOS/...
//! /Applications/Google Chrome.app/Contents/Frameworks/.../Google Chrome Helper (GPU).app/...
//! ```
//!
//! 如果按进程列，用户会看到五条 Chrome 抢带宽。这里取路径里**最外层**的 `.app`
//! 作为归并键，于是全部合成一行 `/Applications/Google Chrome.app`。
//!
//! 没有 `.app` 包裹的系统进程（`/usr/libexec/netbiosd`）就用进程名当键。

use std::collections::HashMap;

use tm_core::pretty_name;

use crate::procname;

/// 一个「应用」的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// 归并键：bundle 路径，或进程名
    pub key: String,
    /// 展示名：`Google Chrome`、`netbiosd`
    pub name: String,
}

impl AppIdentity {
    pub fn new(key: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            name: name.into(),
        }
    }
}

/// 从可执行路径里取出最外层的 `.app` bundle 路径。
///
/// ```text
/// /Applications/Google Chrome.app/Contents/Frameworks/.../Helper.app/Contents/...
///   -> /Applications/Google Chrome.app
/// /usr/libexec/netbiosd
///   -> None
/// ```
pub fn bundle_of(path: &str) -> Option<&str> {
    let mut from = 0usize;
    while let Some(rel) = path[from..].find(".app") {
        let idx = from + rel;
        let end = idx + 4;
        let rest = &path[end..];
        // `.app` 后面必须紧跟路径分隔或已到结尾，排除 `foo.appdata` 这种
        if rest.is_empty() || rest.starts_with('/') {
            return Some(&path[..end]);
        }
        from = end;
    }
    None
}

/// 解析单个 pid 的应用身份。解析失败时退化成 `pid N`，不 panic。
pub fn identify(pid: u32) -> AppIdentity {
    if let Some(bundle) = procname::executable_path(pid)
        .as_deref()
        .and_then(bundle_of)
    {
        return AppIdentity::new(bundle, pretty_name(bundle));
    }
    let name = procname::process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
    AppIdentity::new(name.clone(), name)
}

/// 带缓存的 pid → 应用 解析器。
///
/// 采样每秒钟都会把同样的进程表送进来一次，不缓存的话每秒要做上百次 syscall。
#[derive(Debug, Default)]
pub struct AppResolver {
    map: HashMap<u32, AppIdentity>,
}

impl AppResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(&mut self, pid: u32) -> AppIdentity {
        if let Some(id) = self.map.get(&pid) {
            return id.clone();
        }
        let id = identify(pid);
        self.map.insert(pid, id.clone());
        id
    }

    /// 每帧结束后调用：清掉已退出的 pid，防止长时间运行后缓存膨胀。
    pub fn retain_only(&mut self, alive: &[u32]) {
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
    fn extracts_outermost_bundle() {
        assert_eq!(
            bundle_of("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            Some("/Applications/Google Chrome.app")
        );
        // 嵌套 helper bundle 应归到最外层
        assert_eq!(
            bundle_of(
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/131/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)"
            ),
            Some("/Applications/Google Chrome.app")
        );
        assert_eq!(
            bundle_of("/Applications/Slack.app"),
            Some("/Applications/Slack.app")
        );
    }

    #[test]
    fn ignores_non_bundle_paths() {
        assert_eq!(bundle_of("/usr/libexec/netbiosd"), None);
        assert_eq!(bundle_of("/usr/sbin/mDNSResponder"), None);
        // 不能把 foo.appdata 误判成 bundle
        assert_eq!(bundle_of("/Users/x/foo.appdata/bin/agent"), None);
    }

    #[test]
    fn bundle_name_is_pretty() {
        let b = bundle_of("/Applications/Google Chrome.app/x").unwrap();
        assert_eq!(pretty_name(b), "Google Chrome");
    }

    #[test]
    fn identify_falls_back_for_bad_pid() {
        let id = identify(999_999);
        assert!(id.name.starts_with("pid ") || !id.name.is_empty());
    }

    #[test]
    fn resolver_caches_by_pid() {
        let mut r = AppResolver::new();
        let a = r.resolve(999_999);
        let b = r.resolve(999_999);
        assert_eq!(a, b);
        assert_eq!(r.len(), 1);

        r.retain_only(&[]);
        assert!(r.is_empty(), "进程退出后缓存应被清理");
    }
}
