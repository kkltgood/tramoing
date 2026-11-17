//! 菜单栏图标：一条心跳式波形。
//!
//! 为什么是波形而不是柱状图：柱状条在 18pt 下就是三块色块，既没有信息量也不好看；
//! 波形能表达「有流量在动」，缩放后线条仍然连贯。
//!
//! macOS 上状态栏图标是「模板图」：系统只取 alpha，自行按明暗模式上色，
//! 所以这里只需要画 alpha，颜色恒为黑。
//!
//! 渲染方式：4x 超采样再盒式降采样。整个项目零图像依赖，
//! 为一张 36px 图标引 `image` crate 不值得。

/// 输出边长（像素）。状态栏按 18pt 显示，这是 2x 的尺寸。
const SIZE: u32 = 36;
/// 超采样倍率
const SS: u32 = 4;
const N: usize = (SIZE * SS) as usize;

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

pub fn waveform() -> (Vec<u8>, u32, u32) {
    let mut mask = vec![0f32; N * N];

    for y in 0..N {
        for x in 0..N {
            // 采样点映射回 36x36 坐标
            let px = (x as f32 + 0.5) / SS as f32;
            let py = (y as f32 + 0.5) / SS as f32;
            let hit = SEGMENTS
                .iter()
                .any(|&(x1, y1, x2, y2)| dist_to_segment(px, py, x1, y1, x2, y2) <= RADIUS);
            mask[y * N + x] = if hit { 1.0 } else { 0.0 };
        }
    }

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let samples = (SS * SS) as f32;
    for y in 0..SIZE as usize {
        for x in 0..SIZE as usize {
            let mut acc = 0f32;
            for sy in 0..SS as usize {
                for sx in 0..SS as usize {
                    acc += mask[(y * SS as usize + sy) * N + (x * SS as usize + sx)];
                }
            }
            let alpha = (acc / samples * 255.0).round().clamp(0.0, 255.0) as u8;
            let i = (y * SIZE as usize + x) * 4;
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
            rgba[i + 3] = alpha;
        }
    }

    (rgba, SIZE, SIZE)
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
        assert_eq!((w, h), (SIZE, SIZE));
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
        let soft = buf
            .chunks(4)
            .filter(|p| p[3] > 8 && p[3] < 247)
            .count();
        assert!(soft > 20, "只找到 {soft} 个过渡像素，抗锯齿没生效");
    }
}
