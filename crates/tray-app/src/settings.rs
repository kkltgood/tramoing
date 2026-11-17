//! 用户设置：读写 `settings.conf`。
//!
//! 格式是 `key=value` 的纯文本，不是 JSON。理由：
//! - 只有七个字段，为此引入 serde + serde_json 太重；
//! - 用户可能想直接改文件，纯文本最直观；
//! - 解析器四十行，单测能覆盖全部边界。
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
    /// 列表最多显示几个应用
    pub row_limit: usize,
    /// 历史保留天数
    pub retention_days: i64,
    /// 近 10 分钟单应用超过这个 MB 数就在面板里提醒；0 表示关闭
    pub alert_mb: u64,
    /// 菜单栏用彩色区分上下行。关掉后统一跟随系统色，适合喜欢素一点的菜单栏
    pub tray_color: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tray_mode: TrayMode::Both,
            theme: ThemeMode::System,
            lang: LangMode::Auto,
            row_limit: 8,
            retention_days: 30,
            alert_mb: 0,
            tray_color: true,
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
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, self.serialize()) {
            eprintln!("[settings] 保存失败（{}）：{e}", path.display());
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
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
                "row_limit" => {
                    if let Ok(n) = v.parse::<usize>() {
                        s.row_limit = n.clamp(3, 20);
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
             row_limit={}\n\
             retention_days={}\n\
             alert_mb={}\n\
             tray_color={}\n",
            self.row_limit,
            self.retention_days,
            self.alert_mb,
            if self.tray_color { "on" } else { "off" }
        )
    }
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
    fn roundtrip_preserves_everything() {
        let s = Settings {
            tray_mode: TrayMode::SpeedOnly,
            theme: ThemeMode::Dark,
            lang: LangMode::En,
            row_limit: 12,
            retention_days: 90,
            alert_mb: 500,
            tray_color: false,
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
            row_limit=abc\n\
            future_key=42\n\
            lang=en\n";
        let s = Settings::parse(text);
        assert_eq!(s.theme, ThemeMode::System, "非法值应该退回默认");
        assert_eq!(s.row_limit, 8, "非法数字应该退回默认");
        assert_eq!(s.lang, LangMode::En, "同一行里的合法值仍要生效");
    }

    #[test]
    fn values_are_clamped() {
        let s = Settings::parse("row_limit=999\nretention_days=-5\nalert_mb=999999999999");
        assert_eq!(s.row_limit, 20);
        assert_eq!(s.retention_days, 1, "少于一天等于没有历史，夹到 1");
        assert_eq!(s.alert_mb, 1024 * 1024);
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Settings::parse(""), Settings::default());
    }
}
