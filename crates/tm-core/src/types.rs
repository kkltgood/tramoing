//! 领域类型：采样输入与应用维度速率快照。

use std::fmt;

/// 应用标识，归并键。
///
/// macOS 优先用 bundle id（`com.google.Chrome`），取不到回退到可执行路径；
/// Windows 用可执行文件的完整路径。用它归并，Chrome 的几十个 Renderer
/// 才会在列表里合成一行，而不是刷屏。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey(pub String);

impl AppKey {
    pub fn new(s: impl Into<String>) -> Self {
        AppKey(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AppKey {
    fn from(s: &str) -> Self {
        AppKey(s.to_owned())
    }
}

/// 一次采样中某个进程的网络计数器**绝对值**（累计，非增量）。
/// 速率由 [`crate::RateCalculator`] 对两次采样做差分得到。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSample {
    pub pid: u32,
    pub app: AppKey,
    /// 累计接收字节
    pub rx_total: u64,
    /// 累计发送字节
    pub tx_total: u64,
}

impl ProcSample {
    pub fn new(pid: u32, app: impl Into<AppKey>, rx_total: u64, tx_total: u64) -> Self {
        Self {
            pid,
            app: app.into(),
            rx_total,
            tx_total,
        }
    }
}

/// 聚合到应用维度后的速率快照，UI 只消费这个。
#[derive(Debug, Clone, PartialEq)]
pub struct AppRate {
    pub app: AppKey,
    /// 接收速率，字节/秒
    pub rx_bps: f64,
    /// 发送速率，字节/秒
    pub tx_bps: f64,
    /// 进程启动以来累计接收
    pub rx_session: u64,
    /// 进程启动以来累计发送
    pub tx_session: u64,
}

impl AppRate {
    pub fn total_bps(&self) -> f64 {
        self.rx_bps + self.tx_bps
    }

    pub fn session_total(&self) -> u64 {
        self.rx_session + self.tx_session
    }
}

/// 把字节/秒格式化成菜单栏能塞下的短字符串。
pub fn format_speed(bps: f64) -> String {
    const K: f64 = 1024.0;
    if bps < 1.0 {
        return "0".to_owned();
    }
    if bps < K {
        format!("{bps:.0} B/s")
    } else if bps < K * K {
        format!("{:.1} KB/s", bps / K)
    } else if bps < K * K * K {
        format!("{:.1} MB/s", bps / (K * K))
    } else {
        format!("{:.2} GB/s", bps / (K * K * K))
    }
}

/// 从归并键里取展示名。
///
/// ```text
/// /Applications/Google Chrome.app  -> Google Chrome
/// C:\Program Files\X\chrome.exe    -> chrome
/// com.docker.backend               -> com.docker.backend（不动）
/// ```
///
/// 只剥掉 `.app` / `.exe` 两种确定的后缀，不按最后一个点切 ——
/// 否则 `com.docker.backend` 会被错切成 `com`。
pub fn pretty_name(key: &str) -> String {
    let base = key
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(key);
    let stem = base
        .strip_suffix(".app")
        .or_else(|| base.strip_suffix(".exe"))
        .unwrap_or(base);
    if stem.is_empty() {
        key.to_owned()
    } else {
        stem.to_owned()
    }
}

/// 把累计字节格式化成 "1.24 GB" 这种。
pub fn format_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.2} GB", b / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_speed_scales() {
        assert_eq!(format_speed(0.0), "0");
        assert_eq!(format_speed(999.0), "999 B/s");
        assert_eq!(format_speed(1536.0), "1.5 KB/s");
        assert_eq!(format_speed(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn pretty_name_strips_bundle_and_exe() {
        assert_eq!(pretty_name("/Applications/Google Chrome.app"), "Google Chrome");
        assert_eq!(pretty_name(r"C:\Program Files\Google\chrome.exe"), "chrome");
        assert_eq!(pretty_name("mDNSResponder"), "mDNSResponder");
        // 带点的进程名不能被切
        assert_eq!(pretty_name("com.docker.backend"), "com.docker.backend");
    }
}
