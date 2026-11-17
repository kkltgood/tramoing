//! 应用图标生成器（一次性工具，产物在 `assets/AppIcon.icns`）。
//!
//! 设计：深色圆角方块 + 一条蓝色波形。波形和菜单栏图标是同一个形状，
//! 缩小到 16px 还能认出来——这是图标最重要的性质。
//!
//! 为什么手写光栅化：整个项目零图像依赖，为一张图标引入 `image` crate 不值得。
//! 这里用距离场直接算覆盖率（每个像素算到折线的最近距离），比超采样快且无噪点。
//!
//! 用法（改了图标之后）：
//!   rm -f assets/AppIcon.icns assets/icon-1024.png
//!   ./scripts/package-macos.sh
//! 脚本会自动重跑本文件生成 .bmp，再转 png 和 icns。

use std::fs::File;
use std::io::{BufWriter, Write};

const SIZE: usize = 1024;

type Rgb = [f32; 3];

fn main() {
    let mut buf = vec![[0f32; 3]; SIZE * SIZE];

    // ---- 背景：圆角方块 + 上亮下暗的渐变 ----
    let inset = 44.0;
    let radius = 210.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if !in_round_rect(px, py, inset, inset, SIZE as f32 - inset, SIZE as f32 - inset, radius)
            {
                continue;
            }
            let t = ((py - inset) / (SIZE as f32 - 2.0 * inset)).clamp(0.0, 1.0);
            buf[y * SIZE + x] = lerp([0.13, 0.17, 0.24], [0.04, 0.06, 0.10], t);
        }
    }

    // ---- 波形：先铺一层低透明度做辉光，再画实心 ----
    const SCALE: f32 = 28.0;
    const SEG: [(f32, f32, f32, f32); 5] = [
        (4.5, 21.0, 9.5, 21.0),
        (9.5, 21.0, 13.5, 11.5),
        (13.5, 11.5, 18.5, 25.5),
        (18.5, 25.5, 22.5, 15.0),
        (22.5, 15.0, 31.5, 15.0),
    ];
    let stroke = 2.1 * SCALE;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            let d = SEG
                .iter()
                .map(|&(x1, y1, x2, y2)| {
                    dist_to_segment(px, py, x1 * SCALE, y1 * SCALE, x2 * SCALE, y2 * SCALE)
                })
                .fold(f32::MAX, f32::min);

            // 抗锯齿：距离场 ±0.5 像素内做线性过渡
            let glow = coverage(d - stroke - 16.0) * 0.20;
            let core = coverage(d - stroke);

            if glow > 0.001 {
                let c = wave_color(px);
                let dst = &mut buf[y * SIZE + x];
                for i in 0..3 {
                    dst[i] = c[i] * glow + dst[i] * (1.0 - glow);
                }
            }
            if core > 0.001 {
                let c = wave_color(px);
                let dst = &mut buf[y * SIZE + x];
                for i in 0..3 {
                    dst[i] = c[i] * core + dst[i] * (1.0 - core);
                }
            }
        }
    }

    let rgba = to_rgba(&buf);
    write_bmp("assets/icon-1024.bmp", SIZE, SIZE, &rgba).expect("写 BMP 失败");
    println!("已生成 assets/icon-1024.bmp（{SIZE}x{SIZE}）");
}

/// 波形颜色：从左到右由深蓝渐变到亮青，和界面里的强调色同族。
fn wave_color(x: f32) -> Rgb {
    let t = ((x - 120.0) / 780.0).clamp(0.0, 1.0);
    lerp([0.29, 0.62, 1.0], [0.56, 0.85, 1.0], t)
}

fn coverage(signed_dist: f32) -> f32 {
    (0.5 - signed_dist).clamp(0.0, 1.0)
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

fn in_round_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if px < x0 || px > x1 || py < y0 || py > y1 {
        return false;
    }
    let (dx, dy) = (px - px.clamp(x0 + r, x1 - r), py - py.clamp(y0 + r, y1 - r));
    dx * dx + dy * dy <= r * r
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn to_rgba(buf: &[Rgb]) -> Vec<u8> {
    let mut out = vec![0u8; buf.len() * 4];
    for (i, c) in buf.iter().enumerate() {
        out[i * 4] = (c[0] * 255.0).round().clamp(0.0, 255.0) as u8;
        out[i * 4 + 1] = (c[1] * 255.0).round().clamp(0.0, 255.0) as u8;
        out[i * 4 + 2] = (c[2] * 255.0).round().clamp(0.0, 255.0) as u8;
        // 圆角之外是透明的
        out[i * 4 + 3] = if c[0] + c[1] + c[2] > 0.004 { 255 } else { 0 };
    }
    out
}

/// 32 位 BMP，行序自下而上，BGRA。sips 能直接读，再转 PNG。
fn write_bmp(path: &str, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<()> {
    let row = w * 4;
    let data_size = row * h;
    let mut f = BufWriter::new(File::create(path)?);

    f.write_all(b"BM")?;
    f.write_all(&((54 + data_size) as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(h as i32).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(data_size as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    for y in (0..h).rev() {
        for x in 0..w {
            let i = (y * w + x) * 4;
            f.write_all(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]])?;
        }
    }
    f.flush()
}
