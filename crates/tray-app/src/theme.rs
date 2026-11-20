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
    let mut fonts = egui::FontDefinitions::default();
    let mut cjk_path = None;
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

        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        // 放在最前面：中英文混排时优先用它的中文字形
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "cjk".to_owned());
        }
        // 等宽字体里只补位，别抢掉数字的对齐
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.push("cjk".to_owned());
        }
        cjk_path = Some(c.path.display().to_string());
        break;
    }

    // 正文与数字分别使用系统的常规、半粗字面，中文仍由上面的字体补齐。
    // 不随应用分发系统字体；缺失时保留 egui 自带字体。
    #[cfg(target_os = "macos")]
    let faces = [
        (
            "body",
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"),
        ),
        (
            "display",
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial Bold.ttf"),
        ),
    ];
    #[cfg(windows)]
    let faces = {
        let dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        [
            ("body", dir.join("segoeui.ttf")),
            ("display", dir.join("seguisb.ttf")),
        ]
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let faces: [(&str, PathBuf); 0] = [];
    for (name, path) in faces {
        if let Ok(bytes) = std::fs::read(path) {
            if looks_like_font(&bytes) {
                fonts
                    .font_data
                    .insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            }
        }
    }
    if fonts.font_data.contains_key("body") {
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "body".into());
    }
    let mut display = fonts.families[&egui::FontFamily::Proportional].clone();
    if fonts.font_data.contains_key("display") {
        display.insert(0, "display".into());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name("display".into()), display);
    ctx.set_fonts(fonts);
    cjk_path
}

pub fn display_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("display".into()))
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
    pub dark: bool,
    pub panel: egui::Color32,
    pub panel_top: egui::Color32,
    pub panel_bottom: egui::Color32,
    pub wave: egui::Color32,
    /// 卡片、设置分组的底色。
    pub surface: egui::Color32,
    /// 分段选项的选中底色。
    pub selection: egui::Color32,
    pub selection_top: egui::Color32,
    pub on_selection: egui::Color32,
    pub text: egui::Color32,
    pub subtext: egui::Color32,
    /// 速率单位（`B/s`）、计数（`· 3`）这类「注脚」文字。
    ///
    /// 以前是 `subtext * 0.7~0.8` 算出来的：浅底上叠两次变淡，10pt 的字
    /// 直接糊成一片浅灰。注脚该比正文淡，但不该淡到读不出来 —— 单独一档。
    pub unit: egui::Color32,
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
            dark: true,
            panel: egui::Color32::from_rgb(24, 34, 55),
            panel_top: egui::Color32::from_rgb(30, 43, 70),
            panel_bottom: egui::Color32::from_rgb(23, 32, 51),
            wave: egui::Color32::from_rgb(36, 55, 96),
            surface: egui::Color32::from_rgb(32, 44, 67),
            selection: egui::Color32::from_rgb(37, 139, 245),
            selection_top: egui::Color32::from_rgb(68, 173, 255),
            on_selection: egui::Color32::WHITE,
            text: egui::Color32::from_rgb(245, 247, 255),
            subtext: egui::Color32::from_rgb(181, 193, 219),
            unit: egui::Color32::from_rgb(166, 182, 210),
            accent: egui::Color32::from_rgb(67, 165, 255),
            download: srgb(tm_core::color::download(true)),
            upload: srgb(tm_core::color::upload(true)),
            chip: egui::Color32::from_rgb(26, 38, 62),
            border: egui::Color32::from_rgb(51, 68, 101),
            warn: egui::Color32::from_rgb(240, 180, 90),
            switch_off: egui::Color32::from_rgb(66, 82, 112),
        }
    } else {
        // 浅色底上的每一档灰都往上压了一格：11pt 的中文在纯白面板上，
        // 原来那套 (#787880 / 10% 底槽 / 10% 描边) 读起来像没画完。
        // 层次靠字号和颜色深浅区分，不靠「淡到几乎看不见」。
        Palette {
            dark: false,
            panel: egui::Color32::from_rgb(237, 243, 255),
            panel_top: egui::Color32::from_rgb(222, 233, 255),
            panel_bottom: egui::Color32::from_rgb(242, 246, 255),
            wave: egui::Color32::from_rgb(213, 228, 255),
            surface: egui::Color32::from_rgb(249, 251, 255),
            selection: egui::Color32::from_rgb(36, 91, 244),
            selection_top: egui::Color32::from_rgb(59, 131, 255),
            on_selection: egui::Color32::WHITE,
            text: egui::Color32::from_rgb(15, 28, 73),
            subtext: egui::Color32::from_rgb(54, 73, 117),
            unit: egui::Color32::from_rgb(67, 85, 126),
            accent: egui::Color32::from_rgb(38, 96, 244),
            download: srgb(tm_core::color::download(false)),
            upload: srgb(tm_core::color::upload(false)),
            // 底槽 = 分段控件的轨道：太淡就看不出「这是一组可选项」
            chip: egui::Color32::from_rgb(225, 234, 251),
            // 描边同时用于行卡片、分隔线和面板外框，26/255 在浅底上几乎没有边
            border: egui::Color32::from_rgb(214, 225, 247),
            warn: egui::Color32::from_rgb(166, 96, 8),
            switch_off: egui::Color32::from_rgb(173, 190, 220),
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
        assert!(looks_like_font(b"ttcf\x00\x01\x00\x00\x00\x00\x00\x00\x00"));
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
