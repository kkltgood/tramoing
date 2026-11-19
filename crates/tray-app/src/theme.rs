//! 主题与字体。
//!
//! egui 自带字体不含中文，直接显示会变成方块。两个平台的系统字体形态不一样：
//!
//! - macOS 没有单文件 CJK `.ttf`（大多是 `.ttc` 字体集合），只有
//!   `/System/Library/Fonts/Supplemental/Arial Unicode.ttf` 是单文件；
//! - Windows 全是 `.ttc`（微软雅黑、宋体）或单文件（黑体、等线），
//!   但 egui 的 `FontData` 带 `index` 字段，`.ttc` 取第 0 个字面是支持的。
//!
//! 找不到就把界面切成英文 —— 绝不能给用户看一屏方块。

use std::path::PathBuf;
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

/// 一个候选字体：文件路径 + 集合内的字面序号（单文件字体恒为 0）。
struct Candidate {
    path: PathBuf,
    index: u32,
}

fn candidates() -> Vec<Candidate> {
    #[cfg(target_os = "macos")]
    {
        vec![
            Candidate {
                path: PathBuf::from("/System/Library/Fonts/Supplemental/Arial Unicode.ttf"),
                index: 0,
            },
            Candidate {
                path: PathBuf::from("/Library/Fonts/Arial Unicode.ttf"),
                index: 0,
            },
        ]
    }

    #[cfg(windows)]
    {
        // 不写死 C: —— 系统盘符可以改，WINDIR 才是准的
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");

        let pick = |name: &str| Candidate {
            path: fonts.join(name),
            index: 0,
        };

        vec![
            pick("msyh.ttc"),   // 微软雅黑（Win7+ 都有）
            pick("Deng.ttf"),   // 等线（Win10+）
            pick("simhei.ttf"), // 黑体（最老的兜底，一定在）
            pick("simsun.ttc"), // 宋体
        ]
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Vec::new()
    }
}

/// 尝试安装中文字体，成功返回所用文件路径。
pub fn install_cjk(ctx: &egui::Context) -> Option<String> {
    for c in candidates() {
        let Ok(bytes) = std::fs::read(&c.path) else {
            continue;
        };
        if !looks_like_font(&bytes) {
            continue;
        }

        let mut data = egui::FontData::from_owned(bytes);
        // `.ttc` 是字体集合，用第 index 个字面；单文件字体这个值恒为 0
        data.index = c.index;

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        // 放在最前面：中英文混排时优先用它的中文字形
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "cjk".to_owned());
        }
        // 等宽字体里只补位，别抢掉数字的对齐
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return Some(c.path.display().to_string());
    }
    None
}

/// 只接受字体文件（TrueType / OpenType / 字体集合），排除把图片之类当字体读进去。
///
/// 前四个字节就够判断了：`00 01 00 00` / `true` / `OTTO` / `ttcf`。
fn looks_like_font(bytes: &[u8]) -> bool {
    bytes.len() > 12
        && (bytes.starts_with(&[0x00, 0x01, 0x00, 0x00])
            || bytes.starts_with(b"true")
            || bytes.starts_with(b"OTTO")
            || bytes.starts_with(b"ttcf"))
}

/// 一套配色。跟随系统明暗模式。
pub struct Palette {
    pub panel: egui::Color32,
    pub text: egui::Color32,
    pub subtext: egui::Color32,
    /// 界面强调色：悬停描边、开关轨道。**不参与**上下行的语义
    pub accent: egui::Color32,
    /// 下行（下载）速率：绿。绿色的语义是「进来」
    pub download: egui::Color32,
    /// 上行（上传）速率：蓝
    pub upload: egui::Color32,
    pub border: egui::Color32,
    /// 分段控件的底槽
    pub chip: egui::Color32,
    /// 提醒色（超阈值警告）
    pub warn: egui::Color32,
    /// 开关「关」状态的轨道色。不能用半透明白：浅色面板上几乎看不见
    pub switch_off: egui::Color32,
}

/// `tm-core` 的语义色 → egui 颜色。
///
/// 上下行的颜色**只在 `tm_core::color` 里定义一份**：菜单栏和 Windows 贴片
/// 也用同一份，改一处三处一起变（见 crates/tm-core/src/color.rs）。
fn srgb(c: tm_core::Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            panel: egui::Color32::from_rgb(28, 28, 32),
            text: egui::Color32::from_rgb(238, 238, 242),
            subtext: egui::Color32::from_rgb(148, 148, 156),
            accent: egui::Color32::from_rgb(94, 178, 255),
            download: srgb(tm_core::color::download(true)),
            upload: srgb(tm_core::color::upload(true)),
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
            download: srgb(tm_core::color::download(false)),
            upload: srgb(tm_core::color::upload(false)),
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
    fn accepts_collections_but_not_garbage() {
        // .ttc（字体集合）要收：Windows 的中文字体几乎都是集合，
        // egui 的 FontData 带 index，取第 0 个字面就行
        assert!(looks_like_font(
            b"ttcf\x00\x01\x00\x00\x00\x00\x00\x00\x00"
        ));
        assert!(looks_like_font(&[
            0x00, 0x01, 0x00, 0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13
        ]));
        // PNG 之类的二进制不能被当成字体读进去
        assert!(!looks_like_font(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"));
    }

    #[test]
    fn lang_switches_copy() {
        assert_eq!(Lang::Zh.t("网络", "Network"), "网络");
        assert_eq!(Lang::En.t("网络", "Network"), "Network");
    }
}
