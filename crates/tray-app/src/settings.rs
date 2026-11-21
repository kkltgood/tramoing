//! 用户设置：读写 `settings.conf`。
//!
//! 使用 `key=value` 纯文本，便于用户手动编辑；写入通过同目录临时文件提交。
//!
//! 解析永远不失败：读到不认识的行或非法值就跳过，用默认值兜底。
//! 配置文件坏掉不能让整个程序起不来。

use std::path::PathBuf;
use tm_core::color::{DisplayColors, Rgb};
use tm_core::taskbar::{TaskbarItem, TaskbarOrder};

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
    /// Windows 任务栏：直接拖拽统计项保存的顺序，不在设置页提供排序控件。
    pub taskbar_order: TaskbarOrder,
    /// CPU / GPU 共用一个双行区块（macOS 菜单栏 / Windows 任务栏）。
    pub performance_enabled: bool,
    pub performance_color: bool,
    /// 性能采样间隔，只提供 1 / 2 / 5 秒。
    pub performance_interval: u64,
    /// 两个平台共用的四项显示色；未配置时继续跟随系统明暗主题。
    pub display_colors: DisplayColors,
    /// 启动后自动检查新版本。
    ///
    /// 新安装默认开启，读取官方 GitHub Releases；已有的关闭选择保持不变。
    /// 设置页提供开关，版本号和 --check-update 提供手动检查入口。
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
            taskbar_order: TaskbarOrder::default(),
            performance_enabled: false,
            performance_color: true,
            performance_interval: 2,
            display_colors: DisplayColors::default(),
            update_check: true,
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
        let mut taskbar_order = None;
        let mut legacy_performance_first = false;
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
                "taskbar_performance_first" => {
                    if let Some(value) = parse_bool(v) {
                        legacy_performance_first = value;
                    }
                }
                "taskbar_order" => {
                    if let Some(order) = TaskbarOrder::parse(v) {
                        taskbar_order = Some(order);
                    }
                }
                "update_check" => match v {
                    "on" | "true" | "1" => s.update_check = true,
                    "off" | "false" | "0" => s.update_check = false,
                    _ => {}
                },
                "performance_enabled" => {
                    if let Some(value) = parse_bool(v) {
                        s.performance_enabled = value;
                    }
                }
                "performance_color" => {
                    if let Some(value) = parse_bool(v) {
                        s.performance_color = value;
                    }
                }
                "performance_interval" => {
                    if let Ok(value @ (1 | 2 | 5)) = v.parse::<u64>() {
                        s.performance_interval = value;
                    }
                }
                "download_color" | "upload_color" | "cpu_color" | "gpu_color" => {
                    let color = if v.eq_ignore_ascii_case("auto") {
                        Some(None)
                    } else {
                        Rgb::from_hex(v).map(Some)
                    };
                    if let Some(color) = color {
                        match k {
                            "download_color" => s.display_colors.download = color,
                            "upload_color" => s.display_colors.upload = color,
                            "cpu_color" => s.display_colors.cpu = color,
                            _ => s.display_colors.gpu = color,
                        }
                    }
                }
                "last_check" => {
                    if let Ok(n) = v.parse::<i64>() {
                        s.last_check = n.max(0);
                    }
                }
                _ => {} // 未知键忽略：旧版本写的字段不该让新版本报错
            }
        }
        // 新格式优先，与两个键在文件中的先后无关；兼容升级前的顺序。
        if let Some(order) = taskbar_order {
            s.taskbar_order = order;
        } else if legacy_performance_first {
            s.taskbar_order = TaskbarOrder::new([TaskbarItem::Performance, TaskbarItem::Speed]);
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
             taskbar_order={}\n\
             performance_enabled={}\n\
             performance_color={}\n\
             performance_interval={}\n\
             download_color={}\n\
             upload_color={}\n\
             cpu_color={}\n\
             gpu_color={}\n\
             update_check={}\n\
             last_check={}\n",
            self.max_apps,
            self.retention_days,
            self.alert_mb,
            if self.tray_color { "on" } else { "off" },
            if self.tray_badge { "on" } else { "off" },
            self.taskbar_order.serialize(),
            if self.performance_enabled {
                "on"
            } else {
                "off"
            },
            if self.performance_color { "on" } else { "off" },
            self.performance_interval,
            serialize_color(self.display_colors.download),
            serialize_color(self.display_colors.upload),
            serialize_color(self.display_colors.cpu),
            serialize_color(self.display_colors.gpu),
            if self.update_check { "on" } else { "off" },
            self.last_check
        )
    }
}

