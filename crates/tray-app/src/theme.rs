//! 主题与字体。
//!
//! egui 自带字体不含中文，直接显示会变成方块。macOS 上没有单独的 CJK `.ttf`
//! （大多是 `.ttc` 字体集合，ab_glyph 不支持），只有
//! `/System/Library/Fonts/Supplemental/Arial Unicode.ttf` 是单文件字体，
//! 所以优先用它，找不到就把界面切成英文 —— 绝不能给用户看一屏方块。

use std::sync::Arc;

use eframe::egui;

/// 界面语言。字体缺失时自动退回英文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn t(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

const CJK_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
];

/// 尝试安装中文字体，成功返回所用文件路径。
pub fn install_cjk(ctx: &egui::Context) -> Option<String> {
    for path in CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if !looks_like_single_font(&bytes) {
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        // 放在最前面：中英文混排时优先用它的中文字形
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "cjk".to_owned());
        }
        // 等宽字体里只补位，别抢掉数字的对齐
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return Some((*path).to_owned());
    }
    None
}

/// 只接受单字体文件（TrueType / OpenType），排除 `.ttc` 集合。
fn looks_like_single_font(bytes: &[u8]) -> bool {
    bytes.len() > 16
        && (bytes.starts_with(&[0x00, 0x01, 0x00, 0x00])
            || bytes.starts_with(b"true")
            || bytes.starts_with(b"OTTO"))
}

/// 一套配色。跟随系统明暗模式。
pub struct Palette {
    pub panel: egui::Color32,
    pub text: egui::Color32,
    pub subtext: egui::Color32,
    pub accent: egui::Color32,
    pub upload: egui::Color32,
    pub border: egui::Color32,
    /// 分段控件的底槽
    pub chip: egui::Color32,
    /// 提醒色（超阈值警告）
    pub warn: egui::Color32,
    /// 开关「关」状态的轨道色。不能用半透明白：浅色面板上几乎看不见
    pub switch_off: egui::Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            panel: egui::Color32::from_rgb(28, 28, 32),
            text: egui::Color32::from_rgb(238, 238, 242),
            subtext: egui::Color32::from_rgb(148, 148, 156),
            accent: egui::Color32::from_rgb(94, 178, 255),
            upload: egui::Color32::from_rgb(140, 200, 150),
            chip: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16),
            border: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
            warn: egui::Color32::from_rgb(240, 180, 90),
            switch_off: egui::Color32::from_rgb(66, 66, 72),
        }
    } else {
        Palette {
            panel: egui::Color32::from_rgb(252, 252, 253),
            text: egui::Color32::from_rgb(24, 24, 28),
            subtext: egui::Color32::from_rgb(120, 120, 128),
            accent: egui::Color32::from_rgb(20, 110, 200),
            upload: egui::Color32::from_rgb(40, 130, 80),
            chip: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10),
            border: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 26),
            warn: egui::Color32::from_rgb(176, 106, 12),
            switch_off: egui::Color32::from_rgb(198, 198, 204),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_font_collections() {
        assert!(!looks_like_single_font(b"ttcf\x00\x01\x00\x00\x00\x00\x00\x00\x00"));
        assert!(looks_like_single_font(&[0x00, 0x01, 0x00, 0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]));
    }

    #[test]
    fn lang_switches_copy() {
        assert_eq!(Lang::Zh.t("网络", "Network"), "网络");
        assert_eq!(Lang::En.t("网络", "Network"), "Network");
    }
}
