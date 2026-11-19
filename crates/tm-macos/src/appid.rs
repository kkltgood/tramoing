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

/// 解析单个 pid 的应用身份。
///
/// `fallback` 是 nettop 那一行给的进程名 —— 解析不到可执行路径时用它，
/// 而不是退化成 `pid N`。典型场景是 **root 起的代理进程**（verge-mihomo / ClashX）：
/// 别的用户的进程读不到 `proc_pidpath`，但 nettop 自己知道它叫什么，
/// 列表里显示 `verge-mihomo` 比 `pid 2043` 有用得多。解析全失败时才写 `pid N`。
pub fn identify(pid: u32, fallback: &str) -> AppIdentity {
    if let Some(bundle) = procname::executable_path(pid)
        .as_deref()
        .and_then(bundle_of)
    {
        return AppIdentity::new(bundle, pretty_name(bundle));
    }
    // nettop 把名字截到约 20 字符，能取到全名就用全名
    let name = procname::process_name(pid)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let f = fallback.trim().trim_end_matches('.').trim();
            (!f.is_empty()).then(|| f.to_owned())
        })
        .unwrap_or_else(|| format!("pid {pid}"));
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

    /// 解析 pid。`fallback` 取自 nettop 输出里的进程名，只在 syscall 拿不到身份时兜底。
    pub fn resolve(&mut self, pid: u32, fallback: &str) -> AppIdentity {
        if let Some(id) = self.map.get(&pid) {
            return id.clone();
        }
        let id = identify(pid, fallback);
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
    fn identify_falls_back_to_nettop_name_for_foreign_process() {
        // 别的用户的进程读不到路径和全名（root 起的代理就是这样），
        // 此时必须用 nettop 给的名字，而不是显示成 `pid N`
        let id = identify(999_999, "verge-mihomo");
        assert_eq!(id.name, "verge-mihomo");
        assert_eq!(id.key, "verge-mihomo");
    }

    #[test]
    fn identify_uses_pid_only_when_everything_fails() {
        let id = identify(999_999, "  ");
        assert_eq!(id.name, "pid 999999");
    }

    #[test]
    fn resolver_caches_by_pid() {
        let mut r = AppResolver::new();
        let a = r.resolve(999_999, "verge-mihomo");
        let b = r.resolve(999_999, "ignored");
        assert_eq!(a, b);
        assert_eq!(r.len(), 1);

        r.retain_only(&[]);
        assert!(r.is_empty(), "进程退出后缓存应被清理");
    }
}
