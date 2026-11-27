//! 用户设置：读写 `settings.conf`。
//!
//! 使用 `key=value` 纯文本，便于用户手动编辑；写入通过同目录临时文件提交。
//!
//! 解析永远不失败：读到不认识的行或非法值就跳过，用默认值兜底。
//! 配置文件坏掉不能让整个程序起不来。

use std::path::PathBuf;

use crate::i18n::Lang;
use tm_core::color::{DisplayColors, Rgb};
use tm_core::taskbar::{TaskbarItem, TaskbarOrder};

/// Windows 手动输入的 exe 路径可能与采集器返回的大小写不同。
pub(crate) fn same_app(a: &str, b: &str) -> bool {
    a == b || (cfg!(windows) && a.to_lowercase() == b.to_lowercase())
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
    /// Match the system's ordered UI language preferences.
    System,
    Selected(Lang),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateCheckResult {
    #[default]
    Unknown,
    Pending,
    Latest,
    Available(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 是否显示菜单栏 / 系统托盘中的应用图标，与网速和性能显示独立。
    pub tray_icon: bool,
    pub theme: ThemeMode,
    pub lang: LangMode,
    /// 列表通常最多列出几个应用。置顶项优先保留，剩余名额按流量从高到低填充；
    /// 置顶项超过上限时全部展示，超过可视行数的部分靠滚动查看。
    pub max_apps: usize,
    /// 按置顶顺序保存的完整应用键。名称相同但路径不同的应用不能互相影响。
    pub pinned_apps: Vec<String>,
    /// 历史保留天数
    pub retention_days: i64,
    /// 近 10 分钟单应用超过这个 MB 数就在面板里提醒；0 表示关闭
    pub alert_mb: u64,
    /// 菜单栏用彩色区分上下行。关掉后统一跟随系统色，适合喜欢素一点的菜单栏
    pub tray_color: bool,
    /// 是否显示实时网速（macOS 菜单栏 / Windows 原生任务栏区域）。
    pub tray_badge: bool,
    /// Windows 任务栏：直接拖拽统计项保存的顺序，不在设置页提供排序控件。
    pub taskbar_order: TaskbarOrder,
    /// CPU / GPU 共用一个双行区块（macOS 菜单栏 / Windows 任务栏）。
    pub performance_enabled: bool,
    pub performance_color: bool,
    /// Hidden temperatures also stop sensor polling; utilization remains enabled.
    pub performance_temperature: bool,
    /// 性能采样间隔，只提供 1 / 2 / 5 秒。
    pub performance_interval: u64,
    /// 两个平台共用的四项显示色；未配置时继续跟随系统明暗主题。
    pub display_colors: DisplayColors,
    /// 启动后自动检查新版本。
    ///
    /// 新安装默认开启，读取官方 GitHub Releases；已有的关闭选择保持不变。
    /// 设置页提供开关，“检查更新”按钮和 --check-update 提供手动检查入口。
    pub update_check: bool,
    /// 上次检查更新的时刻（Unix 秒）。0 表示从没查过。
    ///
    /// 存盘而不是只放内存：这个程序是常驻托盘的，用户可能几周不退出，
    /// 每次启动都发一次请求既没必要，也会让「每天最多一次」失效。
    pub last_check: i64,
    pub(crate) last_check_result: UpdateCheckResult,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tray_icon: true,
            theme: ThemeMode::System,
            lang: LangMode::System,
            max_apps: 10,
            pinned_apps: Vec::new(),
            retention_days: 30,
            alert_mb: 0,
            tray_color: true,
            tray_badge: true,
            taskbar_order: TaskbarOrder::default(),
            performance_enabled: false,
            performance_color: true,
            performance_temperature: true,
            performance_interval: 2,
            display_colors: DisplayColors::default(),
            update_check: true,
            last_check: 0,
            last_check_result: UpdateCheckResult::Unknown,
        }
    }
}

