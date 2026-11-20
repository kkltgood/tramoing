//! 从 `.app` bundle 取应用图标。
//!
//! 为什么不自己解 `.icns`：现代 app 的 icns 里混着 PNG、JPEG2000、多种尺寸，
//! 还得先解析 Info.plist 才能知道图标文件名。`NSWorkspace` 一行就能拿到
//! 和 Finder / Dock 完全一致的那张图，而且是按目标尺寸渲染好的。

use objc2::rc::Retained;
// objc2 0.6 把 `alloc` 挪到了 AnyThread（它标记「可以在任意线程上分配」）
use objc2::AnyThread;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace,
    NSGraphicsContext, NSImage, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// 输出位图边长（像素）。列表里按 18pt 显示，2x 屏需要 36px。
pub const ICON_SIDE: u32 = 36;

/// 解码好的图标位图。
#[derive(Debug, Clone)]
pub struct IconBitmap {
    pub width: u32,
    pub height: u32,
    /// RGBA8，**非预乘**（对应 egui 的 `from_rgba_unmultiplied`）
    pub rgba: Vec<u8>,
}

impl IconBitmap {
    /// 整张图是否全透明。有些系统进程会拿到空白图标，这种和「取不到」等价。
    pub fn is_blank(&self) -> bool {
        self.rgba.chunks_exact(4).all(|p| p[3] == 0)
    }
}

/// 取 bundle 图标。不是 `.app` 或路径不存在时返回 `None`，
/// 调用方回退到「首字母色块」，不会出现一个通用的空白文档图标。
pub fn app_icon(bundle_path: &str) -> Option<IconBitmap> {
    // Rust 的常驻后台线程没有 Cocoa 事件循环替它清理 autorelease 对象。
    objc2::rc::autoreleasepool(|_| load_icon(bundle_path))
}

fn load_icon(bundle_path: &str) -> Option<IconBitmap> {
    if !bundle_path.ends_with(".app") || !std::path::Path::new(bundle_path).exists() {
        return None;
    }

    let side = ICON_SIDE as isize;

    // SAFETY: 全程只做离屏绘制，不触碰任何 NSView / NSWindow。
    // AppKit 的图像渲染允许在非主线程进行，这正是把图标加载放到后台线程的前提。
    unsafe {
        let workspace = NSWorkspace::sharedWorkspace();
        let image: Retained<NSImage> = workspace.iconForFile(&NSString::from_str(bundle_path));

        // 先试「非预乘」格式：这样读出来的像素可以直接交给 egui 的
        // from_rgba_unmultiplied。某些 macOS 版本不认这个 format 标志，
        // 会导致 graphicsContextWithBitmapImageRep 返回 nil，此时退回默认
        // （预乘）格式，在拷贝时再做一次反预乘。
        let mut premultiplied = false;
        let mut rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side,
            side,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            side * 4,
            32,
        );

        let mut ctx = rep
            .as_ref()
            .and_then(|r| NSGraphicsContext::graphicsContextWithBitmapImageRep(r));

        if ctx.is_none() {
            rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                side,
                side,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                side * 4,
                32,
            );
            premultiplied = true;
            ctx = rep
                .as_ref()
                .and_then(|r| NSGraphicsContext::graphicsContextWithBitmapImageRep(r));
        }

        let rep = rep?;
        let dbg = std::env::var_os("TM_DEBUG_ICON").is_some();
        if dbg {
            eprintln!(
                "[icon] rep={}x{} 预乘={} ctx={:?} image_size={:?}",
                rep.pixelsWide(),
                rep.pixelsHigh(),
                premultiplied,
                ctx.is_some(),
                image.size()
            );
        }
        let ctx = ctx?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));
        image.drawInRect_fromRect_operation_fraction(
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(side as f64, side as f64),
            ),
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            1.0,
        );
        NSGraphicsContext::restoreGraphicsState_class();

        let w = rep.pixelsWide() as u32;
        let h = rep.pixelsHigh() as u32;
        let bpr = rep.bytesPerRow() as usize;
        let spp = rep.samplesPerPixel() as usize;
        let src = rep.bitmapData();
        if src.is_null() || w == 0 || h == 0 || spp < 3 {
            return None;
        }

        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for y in 0..h as usize {
            let row = src.add(y * bpr);
            for x in 0..w as usize {
                let p = row.add(x * spp);
                let (mut r, mut g, mut b) = (*p, *p.add(1), *p.add(2));
                let a = if spp >= 4 { *p.add(3) } else { 255 };
                if premultiplied && a > 0 && a < 255 {
                    // 反预乘，还原成 egui 期望的直通 alpha
                    let inv = |v: u8| (v as u32 * 255 / a as u32).min(255) as u8;
                    r = inv(r);
                    g = inv(g);
                    b = inv(b);
                }
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }

        if std::env::var_os("TM_DEBUG_ICON").is_some() {
            let opaque = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
            eprintln!("[icon] {bundle_path}: {w}x{h} bpr={bpr} spp={spp} 非透明像素={opaque}");
        }

        let bmp = IconBitmap {
            width: w,
            height: h,
            rgba,
        };
        if bmp.is_blank() {
            return None;
        }
        Some(bmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_finder_icon() {
        let path = "/System/Library/CoreServices/Finder.app";
        if !std::path::Path::new(path).exists() {
            eprintln!("跳过：本机没有 Finder.app");
            return;
        }
        let icon = app_icon(path).expect("Finder 应该有图标");
        assert_eq!(icon.width, ICON_SIDE);
        assert_eq!(icon.height, ICON_SIDE);
        assert_eq!(icon.rgba.len(), (ICON_SIDE * ICON_SIDE * 4) as usize);
        // 图标中心必须是不透明的实色，否则说明绘制或读像素这条链断了
        let center = ((ICON_SIDE / 2 * ICON_SIDE + ICON_SIDE / 2) * 4) as usize;
        assert_eq!(icon.rgba[center + 3], 255, "中心像素应该是实心的");
        eprintln!(
            "Finder 图标中心像素：rgb({}, {}, {}) a={}",
            icon.rgba[center],
            icon.rgba[center + 1],
            icon.rgba[center + 2],
            icon.rgba[center + 3]
        );
    }

    #[test]
    fn rejects_non_bundle() {
        assert!(app_icon("/usr/libexec/netbiosd").is_none());
        assert!(app_icon("mDNSResponder").is_none());
        assert!(app_icon("/Applications/不存在的应用.app").is_none());
    }
}
