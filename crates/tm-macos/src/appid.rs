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

use std::collections::{HashMap, HashSet};

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
    map: HashMap<u32, CachedIdentity>,
}

#[derive(Debug)]
struct CachedIdentity {
    started: Option<(u64, u64)>,
    fallback: String,
    identity: AppIdentity,
}

impl AppResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析 pid。`fallback` 取自 nettop 输出里的进程名，只在 syscall 拿不到身份时兜底。
    pub fn resolve(&mut self, pid: u32, fallback: &str) -> AppIdentity {
        self.resolve_identity(pid, procname::start_time(pid), fallback, || {
            identify(pid, fallback)
        })
    }

    /// Resolve an old socket row only against a process generation that already
    /// existed when that snapshot arrived. Do not ask a reused PID for its path.
    pub(crate) fn resolve_snapshot(
        &mut self,
        pid: u32,
        fallback: &str,
        cutoff: (u64, u64),
    ) -> Option<AppIdentity> {
        self.resolve_snapshot_with(
            pid,
            fallback,
            cutoff,
            || procname::start_time(pid),
            || identify(pid, fallback),
        )
    }

    pub(crate) fn cached_snapshot(
        &self,
        pid: u32,
        fallback: &str,
        cutoff: (u64, u64),
    ) -> Option<AppIdentity> {
        let cached = self
            .map
            .get(&pid)
            .filter(|cached| cached.fallback == fallback)?;
        match cached.started {
            Some(at) if at <= cutoff => Some(cached.identity.clone()),
            Some(_) => None,
            None => Self::snapshot_fallback(fallback),
        }
    }

    fn snapshot_fallback(fallback: &str) -> Option<AppIdentity> {
        let name = fallback.trim().trim_end_matches('.').trim();
        (!name.is_empty()).then(|| AppIdentity::new(name, name))
    }

    fn resolve_snapshot_with(
        &mut self,
        pid: u32,
        fallback: &str,
        cutoff: (u64, u64),
        mut start_time: impl FnMut() -> Option<(u64, u64)>,
        load: impl FnOnce() -> AppIdentity,
    ) -> Option<AppIdentity> {
        let started = start_time();
        if started.is_some_and(|at| at > cutoff) {
            return None;
        }
        if started.is_none() {
            // An exited/protected process can still be shown under its known
            // snapshot identity, or the nettop-supplied name. Never query its
            // current path/name without a verifiable process generation.
            if let Some(cached) = self.map.get(&pid).filter(|cached| {
                cached.fallback == fallback && cached.started.is_some_and(|at| at <= cutoff)
            }) {
                return Some(cached.identity.clone());
            }
            return Self::snapshot_fallback(fallback);
        }
        let identity = self.resolve_identity(pid, started, fallback, load);
        // proc_pidpath has no process-handle variant. Check the generation again
        // so a PID reused during path resolution cannot escape the cutoff check.
        if start_time() == started {
            Some(identity)
        } else {
            self.map.remove(&pid);
            None
        }
    }

    fn resolve_identity(
        &mut self,
        pid: u32,
        started: Option<(u64, u64)>,
        fallback: &str,
        load: impl FnOnce() -> AppIdentity,
    ) -> AppIdentity {
        if let Some(cached) = self.map.get(&pid) {
            if cached.started.is_some()
                && (started == cached.started || started.is_none())
                && cached.fallback == fallback
                && cached.identity.key.starts_with('/')
            {
                // nettop 的最后一帧可能来自刚退出的进程，此时保留已知归属。
                return cached.identity.clone();
            }
        }
        let id = load();
        self.map.insert(
            pid,
            CachedIdentity {
                started,
                fallback: fallback.to_owned(),
                identity: id.clone(),
            },
        );
        id
    }

    /// 每帧结束后调用：清掉已退出的 pid，防止长时间运行后缓存膨胀。
    pub fn retain_only(&mut self, alive: &[u32]) {
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
    fn snapshot_identity_rejects_reused_pids_before_and_during_path_lookup() {
        let mut resolver = AppResolver::new();
        let cutoff = (100, 500);
        assert!(resolver
            .resolve_snapshot_with(
                42,
                "app",
                cutoff,
                || Some((101, 0)),
                || { panic!("a newer PID must not be resolved") }
            )
            .is_none());
        let mut generations = [Some((90, 0)), Some((102, 0))].into_iter();
        assert!(resolver
            .resolve_snapshot_with(
                42,
                "app",
                cutoff,
                || generations.next().unwrap(),
                || { AppIdentity::new("/new/app", "app") }
            )
            .is_none());
        assert_eq!(
            resolver
                .resolve_snapshot_with(
                    43,
                    "old",
                    cutoff,
                    || Some((90, 0)),
                    || { AppIdentity::new("/old/app", "old") }
                )
                .unwrap()
                .key,
            "/old/app"
        );
        assert_eq!(
            resolver
                .resolve_snapshot_with(
                    44,
                    "fixture",
                    cutoff,
                    || None,
                    || { panic!("unknown generation must use only the supplied fallback") }
                )
                .unwrap()
                .key,
            "fixture"
        );
    }

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
        assert!(
            !id.key.ends_with(".app"),
            "测试进程不该被认成 app：{}",
            id.key
        );
        assert!(
            id.key.starts_with('/'),
            "无 bundle 时键应是完整路径：{}",
            id.key
        );
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
    fn resolver_refreshes_reused_pid_even_if_the_process_name_is_unchanged() {
        let mut r = AppResolver::new();
        let old = AppIdentity::new("/old/agent", "agent");
        let new = AppIdentity::new("/new/agent", "agent");
        assert_eq!(
            r.resolve_identity(42, Some((1, 0)), "agent", || old.clone()),
            old
        );
        assert_eq!(
            r.resolve_identity(42, Some((1, 0)), "agent", || panic!("cached")),
            old
        );
        assert_eq!(
            r.resolve_identity(42, Some((2, 0)), "agent", || new.clone()),
            new
        );
        assert_eq!(r.len(), 1);

        r.retain_only(&[]);
        assert!(r.is_empty(), "进程退出后缓存应被清理");
    }

    #[test]
    fn unresolved_identity_is_retried_instead_of_cached_forever() {
        let mut r = AppResolver::new();
        r.resolve_identity(42, None, "agent", || AppIdentity::new("agent", "agent"));
        let resolved = AppIdentity::new("/Applications/Agent.app", "Agent");
        assert_eq!(
            r.resolve_identity(42, Some((1, 0)), "agent", || resolved.clone()),
            resolved
        );
        assert_eq!(r.resolve(999_999, "old").key, "old");
        assert_eq!(r.resolve(999_999, "new").key, "new");
        r.resolve_identity(43, Some((1, 0)), "agent", || {
            AppIdentity::new("agent", "agent")
        });
        assert_eq!(
            r.resolve_identity(43, Some((1, 0)), "agent", || resolved.clone()),
            resolved
        );
    }
}
