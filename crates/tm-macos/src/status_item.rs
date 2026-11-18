//! 原生 macOS 状态栏项（`NSStatusItem`）。
//!
//! # 为什么不用 `tray-icon`
//!
//! 在本机（macOS 27）上它创建的项拿到的窗口高度恒为 0
//! （`frame = (0, 2338, 160, 0)`），菜单栏里什么都不出现；
//! 而同样逻辑的原生 Swift `NSStatusItem` 打包成 `.app` 后完全正常
//! （按钮 53×22、窗口 39pt 高、`isVisible = true`）。
//!
//! 也就是说问题出在那层抽象上，不是系统限制。我们需要的能力其实只有三件事：
//! 一个图标、一段文字、一个点击回调 —— 直接调 AppKit 更省心。

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, AnyThread, ClassType, MainThreadMarker};
use objc2_app_kit::{
    NSAppearanceCustomization, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSFont,
    NSBaselineOffsetAttributeName, NSFontAttributeName, NSFontWeightMedium,
    NSForegroundColorAttributeName, NSImage,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSStatusBar, NSStatusItem,
    NSTextAlignment, NSVariableStatusItemLength, NSSquareStatusItemLength,
};
use objc2_foundation::{
    NSDictionary, NSMutableAttributedString, NSNumber, NSRange, NSSize, NSString,
    NSObjectProtocol,
};

/// 点击回调。全进程只有一个状态栏项，用全局变量比往 ObjC 对象里塞闭包省事。
static ON_CLICK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// 菜单栏标题里一段文字的语义。
///
/// 这里只表达「这段是下载 / 上传 / 无所谓」，具体颜色由状态栏当时的明暗外观
/// 决定 —— 浅色菜单栏配深一档的蓝绿，深色菜单栏配亮一档，
/// 否则同一个颜色总有一边看不清。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Down,
    Up,
    /// 不区分方向：分隔空格、纯文字模式
    Plain,
}

/// 两行文字的显示规格。
///
/// 竖排而不是横排：菜单栏横向空间最贵，横着一组 `↓107K ↑178K` 要九十几点，
/// 竖排之后宽度只取最长那一行；而且「上面那行是上传、下面那行是下载」是固定
/// 位置关系，扫一眼不用再按箭头方向去分辨。
pub struct SpeedLines<'a> {
    /// 上行（上传）
    pub up: &'a str,
    /// 下行（下载）
    pub down: &'a str,
    /// 箭头是否按方向着色；关掉后整段跟随系统标签色
    pub colored: bool,
}

/// 行高与字号。
///
/// 菜单栏按钮只有 22pt 高，而 NSButtonCell 给标题留的上下边距很小：
/// 字号 10pt / 行高 10.5 时第一行的字顶会被切掉，9.5pt / 10.0 才刚好。
/// 这两个数不要凭感觉调大，改完要用离屏渲染（`dump_png`）核对一遍。
const LINE_H: f64 = 10.0;
/// 两行版本的字号，比菜单栏正文字号（13pt）小一档。
const FONT_SIZE: f64 = 9.5;

/// 整体下移的量（pt，负数是往下）。
///
/// NSButtonCell 画多行标题时把整块顶在内容区上沿，不下移第一行的字顶会被切平；
/// 下移太多又会切掉第二行。实测 22pt 高的按钮上下各留 2pt 左右时（-4.0）正好。
const BASELINE_SHIFT: f64 = -4.0;

/// 标题字号取系统菜单栏字体大小，保证和旁边的输入法、电量图标一个高度。
fn title_font() -> Retained<NSFont> {
    // **整段等宽**，不只是数字。
    //
    // 数值本身已经固定成 4 个字符（`tray-app` 的 `menu_speed`），但 K/M/B
    // 这些单位字母在比例字体里宽度不同：`820K` 和 `1.2M` 都是 4 字符，
    // 宽度却能差一两个像素，菜单栏项照样会左右挪。全等宽之后，字符数相同
    // 宽度就相同，整块标题一个像素都不会变。
    //
    // 权重 medium 而不是 semibold —— 菜单栏里其他文字都是 regular，
    // semibold 会显得比系统时间还重，像在喊。
    unsafe { NSFont::monospacedSystemFontOfSize_weight(FONT_SIZE, NSFontWeightMedium) }
}

