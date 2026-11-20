//! 用户设置：读写 `settings.conf`。
//!
//! 使用 `key=value` 纯文本，便于用户手动编辑；写入通过同目录临时文件提交。
//!
//! 解析永远不失败：读到不认识的行或非法值就跳过，用默认值兜底。
//! 配置文件坏掉不能让整个程序起不来。

use std::path::PathBuf;

/// 状态栏显示内容。
///
/// 只有三档：图标 + 两行速率 / 只要两行速率 / 只要图标。
/// 菜单栏空间紧张时「窄」本身就是可用性——被系统收进 `«` 折叠区之后，
/// 用户基本不会去点它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMode {
    /// 图标 + 上下两行（↑ 上传 / ↓ 下载）
    Both,
    /// 只有上下两行速率，不占图标的位置
    SpeedOnly,
    /// 只有图标
    IconOnly,
}

/// 外观
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

/// 界面语言
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LangMode {
    /// 有中文字体就用中文，否则英文
    Auto,
    Zh,
    En,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub tray_mode: TrayMode,
    pub theme: ThemeMode,
    pub lang: LangMode,
    /// 列表最多列出几个应用。超出这个数量的应用不参与展示（按流量从高到低截断），
    /// 超过可视行数的那部分靠滚动查看。
    pub max_apps: usize,
    /// 历史保留天数
    pub retention_days: i64,
    /// 近 10 分钟单应用超过这个 MB 数就在面板里提醒；0 表示关闭
    pub alert_mb: u64,
    /// 菜单栏用彩色区分上下行。关掉后统一跟随系统色，适合喜欢素一点的菜单栏
    pub tray_color: bool,
    /// 是否显示 Windows 原生任务栏网速区域。
    pub tray_badge: bool,
    /// 启动后自动检查新版本。
    ///
    /// **默认关闭**（2026-11-19 决定先不上线）：清单地址还没定，
    /// 开着只会让每台机器往一个占位地址发请求、再静默 404。
    /// 想启用：用户自己在 `settings.conf` 写 `update_check=on`，
    /// 或者在设置页点标题栏的版本号手动查一次（不改这个开关）。
    pub update_check: bool,
    /// 上次检查更新的时刻（Unix 秒）。0 表示从没查过。
    ///
    /// 存盘而不是只放内存：这个程序是常驻托盘的，用户可能几周不退出，
    /// 每次启动都发一次请求既没必要，也会让「每天最多一次」失效。
    pub last_check: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tray_mode: TrayMode::Both,
            theme: ThemeMode::System,
            lang: LangMode::Auto,
            max_apps: 10,
            retention_days: 30,
            alert_mb: 0,
            tray_color: true,
            tray_badge: true,
            update_check: false,
            last_check: 0,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        tm_store::data_dir().join("settings.conf")
    }

    /// 读配置。文件不存在、读不了、内容坏了，统统返回默认值。
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Err(e) = tm_store::files::atomic_write(&path, self.serialize().as_bytes()) {
            eprintln!("[settings] 保存失败（{}）：{e}", path.display());
            crate::update::log_line(&format!("保存设置失败（{}）：{e}", path.display()));
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "tray_mode" => {
                    if let Some(m) = parse_tray(v) {
                        s.tray_mode = m;
                    }
                }
                "theme" => {
                    if let Some(m) = parse_theme(v) {
                        s.theme = m;
                    }
                }
                "lang" => {
                    if let Some(m) = parse_lang(v) {
                        s.lang = m;
                    }
                }
                "max_apps" => {
                    if let Ok(n) = v.parse::<usize>() {
                        s.max_apps = snap_options(n);
                    }
                }
                // 旧版本这个键叫 row_limit，语义是「可视行数」。
                // 读到时按新语义接住，别让升级把用户的选择重置成默认值。
                "row_limit" => {
                    if let Ok(n) = v.parse::<usize>() {
                        s.max_apps = snap_options(n);
                    }
                }
                "retention_days" => {
                    if let Ok(n) = v.parse::<i64>() {
                        s.retention_days = n.clamp(1, 365);
                    }
                }
                "alert_mb" => {
                    if let Ok(n) = v.parse::<u64>() {
                        s.alert_mb = n.min(1024 * 1024);
                    }
                }
                "tray_color" => match v {
                    "on" | "true" | "1" => s.tray_color = true,
                    "off" | "false" | "0" => s.tray_color = false,
                    _ => {}
                },
                "tray_badge" => match v {
                    "on" | "true" | "1" => s.tray_badge = true,
                    "off" | "false" | "0" => s.tray_badge = false,
                    _ => {}
                },
                "update_check" => match v {
                    "on" | "true" | "1" => s.update_check = true,
                    "off" | "false" | "0" => s.update_check = false,
                    _ => {}
                },
                "last_check" => {
                    if let Ok(n) = v.parse::<i64>() {
                        s.last_check = n.max(0);
                    }
                }
                _ => {} // 未知键忽略：旧版本写的字段不该让新版本报错
            }
        }
        s
    }

    pub fn serialize(&self) -> String {
        let tray = match self.tray_mode {
            TrayMode::Both => "both",
            TrayMode::SpeedOnly => "speed",
            TrayMode::IconOnly => "icon",
        };
        let theme = match self.theme {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        };
        let lang = match self.lang {
            LangMode::Auto => "auto",
            LangMode::Zh => "zh",
            LangMode::En => "en",
        };
        format!(
            "# TrafficMonitor 设置。删掉这个文件即恢复全部默认值。\n\
             tray_mode={tray}\n\
             theme={theme}\n\
             lang={lang}\n\
             max_apps={}\n\
             retention_days={}\n\
             alert_mb={}\n\
             tray_color={}\n\
             tray_badge={}\n\
             update_check={}\n\
             last_check={}\n",
            self.max_apps,
            self.retention_days,
            self.alert_mb,
            if self.tray_color { "on" } else { "off" },
            if self.tray_badge { "on" } else { "off" },
            if self.update_check { "on" } else { "off" },
            self.last_check
        )
    }
}