impl Settings {
    pub(crate) fn last_check_local_time(&self) -> Option<String> {
        if self.last_check <= 0 {
            return None;
        }
        let timestamp = self.last_check as libc::time_t;
        if i128::from(timestamp) != i128::from(self.last_check) {
            return None;
        }
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        #[cfg(unix)]
        let valid = unsafe { !libc::localtime_r(&timestamp, &mut local).is_null() };
        #[cfg(windows)]
        let valid = unsafe { libc::localtime_s(&mut local, &timestamp) == 0 };
        if !valid {
            return None;
        }
        let year = local
            .tm_year
            .checked_add(1900)
            .filter(|year| *year <= 9999)?;
        Some(format!(
            "{year:04}-{:02}-{:02} {:02}:{:02}",
            local.tm_mon + 1,
            local.tm_mday,
            local.tm_hour,
            local.tm_min
        ))
    }

    pub fn path() -> PathBuf {
        tm_store::data_dir().join("settings.conf")
    }

    /// 读配置。文件不存在、读不了、内容坏了，统统返回默认值。
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => Self::parse(&text),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::error!(target: "tramoing::settings", "读取设置失败，使用默认值：{error}");
                }
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Err(e) = tm_store::files::atomic_write(&path, self.serialize().as_bytes()) {
            eprintln!("[settings] 保存失败（{}）：{e}", path.display());
            log::error!(target: "tramoing::settings", "保存设置失败（{}）：{e}", path.display());
        }
    }

    pub fn is_pinned(&self, app: &str) -> bool {
        self.pinned_apps.iter().any(|key| same_app(key, app))
    }

    /// 切换置顶，返回切换后的状态。已有置顶项的相对顺序保持不变。
    pub fn toggle_pin(&mut self, app: &str) -> bool {
        if self.is_pinned(app) {
            self.pinned_apps.retain(|key| !same_app(key, app));
            false
        } else if !app.trim().is_empty() {
            self.pinned_apps.push(app.to_owned());
            true
        } else {
            false
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        let mut legacy_tray = None;
        let mut tray_icon_present = false;
        let mut tray_icon = None;
        let mut tray_badge = None;
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
                    if let Some(visibility) = parse_legacy_tray(v) {
                        legacy_tray = Some(visibility);
                    }
                }
                "tray_icon" => {
                    tray_icon_present = true;
                    if let Some(value) = parse_bool(v) {
                        tray_icon = Some(value);
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
                "pinned_apps" => {
                    if let Ok(apps) = serde_json::from_str::<Vec<String>>(v) {
                        let mut seen = std::collections::HashSet::new();
                        s.pinned_apps = apps
                            .into_iter()
                            .filter(|app| {
                                !app.trim().is_empty()
                                    && seen.insert(if cfg!(windows) {
                                        app.to_lowercase()
                                    } else {
                                        app.clone()
                                    })
                            })
                            .collect();
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
                "tray_badge" => {
                    if let Some(value) = parse_bool(v) {
                        tray_badge = Some(value);
                    }
                }
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
                "performance_temperature" => {
                    if let Some(value) = parse_bool(v) {
                        s.performance_temperature = value;
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
                "last_check_result" => {
                    if let Ok(result) = serde_json::from_str(v) {
                        s.last_check_result = result;
                    }
                }
                _ => {} // 未知键忽略：旧版本写的字段不该让新版本报错
            }
        }
        if s.last_check == 0 {
            s.last_check_result = UpdateCheckResult::Unknown;
        }
        (s.tray_icon, s.tray_badge) = resolve_tray_visibility(
            legacy_tray,
            tray_icon_present,
            tray_icon,
            tray_badge,
            cfg!(windows),
        );
        if !s.tray_icon && !s.tray_badge && !s.performance_enabled {
            s.tray_icon = true;
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
        let tray_icon = if self.tray_icon { "on" } else { "off" };
        let theme = match self.theme {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        };
        let lang = match self.lang {
            LangMode::System => "system",
            LangMode::Selected(language) => language.code(),
        };
        format!(
            "# Tramoing 设置。删掉这个文件即恢复全部默认值。\n\
             tray_icon={tray_icon}\n\
             theme={theme}\n\
             lang={lang}\n\
             max_apps={}\n\
             pinned_apps={}\n\
             retention_days={}\n\
             alert_mb={}\n\
             tray_color={}\n\
             tray_badge={}\n\
             taskbar_order={}\n\
             performance_enabled={}\n\
             performance_temperature={}\n\
             performance_color={}\n\
             performance_interval={}\n\
             download_color={}\n\
             upload_color={}\n\
             cpu_color={}\n\
             gpu_color={}\n\
             update_check={}\n\
             last_check={}\n\
             last_check_result={}\n",
            self.max_apps,
            serde_json::to_string(&self.pinned_apps).expect("application keys are strings"),
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
            if self.performance_temperature {
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
            self.last_check,
            serde_json::to_string(&self.last_check_result)
                .expect("update check results are strings"),
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

/// Legacy three-way setting, represented as (application icon, network speed).
fn parse_legacy_tray(v: &str) -> Option<(bool, bool)> {
    match v {
        // full / compact 是旧版配置里的写法，读到时映射到新档位，
        // 不至于因为升级把用户的选择重置掉
        "both" | "full" => Some((true, true)),
        "speed" | "compact" => Some((false, true)),
        "icon" => Some((true, false)),
        _ => None,
    }
}

fn resolve_tray_visibility(
    legacy: Option<(bool, bool)>,
    icon_present: bool,
    icon: Option<bool>,
    badge: Option<bool>,
    windows: bool,
) -> (bool, bool) {
    if icon_present {
        // Presence marks the new format even if the value is malformed. Old mode
        // lines must never silently override either independent preference.
        return (icon.unwrap_or(true), badge.unwrap_or(true));
    }
    let legacy_speed = legacy.map(|(_, speed)| speed);
    let speed = if windows {
        // Old Windows versions displayed the taskbar speed using tray_badge.
        badge.or(legacy_speed)
    } else {
        // Old macOS files also serialized tray_badge, but only tray_mode affected
        // the menu bar. Preserve that visible behavior during migration.
        legacy_speed.or(badge)
    };
    // Windows always displayed its system tray icon before the independent switch.
    // tray_mode only controlled the macOS menu bar, even when saved on Windows.
    let icon = windows || legacy.map(|(icon, _)| icon).unwrap_or(true);
    (icon, speed.unwrap_or(true))
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
    if v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("system") {
        Some(LangMode::System)
    } else if v.eq_ignore_ascii_case("zh") {
        // Preserve the historical preference even if registry aliases are refined later.
        Some(LangMode::Selected(Lang::ZH_CN))
    } else {
        Lang::from_code(v).map(LangMode::Selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bom_does_not_discard_the_first_setting() {
        let settings = Settings::parse("\u{feff}tray_mode=speed\n");
        assert_eq!(settings.tray_icon, cfg!(windows));
        assert!(settings.tray_badge);
    }

    #[test]
    fn language_preferences_migrate_and_keep_stable_locale_codes() {
        for (old, mode, saved) in [
            ("auto", LangMode::System, "system"),
            ("system", LangMode::System, "system"),
            ("zh", LangMode::Selected(Lang::ZH_CN), "zh-CN"),
            ("zh_CN", LangMode::Selected(Lang::ZH_CN), "zh-CN"),
            ("en", LangMode::Selected(Lang::EN), "en"),
            ("unknown-locale", LangMode::System, "system"),
        ] {
            let settings = Settings::parse(&format!("lang={old}"));
            assert_eq!(settings.lang, mode);
            assert!(settings
                .serialize()
                .lines()
                .any(|line| line == format!("lang={saved}")));
            assert_eq!(Settings::parse(&settings.serialize()).lang, mode);
        }
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let s = Settings {
            tray_icon: false,
            theme: ThemeMode::Dark,
            lang: LangMode::Selected(Lang::EN),
            max_apps: 20,
            pinned_apps: vec![
                "/Applications/WeChat.app".into(),
                "C:\\Apps\\Chrome.exe".into(),
            ],
            retention_days: 90,
            alert_mb: 500,
            tray_color: false,
            tray_badge: false,
            taskbar_order: TaskbarOrder::new([TaskbarItem::Performance, TaskbarItem::Speed]),
            performance_enabled: true,
            performance_color: false,
            performance_temperature: false,
            performance_interval: 5,
            display_colors: DisplayColors {
                download: Some(Rgb(255, 123, 0)),
                upload: Some(Rgb(1, 2, 3)),
                cpu: Some(Rgb(0, 255, 255)),
                gpu: None,
            },
            update_check: false,
            last_check: 1_700_000_000,
            last_check_result: UpdateCheckResult::Available("1.2.3".into()),
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn legacy_tray_modes_migrate_with_platform_specific_speed_precedence() {
        for (mode, icon, speed) in [
            ("both", true, true),
            ("full", true, true),
            ("speed", false, true),
            ("compact", false, true),
            ("icon", true, false),
        ] {
            let legacy = parse_legacy_tray(mode);
            assert_eq!(legacy, Some((icon, speed)));
            for windows in [false, true] {
                assert_eq!(
                    resolve_tray_visibility(legacy, false, None, None, windows),
                    (windows || icon, speed)
                );
                for badge in [false, true] {
                    assert_eq!(
                        resolve_tray_visibility(legacy, false, None, Some(badge), windows),
                        (windows || icon, if windows { badge } else { speed })
                    );
                }
            }
            for badge in ["on", "off"] {
                let expected_speed = if cfg!(windows) { badge == "on" } else { speed };
                for text in [
                    format!("tray_mode={mode}\ntray_badge={badge}\nperformance_enabled=on"),
                    format!("performance_enabled=on\ntray_badge={badge}\ntray_mode={mode}"),
                ] {
                    let settings = Settings::parse(&text);
                    assert_eq!(
                        (settings.tray_icon, settings.tray_badge),
                        (cfg!(windows) || icon, expected_speed)
                    );
                    assert!(!settings.serialize().contains("tray_mode="));
                    assert_eq!(Settings::parse(&settings.serialize()), settings);
                }
            }
        }
        assert_eq!(parse_legacy_tray("nonsense"), None);
        assert_eq!(Settings::parse("tray_mode=nonsense"), Settings::default());
    }

    #[test]
    fn independent_tray_preferences_override_legacy_modes_in_any_order() {
        for icon in [false, true] {
            for speed in [false, true] {
                let new = format!("tray_icon={icon}\ntray_badge={speed}\nperformance_enabled=on");
                for mode in ["both", "speed", "icon"] {
                    for text in [
                        format!("{new}\ntray_mode={mode}"),
                        format!("tray_mode={mode}\n{new}"),
                    ] {
                        let settings = Settings::parse(&text);
                        assert_eq!((settings.tray_icon, settings.tray_badge), (icon, speed));
                        assert_eq!(Settings::parse(&settings.serialize()), settings);
                    }
                    for windows in [false, true] {
                        assert_eq!(
                            resolve_tray_visibility(
                                parse_legacy_tray(mode),
                                true,
                                Some(icon),
                                Some(speed),
                                windows
                            ),
                            (icon, speed)
                        );
                    }
                }
            }
        }
        for text in [
            "tray_mode=icon\ntray_icon=invalid",
            "tray_icon=invalid\ntray_mode=icon",
            "tray_icon=on\ntray_icon=invalid\ntray_mode=speed",
        ] {
            let settings = Settings::parse(text);
            assert!(settings.tray_icon && settings.tray_badge);
        }
        let settings =
            Settings::parse("tray_icon=off\ntray_icon=invalid\ntray_badge=on\ntray_badge=invalid");
        assert!(!settings.tray_icon && settings.tray_badge);
    }

    #[test]
    fn a_clickable_entry_is_restored_only_when_all_displays_are_disabled() {
        for icon in [false, true] {
            for speed in [false, true] {
                for performance in [false, true] {
                    for text in [
                        format!("tray_icon={icon}\ntray_badge={speed}\nperformance_enabled={performance}"),
                        format!("performance_enabled={performance}\ntray_badge={speed}\ntray_icon={icon}"),
                    ] {
                        let settings = Settings::parse(&text);
                        assert_eq!(settings.tray_icon, icon || (!speed && !performance));
                        assert_eq!(settings.tray_badge, speed);
                        assert_eq!(settings.performance_enabled, performance);
                        assert_eq!(Settings::parse(&settings.serialize()), settings);
                    }
                }
            }
        }
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
        assert_eq!(
            s.lang,
            LangMode::Selected(Lang::EN),
            "同一行里的合法值仍要生效"
        );
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
    fn pins_preserve_full_keys_order_and_special_characters_across_restart() {
        let mut settings = Settings::parse("theme=dark\nmax_apps=5");
        assert!(settings.pinned_apps.is_empty(), "旧配置默认不置顶任何应用");
        let keys = [
            "/Applications/微信.app",
            "C:\\Program Files\\Chrome\\chrome.exe",
            "/Users/example/Apps/Editor = \"preview\"\n.app",
            "/Applications/Editor.app",
            "/Users/example/Applications/Editor.app",
        ];
        for key in keys {
            assert!(settings.toggle_pin(key));
        }
        let serialized = settings.serialize();
        assert_eq!(serialized.matches("pinned_apps=").count(), 1);
        let pin_line = serialized
            .lines()
            .find(|line| line.starts_with("pinned_apps="))
            .unwrap();
        assert!(
            pin_line.contains("\\n"),
            "路径内的换行必须转义在同一配置行内"
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.conf");
        tm_store::files::atomic_write(&path, serialized.as_bytes()).unwrap();
        let mut reloaded = Settings::parse(&std::fs::read_to_string(path).unwrap());
        assert_eq!(reloaded, settings);
        assert_eq!(reloaded.pinned_apps, keys);
        assert!(!reloaded.toggle_pin(keys[3]));
        assert!(reloaded.is_pinned(keys[4]), "同名应用使用完整路径区分");
        assert_eq!(reloaded.pinned_apps, [keys[0], keys[1], keys[2], keys[4]]);
        assert!(reloaded.toggle_pin(keys[3]));
        assert_eq!(reloaded.pinned_apps.last().unwrap(), keys[3]);
        assert_eq!(Settings::parse(&reloaded.serialize()), reloaded);
    }

    #[test]
    fn invalid_pins_are_ignored_and_duplicate_keys_keep_the_first_position() {
        let settings = Settings::parse(
            "pinned_apps=[\"b\",\"\",\"a\",\"b\",\"   \",\"a\",\"c\"]\ntheme=light",
        );
        assert_eq!(settings.pinned_apps, ["b", "a", "c"]);
        assert_eq!(settings.theme, ThemeMode::Light);
        for invalid in ["broken", "null", "{}", "[1]", "[\"a\",null]", "[\"a\""] {
            let settings = Settings::parse(&format!(
                "pinned_apps=[\"saved\"]\npinned_apps={invalid}\nlang=en"
            ));
            assert_eq!(settings.pinned_apps, ["saved"]);
            assert_eq!(settings.lang, LangMode::Selected(Lang::EN));
        }
        let mut settings = settings;
        assert!(!settings.toggle_pin(" \t"));
        assert_eq!(settings.pinned_apps, ["b", "a", "c"]);
        assert!(Settings::parse("pinned_apps=[]").pinned_apps.is_empty());
    }

    #[test]
    fn saved_pins_use_the_same_path_identity_as_pin_actions() {
        let first = r"C:\Apps\app.exe";
        let alias = r"c:\apps\APP.exe";
        let other = r"D:\Apps\app.exe";
        let mut settings = Settings::parse(&format!(
            "pinned_apps={}",
            serde_json::to_string(&[first, alias, other]).unwrap()
        ));
        let expected = if cfg!(windows) {
            vec![first, other]
        } else {
            vec![first, alias, other]
        };
        assert_eq!(settings.pinned_apps, expected);
        assert!(!settings.toggle_pin(alias));
        assert_eq!(settings.is_pinned(first), !cfg!(windows));
        assert!(settings.is_pinned(other));
        assert_eq!(Settings::parse(&settings.serialize()), settings);
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
    fn check_history_migrates_without_assuming_success() {
        let timestamp = 1_700_000_000;
        for value in [
            "",
            "last_check_result=not-json",
            "last_check_result=\"future\"",
        ] {
            let settings = Settings::parse(&format!(
                "update_check=off\nlast_check={timestamp}\n{value}"
            ));
            assert_eq!(settings.last_check, timestamp);
            assert_eq!(settings.last_check_result, UpdateCheckResult::Unknown);
            assert!(!settings.update_check);
        }
        let saved = "last_check_result=\"latest\"";
        assert_eq!(
            Settings::parse(saved).last_check_result,
            UpdateCheckResult::Unknown
        );
        assert_eq!(
            Settings::parse(&format!("{saved}\nlast_check={timestamp}")).last_check_result,
            UpdateCheckResult::Latest
        );
    }

    #[test]
    fn check_history_roundtrips_on_one_line_and_survives_restart() {
        for result in [
            UpdateCheckResult::Unknown,
            UpdateCheckResult::Pending,
            UpdateCheckResult::Latest,
            UpdateCheckResult::Available("1.12.0-rc.1".into()),
            UpdateCheckResult::Failed(
                "超时\r\nupdate_check=off\nlast_check=0\n\"detail\"\\".into(),
            ),
        ] {
            let settings = Settings {
                last_check: 1_700_000_000,
                last_check_result: result,
                ..Settings::default()
            };
            let saved = settings.serialize();
            assert_eq!(
                saved
                    .lines()
                    .filter(|line| line.starts_with("last_check_result="))
                    .count(),
                1
            );
            assert!(!saved
                .lines()
                .any(|line| matches!(line, "update_check=off" | "last_check=0")));
            assert_eq!(Settings::parse(&saved), settings);
        }
    }

    #[test]
    fn check_history_formats_system_local_time_without_changing_throttle() {
        let settings = Settings::default();
        assert_eq!(settings.last_check_local_time(), None);
        let settings = Settings {
            last_check: 1_700_000_039,
            ..settings
        };
        let displayed = settings.last_check_local_time().unwrap();
        assert_eq!(displayed.len(), 16);
        assert!(displayed.starts_with("2023-11-"));
        assert_eq!(displayed.as_bytes()[10], b' ');
        assert_eq!(displayed.as_bytes()[13], b':');
        assert_eq!(settings.last_check, 1_700_000_039);
        let settings = Settings {
            last_check: i64::MAX,
            ..settings
        };
        assert_eq!(settings.last_check_local_time(), None);
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
    fn temperature_visibility_migrates_and_preserves_an_independent_opt_out() {
        let legacy = Settings::parse("performance_enabled=on");
        assert!(legacy.performance_temperature);
        for value in ["off", "false", "0"] {
            let settings = Settings::parse(&format!(
                "performance_enabled=on\nperformance_temperature={value}"
            ));
            assert!(settings.performance_enabled);
            assert!(!settings.performance_temperature);
            assert_eq!(Settings::parse(&settings.serialize()), settings);
        }
        assert!(Settings::parse("performance_temperature=invalid").performance_temperature);
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
