//! 托盘图标：一条心跳式波形。
//!
//! 为什么是波形而不是柱状图：柱状条在 18pt 下就是三块色块，既没有信息量也不好看；
//! 波形能表达「有流量在动」，缩放后线条仍然连贯。
//!
//! macOS 上状态栏图标是「模板图」：系统只取 alpha，自行按明暗模式上色，
//! 所以这里只需要画 alpha，颜色恒为黑。Windows 不是这样 —— 它照原样画，
//! 深色任务栏上黑图标等于隐形，所以那条路径要自带颜色（见 [`waveform_tinted`]）。
//!
//! 渲染方式：4x 超采样再盒式降采样。整个项目零图像依赖，
//! 为一张图标引 `image` crate 不值得。

/// 设计稿坐标系边长。所有顶点都按这个尺寸写，输出时整体缩放。
const DESIGN: f32 = 36.0;
/// 超采样倍率
const SS: u32 = 4;

/// 波形的折线顶点，坐标在 36x36 的空间里。
const SEGMENTS: [(f32, f32, f32, f32); 5] = [
    (4.5, 21.0, 9.5, 21.0),
    (9.5, 21.0, 13.5, 11.5),
    (13.5, 11.5, 18.5, 25.5),
    (18.5, 25.5, 22.5, 15.0),
    (22.5, 15.0, 31.5, 15.0),
];

/// 线宽的一半
const RADIUS: f32 = 2.1;

/// macOS 菜单栏用：36px 纯黑，交给系统当模板图重新上色。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn waveform() -> (Vec<u8>, u32, u32) {
    render(36, [0, 0, 0])
}

/// Windows 托盘用：自带颜色，不参与模板反色。
///
/// 尺寸取 32 是因为任务栏的图标槽在 100%/125% 缩放下是 16/20px，
/// 交给系统缩一张 32px 的位图比从 36px 缩更干净。
// macOS 走模板图，用不到染色版本（单测里会用到，所以只在非测试构建里放行）
#[cfg_attr(all(target_os = "macos", not(test)), allow(dead_code))]
pub fn waveform_tinted(size: u32, rgb: [u8; 3]) -> (Vec<u8>, u32, u32) {
    render(size, rgb)
}

/// 系统服务用的齿轮。
///
/// 列表里那些没有 `.app` 外壳的守护进程（`mDNSResponder`、`netbiosd`）系统
/// 根本不给图标，统一发一只齿轮：既是「机器」的通用符号，又不像首字母块那样
/// 会和某个应用的品牌色撞上。
///
/// 形状在 [`DESIGN`] 坐标系里定义：8 个齿、齿顶半径 13.9、齿根圆 9.4、中心孔 4.4。
/// 这组数字是照着 18pt 的观感调的 —— 齿再细会糊成一圈毛边，孔再小就像颗太阳。
///
/// 输出纯白（`alpha` 才是形状），明暗主题由调用方用 `painter.image` 的 tint 染色。
/// 中心孔是真正的透明，底下的色块能透上来。
pub fn gear(size: u32) -> (Vec<u8>, u32, u32) {
    /// 齿数
    const TEETH: f32 = 8.0;
    /// 齿顶半径
    const OUTER: f32 = 13.9;
    /// 齿的切向半宽
    const HALF_WIDTH: f32 = 2.7;
    /// 齿根圆（里面是实心轮盘）
    const CORE: f32 = 9.4;
    /// 中心孔
    const HOLE: f32 = 4.4;

    let n = (size * SS) as usize;
    let k = size as f32 / DESIGN;
    let centre = DESIGN / 2.0;
    let step = std::f32::consts::TAU / TEETH;

    let mut mask = vec![0f32; n * n];
    for y in 0..n {
        for x in 0..n {
            let px = (x as f32 + 0.5) / SS as f32 / k;
            let py = (y as f32 + 0.5) / SS as f32 / k;
            let (dx, dy) = (px - centre, py - centre);
            let r = (dx * dx + dy * dy).sqrt();
            let hit = if r <= HOLE {
                false
            } else if r <= CORE {
                true
            } else {
                // 齿按角度等分：把角度折回到「离最近齿心多远」，再拆成切向 /
                // 径向两个分量，一个齿就退化成一次矩形判定
                let local = (dy.atan2(dx) + step / 2.0).rem_euclid(step) - step / 2.0;
                r * local.cos() <= OUTER && (r * local.sin()).abs() <= HALF_WIDTH
            };
            mask[y * n + x] = if hit { 1.0 } else { 0.0 };
        }
    }

    downsample(&mask, size, [255, 255, 255])
}