/// 状态栏当前是不是深色。`name` 里带 `Dark` 的就是。
fn is_dark(button: &objc2_app_kit::NSStatusBarButton) -> bool {
    button.effectiveAppearance().name().to_string().contains("Dark")
}

/// 语义色调 → 实际颜色。
fn tone_color(tone: Tone, dark: bool) -> Retained<NSColor> {
    let (r, g, b) = match (tone, dark) {
        (Tone::Down, true) => (0.44, 0.71, 1.00),
        (Tone::Up, true) => (0.44, 0.85, 0.56),
        (Tone::Down, false) => (0.09, 0.41, 0.77),
        (Tone::Up, false) => (0.11, 0.50, 0.27),
        (Tone::Plain, _) => return NSColor::labelColor(),
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)
}


define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TMStatusItemTarget"]
    struct ClickTarget;

    unsafe impl NSObjectProtocol for ClickTarget {}

    impl ClickTarget {
        /// `NSStatusBarButton` 的 action。`-` 结尾的 selector 记得带冒号，
        /// 少写一个冒号会静默地永远不触发。
        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: &AnyObject) {
            if let Some(cb) = ON_CLICK.get() {
                cb();
            }
        }
    }
);

pub struct StatusItem {
    item: Retained<NSStatusItem>,
    /// 必须持有：target 是 weak 的（NSControl 的 target 不 retain），
    /// 掉了就再也收不到点击。
    _target: Retained<ClickTarget>,
}

impl StatusItem {
    /// 创建状态栏项。`on_click` 在用户点击图标时被调用（主线程）。
    pub fn new(on_click: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        let _ = ON_CLICK.set(Box::new(on_click));
        let mtm = MainThreadMarker::new()?;

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);

        let target: Retained<ClickTarget> = unsafe { msg_send![ClickTarget::class(), new] };
        let button = item.button(mtm)?;
        // 标题是两行的，必须允许换行、关掉单行模式，
        // 否则 AppKit 会把 "\n" 当成普通字符，两行挤成一行再被裁掉。
        if let Some(cell) = button.cell() {
            cell.setWraps(true);
            cell.setUsesSingleLineMode(false);
        }
        // SAFETY: target 由 Self 持有到状态项销毁；selector 就是本模块定义的那个
        unsafe {
            button.setTarget(Some(&*target as &AnyObject));
            button.setAction(Some(sel!(toggle:)));
        }