fn serialize_color(color: Option<Rgb>) -> String {
    color.map(Rgb::to_hex).unwrap_or_else(|| "auto".into())
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

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
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
            taskbar_order: TaskbarOrder::new([TaskbarItem::Performance, TaskbarItem::Speed]),
            performance_enabled: true,
            performance_color: false,
            performance_interval: 5,
            display_colors: DisplayColors {
                download: Some(Rgb(255, 123, 0)),
                upload: Some(Rgb(1, 2, 3)),
                cpu: Some(Rgb(0, 255, 255)),
                gpu: None,
            },
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
    fn update_check_defaults_on_and_preserves_an_explicit_opt_out() {
        assert!(Settings::default().update_check);
        assert!(Settings::parse("theme=dark").update_check);
        for value in ["off", "false", "0"] {
            let settings = Settings::parse(&format!("update_check={value}"));
            assert!(!settings.update_check);
            assert_eq!(Settings::parse(&settings.serialize()), settings);
        }
    }

    #[test]
    fn performance_settings_are_opt_in_and_validate_interval() {
        assert!(!Settings::parse("theme=dark").performance_enabled);
        let settings = Settings::parse(
            "performance_enabled=on\nperformance_color=off\nperformance_interval=1",
        );
        assert!(settings.performance_enabled);
        assert!(!settings.performance_color);
        assert_eq!(settings.performance_interval, 1);
        for value in ["0", "-1", "3", "999", "abc"] {
            assert_eq!(
                Settings::parse(&format!("performance_interval={value}")).performance_interval,
                2
            );
        }
        assert!(!Settings::parse("performance_enabled=maybe").performance_enabled);
    }

    #[test]
    fn taskbar_order_defaults_to_network_and_survives_restart() {
        assert_eq!(
            Settings::parse("theme=dark").taskbar_order,
            TaskbarOrder::default()
        );
        for value in ["on", "true", "1"] {
            let settings = Settings::parse(&format!("taskbar_performance_first={value}"));
            assert_eq!(settings.taskbar_order.items()[0], TaskbarItem::Performance);
            assert!(!settings.serialize().contains("taskbar_performance_first="));
            assert_eq!(Settings::parse(&settings.serialize()), settings);
        }
        assert_eq!(
            Settings::parse("taskbar_performance_first=invalid").taskbar_order,
            TaskbarOrder::default()
        );
        let back = Settings::parse("taskbar_performance_first=on\ntaskbar_performance_first=off");
        assert_eq!(back.taskbar_order, TaskbarOrder::default());
        assert_eq!(Settings::parse(&back.serialize()), back);
    }

    #[test]
    fn taskbar_item_order_takes_precedence_and_preserves_hidden_items() {
        for text in [
            "taskbar_order=performance,speed\ntaskbar_performance_first=off",
            "taskbar_performance_first=off\ntaskbar_order=performance,speed",
            "taskbar_order=performance\ntaskbar_order=unknown",
        ] {
            let settings = Settings::parse(text);
            assert_eq!(
                settings.taskbar_order.items(),
                &[TaskbarItem::Performance, TaskbarItem::Speed]
            );
            assert!(!settings.performance_enabled);
            assert_eq!(Settings::parse(&settings.serialize()), settings);
        }
        assert_eq!(
            Settings::parse("taskbar_performance_first=on\ntaskbar_order=speed,performance")
                .taskbar_order,
            TaskbarOrder::default()
        );
        assert_eq!(
            Settings::parse("taskbar_performance_first=on\ntaskbar_order=invalid")
                .taskbar_order
                .items()[0],
            TaskbarItem::Performance
        );
    }

    #[test]
    fn display_colors_validate_hex_preserve_black_and_restore_theme_defaults() {
        let settings = Settings::parse(
            "download_color=#ff8800\nupload_color=#000000\ncpu_color=#aaBBcc\ngpu_color=auto",
        );
        assert_eq!(settings.display_colors.download, Some(Rgb(255, 136, 0)));
        assert_eq!(settings.display_colors.upload, Some(Rgb(0, 0, 0)));
        assert_eq!(settings.display_colors.cpu, Some(Rgb(170, 187, 204)));
        assert_eq!(settings.display_colors.gpu, None);
        assert_eq!(Settings::parse(&settings.serialize()), settings);
        assert_eq!(
            Settings::parse("theme=dark").display_colors,
            DisplayColors::default()
        );
        for bad in ["#xyzxyz", "#123", "#11223344", "#中文", "red", "123456", ""] {
            let settings = Settings::parse(&format!("cpu_color=#112233\ncpu_color={bad}"));
            assert_eq!(settings.display_colors.cpu, Some(Rgb(17, 34, 51)));
        }
        assert_eq!(
            Settings::parse("cpu_color=#112233\ncpu_color=auto")
                .display_colors
                .cpu,
            None
        );
    }
}