/// 超采样遮罩 → 目标尺寸的 RGBA（盒式降采样）。
fn downsample(mask: &[f32], size: u32, rgb: [u8; 3]) -> (Vec<u8>, u32, u32) {
    let n = (size * SS) as usize;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let samples = (SS * SS) as f32;
    for y in 0..size as usize {
        for x in 0..size as usize {
            let mut acc = 0f32;
            for sy in 0..SS as usize {
                for sx in 0..SS as usize {
                    acc += mask[(y * SS as usize + sy) * n + (x * SS as usize + sx)];
                }
            }
            let alpha = (acc / samples * 255.0).round().clamp(0.0, 255.0) as u8;
            let i = (y * size as usize + x) * 4;
            rgba[i] = rgb[0];
            rgba[i + 1] = rgb[1];
            rgba[i + 2] = rgb[2];
            rgba[i + 3] = alpha;
        }
    }

    (rgba, size, size)
}

fn render(size: u32, rgb: [u8; 3]) -> (Vec<u8>, u32, u32) {
    let n = (size * SS) as usize;
    // 设计单位 → 输出像素的缩放比
    let k = size as f32 / DESIGN;

    let mut mask = vec![0f32; n * n];
    for y in 0..n {
        for x in 0..n {
            // 采样点映射回 36x36 的设计坐标
            let px = (x as f32 + 0.5) / SS as f32 / k;
            let py = (y as f32 + 0.5) / SS as f32 / k;
            let hit = SEGMENTS
                .iter()
                .any(|&(x1, y1, x2, y2)| dist_to_segment(px, py, x1, y1, x2, y2) <= RADIUS);
            mask[y * n + x] = if hit { 1.0 } else { 0.0 };
        }
    }

    downsample(&mask, size, rgb)
}

fn dist_to_segment(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((px - x1) * dx + (py - y1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (ex, ey) = (px - (x1 + dx * t), py - (y1 + dy * t));
    (ex * ex + ey * ey).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_buffer_is_well_formed() {
        let (buf, w, h) = waveform();
        assert_eq!(buf.len(), (w * h * 4) as usize);
        assert_eq!((w, h), (36, 36));
    }

    #[test]
    fn icon_has_ink_but_does_not_fill_the_canvas() {
        let (buf, w, h) = waveform();
        let painted = buf.chunks(4).filter(|p| p[3] > 8).count();
        let total = (w * h) as usize;
        assert!(painted > 150, "只画了 {painted} 个像素，波形可能没画上");
        assert!(
            painted < total / 2,
            "涂了 {painted}/{total} 个像素，图标太实，不像线条"
        );
    }

    #[test]
    fn edges_are_antialiased() {
        // 超采样降采样后必须出现「半透明」的边缘像素，否则会是一锯齿
        let (buf, _, _) = waveform();
        let soft = buf.chunks(4).filter(|p| p[3] > 8 && p[3] < 247).count();
        assert!(soft > 20, "只找到 {soft} 个过渡像素，抗锯齿没生效");
    }

    #[test]
    fn tinted_variant_carries_color_and_keeps_shape() {
        // Windows 的托盘图标不参与模板反色，颜色必须真的写进像素里
        let (buf, w, h) = waveform_tinted(32, [255, 255, 255]);
        assert_eq!((w, h), (32, 32));
        assert_eq!(buf.len(), 32 * 32 * 4);
        let opaque: Vec<_> = buf.chunks(4).filter(|p| p[3] > 200).collect();
        assert!(!opaque.is_empty(), "缩到 32px 后波形没了");
        assert!(
            opaque
                .iter()
                .all(|p| p[0] == 255 && p[1] == 255 && p[2] == 255),
            "染色没有落到不透明像素上"
        );
        // 放大缩小后仍然只占画布的一小部分，不能糊成一块
        let painted = buf.chunks(4).filter(|p| p[3] > 8).count();
        assert!(painted < (32 * 32) / 2);
    }

    #[test]
    fn gear_has_hollow_hub_and_separated_teeth() {
        let (buf, w, _) = gear(36);
        let alpha = |x: usize, y: usize| buf[(y * 36 + x) * 4 + 3];

        assert_eq!(alpha(18, 18), 0, "中心孔必须是完全透明的，否则像颗太阳");
        assert!(alpha(18, 12) > 200, "轮盘（半径 6）没画上");
        // 齿心在 0°，齿顶半径 12 处应该有墨
        assert!(alpha(30, 18) > 200, "0° 方向没有齿");
        // 两个齿之间（半个齿距）同样半径处应该是空的
        let gap = 12.0f32;
        let gap_angle = std::f32::consts::TAU / 16.0;
        let (gx, gy) = (
            (18.0 + gap * gap_angle.cos()).round() as usize,
            (18.0 + gap * gap_angle.sin()).round() as usize,
        );
        assert_eq!(alpha(gx, gy), 0, "齿与齿之间没分开，画成了实心圆");

        // 齿顶外沿要留白，否则贴边会被行盒裁掉
        assert_eq!(alpha(1, 18), 0);
        assert_eq!(alpha(34, 18), 0);
        assert_eq!(w, 36);
    }
}