        Some(Self {
            item,
            _target: target,
        })
    }

    /// 设置菜单栏图标。`rgba` 是黑底 + alpha 的模板图，系统自己上色。
    pub fn set_icon(&self, rgba: &[u8], width: u32, height: u32) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(button) = self.item.button(mtm) else {
            return;
        };
        if let Some(img) = make_template_image(rgba, width, height) {
            button.setImage(Some(&img));
        }
    }

    /// 去掉图标，只留文字。
    pub fn clear_icon(&self) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(button) = self.item.button(mtm) else {
            return;
        };
        button.setImage(None);
    }

    /// 设置上下两行的速度文字。
    pub fn set_speed_lines(&self, spec: &SpeedLines) {
        let up_tone = if spec.colored { Tone::Up } else { Tone::Plain };
        let down_tone = if spec.colored { Tone::Down } else { Tone::Plain };

        // 箭头带色、数字跟系统标签色走：数字是主要信息，染色反而降对比度，
        // 箭头染一下 + 固定的上下位置就已经能把两个方向分开了。
        self.apply_parts(&[
            ("↑ ", up_tone),
            (spec.up, Tone::Plain),
            ("\n", Tone::Plain),
            ("↓ ", down_tone),
            (spec.down, Tone::Plain),
        ]);
    }

    /// 清空文字，只留图标。
    pub fn clear_title(&self) {
        self.apply_parts(&[]);
    }

    /// 设置分段着色的标题。传空切片表示只留图标。
    ///
    /// 用 `attributedTitle` 而不是 `title`：箭头要单独上色，
    /// 而且两行之间的行高必须自己压紧，否则第二行会被菜单栏裁掉。
    fn apply_parts(&self, parts: &[(&str, Tone)]) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(button) = self.item.button(mtm) else {
            return;
        };

        // 只有图标时把长度改成「一个正方形」：光把标题设成空串，
        // 状态项未必会跟着缩回去（实测会保留上一次的宽度，白占一块菜单栏）。
        self.item.setLength(if parts.is_empty() {
            NSSquareStatusItemLength
        } else {
            NSVariableStatusItemLength
        });

        let joined: String = parts.iter().map(|(t, _)| *t).collect();
        let dark = is_dark(&button);
        let font = title_font();
        let text = NSString::from_str(&joined);
        let s =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &text);

        // 整段先统一字体，再按段落刷颜色。UTF-16 长度：我们只用 ASCII 和箭头，
        // 与字节长度不同，不能用 joined.len()。
        let total = joined.encode_utf16().count();
        unsafe {
            s.addAttribute_value_range(NSFontAttributeName, font.as_ref(), NSRange::new(0, total));

            // 两行必须左对齐：否则每行按自己的宽度居中，箭头列会错开。
            let para = NSMutableParagraphStyle::new();
            para.setAlignment(NSTextAlignment::Left);
            para.setMinimumLineHeight(LINE_H);
            para.setMaximumLineHeight(LINE_H);
            s.addAttribute_value_range(
                NSParagraphStyleAttributeName,
                para.as_ref(),
                NSRange::new(0, total),
            );

            let num = NSNumber::new_f64(BASELINE_SHIFT);
            s.addAttribute_value_range(
                NSBaselineOffsetAttributeName,
                num.as_ref(),
                NSRange::new(0, total),
            );

            let mut loc = 0usize;
            for (t, tone) in parts {
                let len = t.encode_utf16().count();
                if len == 0 {
                    continue;
                }
                let c = tone_color(*tone, dark);
                s.addAttribute_value_range(
                    NSForegroundColorAttributeName,
                    c.as_ref(),
                    NSRange::new(loc, len),
                );
                loc += len;
            }
        }

        button.setAttributedTitle(&s);
    }

    /// 鼠标悬停提示。菜单栏位置太窄，完整数值放这里。
    pub fn set_tooltip(&self, text: Option<&str>) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(button) = self.item.button(mtm) else {
            return;
        };
        let s = text.map(NSString::from_str);
        button.setToolTip(s.as_ref().map(|s| &**s));
    }

    /// 图标在屏幕上的位置，**左上角原点、单位是点**。
    ///
    /// AppKit 的坐标系原点在左下，这里转成弹窗定位习惯的左上原点。
    /// 拿不到窗口（项还没被布局）时返回 `None`。
    pub fn icon_rect(&self) -> Option<(f64, f64, f64, f64)> {
        let mtm = MainThreadMarker::new()?;
        let button = self.item.button(mtm)?;
        let window = button.window()?;
        let f = window.frame();
        if f.size.height <= 0.0 {
            return None;
        }
        // 用所在屏幕而不是主屏：多显示器时状态栏在哪个屏就得按哪个屏算
        let screen_h = window
            .screen()
            .map(|s| s.frame().size.height)
            .unwrap_or(f.origin.y + f.size.height);
        let top = screen_h - (f.origin.y + f.size.height);
        Some((f.origin.x, top, f.size.width, f.size.height))
    }

    /// 是否真的挂上了状态栏。排查「图标不出现」时先看这个。
    pub fn is_visible(&self) -> bool {
        self.item.isVisible()
    }

    /// 把按钮当前的样子离屏渲染成 PNG。
    ///
    /// 状态栏项被系统收进 `«` 折叠区时，截屏永远拍不到它，但按钮本身
    /// 仍然是活的 —— `cacheDisplay` 走的是视图自己的绘制路径，
    /// 与窗口是否可见无关，正好用来核对排版（行距、对齐、有没有被裁）。
    pub fn dump_png(&self, path: &str) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let Some(button) = self.item.button(mtm) else {
            return false;
        };
        let bf = button.bounds();
        if bf.size.width <= 0.0 || bf.size.height <= 0.0 {
            return false;
        }
        unsafe {
            let Some(rep) = button.bitmapImageRepForCachingDisplayInRect(bf) else {
                return false;
            };
            button.cacheDisplayInRect_toBitmapImageRep(bf, &rep);
            let props = NSDictionary::new();
            let Some(data) =
                rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props)
            else {
                return false;
            };
            data.writeToFile_atomically(&NSString::from_str(path), true)
        }
    }

    /// 排查用的状态串：按钮尺寸、标题长度、窗口可见性与遮挡状态。
    ///
    /// 「状态栏项明明建出来了却看不见」有几种完全不同的成因（没布局、
    /// 窗口不在当前 Space、被系统收进 `«` 折叠区），光看截图分不清，
    /// 这串文本能一次说清。
    pub fn debug_state(&self) -> String {
        let Some(mtm) = MainThreadMarker::new() else {
            return "不在主线程".to_owned();
        };
        let Some(button) = self.item.button(mtm) else {
            return "没有按钮".to_owned();
        };
        let bf = button.frame();
        let len = button.attributedTitle().length();
        let win = button.window();
        let (wvis, osp, alpha, occl) = match &win {
            Some(w) => (
                w.isVisible(),
                w.isOnActiveSpace(),
                w.alphaValue(),
                w.occlusionState().0,
            ),
            None => (false, false, 0.0, 0),
        };
        format!(
            "按钮 {:.0}×{:.0} 标题 {len} 字 窗口可见={wvis} 活跃Space={osp} alpha={alpha:.1} occlusion=0x{occl:x}",
            bf.size.width, bf.size.height,
        )
    }

}

