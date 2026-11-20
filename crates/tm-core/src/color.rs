//! 上下行速率的语义色。
//!
//! 同一个颜色要出现在三个地方：面板里的大数字、macOS 菜单栏的两行小字、
//! Windows 任务栏贴片。**定义只留这一份** —— 以前三处各自硬编码，
//! 面板改成「下载绿」之后菜单栏还是「下载蓝」，同一个界面里两套语义。
//!
//! 这里刻意不依赖任何 GUI 库：给的是 8 位 sRGB 三元组，egui 的 `Color32`
//! 和 AppKit 的 `NSColor` 各自转各自的。

/// 8 位 sRGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// 转成 0.0–1.0 的三个分量（AppKit 要这个）。
    pub fn to_f64(self) -> (f64, f64, f64) {
        let f = |v: u8| v as f64 / 255.0;
        (f(self.0), f(self.1), f(self.2))
    }
}

/// 深色底上的下行色：绿。绿色表示「进来」
const DOWN_ON_DARK: Rgb = Rgb(67, 211, 148);
/// 深色底上的上行色：蓝
const UP_ON_DARK: Rgb = Rgb(58, 183, 255);
/// 浅色底上的下行色：同一支绿压暗，免得在浅底上发飘
const DOWN_ON_LIGHT: Rgb = Rgb(12, 157, 91);
/// 浅色底上的上行色
const UP_ON_LIGHT: Rgb = Rgb(31, 119, 244);

/// 下行（下载）速率色。`on_dark` 指**底色**是深色。
pub fn download(on_dark: bool) -> Rgb {
    if on_dark {
        DOWN_ON_DARK
    } else {
        DOWN_ON_LIGHT
    }
}

/// 上行（上传）速率色。
pub fn upload(on_dark: bool) -> Rgb {
    if on_dark {
        UP_ON_DARK
    } else {
        UP_ON_LIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_never_share_a_color() {
        // 两个方向必须是两支色相，深浅底都一样 —— 复制粘贴写错成同一个值，这里就会炸
        for on_dark in [true, false] {
            assert_ne!(
                download(on_dark),
                upload(on_dark),
                "on_dark={on_dark} 时上下行同色，方向就分不出来了"
            );
        }
    }

    #[test]
    fn light_variants_are_darker() {
        // 浅色底要用压暗的版本，否则在浅底上看不清
        let lum = |c: Rgb| c.0 as u32 + c.1 as u32 + c.2 as u32;
        assert!(lum(download(false)) < lum(download(true)));
        assert!(lum(upload(false)) < lum(upload(true)));
    }

    #[test]
    fn components_convert_to_unit_range() {
        assert_eq!(Rgb(0, 127, 255).to_f64(), (0.0, 127.0 / 255.0, 1.0));
    }
}
