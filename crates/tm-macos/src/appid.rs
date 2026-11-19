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
//! 没有 `.app` 包裹的系统进程（`/usr/sbin/mDNSResponder`）直接拿可执行路径当键，
//! 读不到路径的（别的用户起的进程）才退化成进程名。

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
/// 而不是退化成 `pid N`。典型场景是**别的用户起的进程**（root 跑的代理内核）：
/// 读不到路径时，列表里显示 `verge-mihomo` 比 `pid 2043` 有用得多。
/// 解析全失败时才写 `pid N`。
///
/// 没有 `.app` 外壳时**用完整路径当键**，而不是进程名：图标层要靠路径前缀认出
/// `/usr/sbin/mDNSResponder` 这类系统守护进程（见 `tm_core::is_system_service`），
/// 键退成名字这层信息就没了。按路径归并也比按名字准 —— 两个不同来源的 `node`
/// 本来就是两件事，不该合并成一行。
pub fn identify(pid: u32, fallback: &str) -> AppIdentity {
    let path = procname::executable_path(pid);

    if let Some(bundle) = path.as_deref().and_then(bundle_of) {
        return AppIdentity::new(bundle, pretty_name(bundle));
    }

    // nettop 把名字截到约 20 字符，有路径就用路径里的文件名（不受截断影响）
    let name = match path.as_deref() {
        Some(p) => pretty_name(p),
        None => procname::process_name(pid)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let f = fallback.trim().trim_end_matches('.').trim();
                (!f.is_empty()).then(|| f.to_owned())
            })
            .unwrap_or_else(|| format!("pid {pid}")),
    };
    let key = path.unwrap_or_else(|| name.clone());
    AppIdentity::new(key, name)
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
    fn identify_keys_on_path_when_there_is_no_bundle() {
        // 自己的路径一定读得到，且它不在任何 .app 里
        let id = identify(std::process::id(), "ignored");
        assert!(!id.key.ends_with(".app"), "测试进程不该被认成 app：{}", id.key);
        assert!(id.key.starts_with('/'), "无 bundle 时键应是完整路径：{}", id.key);
        assert_eq!(id.name, pretty_name(&id.key), "展示名取路径里的文件名");
        // 路径是图标层的判断依据，必须能从中认出系统目录
        assert!(!tm_core::is_system_service(&id.key));
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