impl Drop for StatusItem {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

/// 把 RGBA 缓冲区做成模板 `NSImage`。
fn make_template_image(rgba: &[u8], width: u32, height: u32) -> Option<Retained<NSImage>> {
    let (w, h) = (width as isize, height as isize);
    // SAFETY: 只做离屏构造，不碰视图；planes 传 null 表示由 AppKit 分配内存
    unsafe {
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            w * 4,
            32,
        )?;

        let dst = rep.bitmapData();
        if dst.is_null() {
            return None;
        }
        let src_row = (width * 4) as usize;
        let dst_row = rep.bytesPerRow() as usize;
        if rgba.len() < src_row * height as usize {
            return None;
        }
        for y in 0..height as usize {
            std::ptr::copy_nonoverlapping(
                rgba.as_ptr().add(y * src_row),
                dst.add(y * dst_row),
                src_row,
            );
        }

        // 传进来的是 @2x 像素，声明成一半的点数才能在视网膜屏上按原始像素显示。
        // 直接声明成 36pt 的话，AppKit 会把它当成 36pt 的图缩到 24pt 高，
        // 等于先放大再缩小，边缘会糊。
        const SCALE: f64 = 2.0;
        let img = NSImage::initWithSize(
            NSImage::alloc(),
            NSSize::new(w as f64 / SCALE, h as f64 / SCALE),
        );
        img.addRepresentation(&rep);
        // 模板图：系统只取 alpha，自行按明暗模式上色
        img.setTemplate(true);
        Some(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有真跑起来才能拿到 mtm；这里只验证不 panic 的路径。
    #[test]
    fn image_from_rgba_is_rejected_when_buffer_too_small() {
        assert!(make_template_image(&[0u8; 8], 4, 4).is_none());
    }

    #[test]
    fn image_from_rgba_succeeds_for_full_buffer() {
        let (w, h) = (8u32, 8u32);
        let rgba = vec![255u8; (w * h * 4) as usize];
        let img = make_template_image(&rgba, w, h).expect("应该能构造图像");
        assert!(img.isTemplate());
    }
}