/// 把任意数字吸附到界面上真实提供的三档。
///
/// 配置文件是可以手改的，旧版本也会写下 8 / 12 这种值。如果原样留着，
/// 设置页会一个选项都点不亮（`option_row` 找不到匹配项就落在第一档上）。
fn snap_options(n: usize) -> usize {
    [5usize, 10, 20]
        .into_iter()
        .min_by_key(|o| o.abs_diff(n))
        .unwrap_or(10)
}

fn parse_tray(v: &str) -> Option<TrayMode> {
    match v {
        // full / compact 是旧版配置里的写法，读到时映射到新档位，
        // 不至于因为升级把用户的选择重置掉
        "both" | "full" => Some(TrayMode::Both),
        "speed" | "compact" => Some(TrayMode::SpeedOnly),
        "icon" => Some(TrayMode::IconOnly),
        _ => None,
    }
}

fn parse_theme(v: &str) -> Option<ThemeMode> {
    match v {
        "system" => Some(ThemeMode::System),
        "light" => Some(ThemeMode::Light),
        "dark" => Some(ThemeMode::Dark),
        _ => None,
    }
}

fn parse_lang(v: &str) -> Option<LangMode> {
    match v {
        "auto" => Some(LangMode::Auto),
        "zh" => Some(LangMode::Zh),
        "en" => Some(LangMode::En),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bom_does_not_discard_the_first_setting() {
        assert_eq!(
            Settings::parse("\u{feff}tray_mode=speed\n").tray_mode,
            TrayMode::SpeedOnly
        );
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let s = Settings {
            tray_mode: TrayMode::SpeedOnly,
            theme: ThemeMode::Dark,
            lang: LangMode::En,
            max_apps: 20,
            retention_days: 90,
            alert_mb: 500,
            tray_color: false,
            tray_badge: false,
            update_check: false,
            last_check: 1_700_000_000,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn legacy_tray_modes_map_to_new_ones() {
        // 升级前写下的 full / compact 不能让用户的选择丢掉
        assert_eq!(parse_tray("full"), Some(TrayMode::Both));
        assert_eq!(parse_tray("compact"), Some(TrayMode::SpeedOnly));
        assert_eq!(parse_tray("speed"), Some(TrayMode::SpeedOnly));
        assert_eq!(parse_tray("icon"), Some(TrayMode::IconOnly));
        assert_eq!(parse_tray("both"), Some(TrayMode::Both));
        assert_eq!(parse_tray("nonsense"), None);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_ignored() {
        let text = "\
            # 注释\n\
            \n\
            theme=neon\n\
            max_apps=abc\n\
            future_key=42\n\
            lang=en\n";
        let s = Settings::parse(text);
        assert_eq!(s.theme, ThemeMode::System, "非法值应该退回默认");
        assert_eq!(s.max_apps, 10, "非法数字应该退回默认");
        assert_eq!(s.lang, LangMode::En, "同一行里的合法值仍要生效");
    }

    #[test]
    fn legacy_visible_rows_key_still_loads() {
        // 旧版本写的是 row_limit（可视行数）。升级后不能被当成未知键丢掉。
        let s = Settings::parse("row_limit=12");
        assert_eq!(
            s.max_apps, 10,
            "旧键要按「最多展示」接住，并吸附到最近的档位"
        );
    }

    #[test]
    fn values_are_clamped() {
        let s = Settings::parse("max_apps=999\nretention_days=-5\nalert_mb=999999999999");
        assert_eq!(s.max_apps, 20, "超出档位要吸附到最大一档");
        assert_eq!(s.retention_days, 1, "少于一天等于没有历史，夹到 1");
        assert_eq!(s.alert_mb, 1024 * 1024);
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Settings::parse(""), Settings::default());
    }

    #[test]
    fn update_check_is_off_by_default() {
        // 2026-11-19 决定先不上线：新装用户不该有任何出网请求，
        // 而不能指望「反正地址是占位的」——占位地址也是请求。
        assert!(!Settings::default().update_check);
    }
}
