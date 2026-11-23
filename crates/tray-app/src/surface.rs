//! 两页共用的圆角渐变、选中胶囊和流量曲线，全部用原生矢量绘制。

use eframe::egui::{self, Color32, CornerRadius, Painter, Pos2, Rect, Stroke};

use crate::theme::Palette;

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
        channel(a.a(), b.a()),
    )
}

/// 在圆角边界内插值颜色，透明窗口的四角不需要额外遮罩。
pub fn gradient(painter: &Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let mut mesh = egui::Mesh::default();
    let color_at = |y: f32| mix(top, bottom, (y - rect.top()) / rect.height().max(1.0));
    mesh.colored_vertex(rect.center(), color_at(rect.center().y));
    for (center, start) in [
        (
            egui::pos2(rect.right() - radius, rect.top() + radius),
            -90.0_f32,
        ),
        (
            egui::pos2(rect.right() - radius, rect.bottom() - radius),
            0.0,
        ),
        (
            egui::pos2(rect.left() + radius, rect.bottom() - radius),
            90.0,
        ),
        (egui::pos2(rect.left() + radius, rect.top() + radius), 180.0),
    ] {
        for i in 0..=12 {
            let angle = (start + i as f32 * 7.5).to_radians();
            let pos = center + egui::vec2(angle.cos(), angle.sin()) * radius;
            mesh.colored_vertex(pos, color_at(pos.y));
        }
    }
    let end = mesh.vertices.len() as u32;
    for i in 1..end {
        mesh.add_triangle(0, i, if i + 1 == end { 1 } else { i + 1 });
    }
    painter.add(mesh);
}

pub fn panel(painter: &Painter, rect: Rect, radius: u8, p: &Palette) {
    gradient(painter, rect, radius as f32, p.panel_top, p.panel_bottom);
    // 两层低对比的波纹呼应流量曲线；位置固定，不产生持续重绘。
    for layer in 0..2 {
        let points: Vec<_> = (0..=96)
            .map(|i| {
                let t = i as f32 / 96.0;
                let wave = ((t * 2.4 + layer as f32 * 0.48) * std::f32::consts::PI).sin();
                let height = (22.0 + wave * 18.0) * if layer == 0 { 1.0 } else { 0.65 };
                egui::pos2(rect.left() + t * rect.width(), rect.bottom() - height)
            })
            .collect();
        area(
            painter,
            rect,
            radius as f32,
            &points,
            p.wave.gamma_multiply(0.50),
            0.8,
        );
    }
    painter.rect_stroke(
        rect.shrink(0.5),
        CornerRadius::same(radius),
        Stroke::new(1.0_f32, if p.dark { p.border } else { Color32::WHITE }),
        egui::StrokeKind::Inside,
    );
}

pub fn pill(painter: &Painter, rect: Rect, p: &Palette) {
    let radius = rect.height() / 2.0;
    painter.rect_filled(
        rect.translate(egui::vec2(0.0, 1.0)).expand(1.5),
        radius + 1.5,
        p.accent.gamma_multiply(if p.dark { 0.12 } else { 0.10 }),
    );
    gradient(painter, rect, radius, p.selection_top, p.selection);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(0.7_f32, mix(p.selection_top, Color32::WHITE, 0.25)),
        egui::StrokeKind::Inside,
    );
}

pub fn focus(painter: &Painter, rect: Rect, response: &egui::Response, p: &Palette) {
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(0.5),
            7.0,
            Stroke::new(1.5_f32, p.accent),
            egui::StrokeKind::Inside,
        );
    }
}

/// 曲线下面积只填到圆角边界；卡片底部仍然保留透明的圆角。
fn area(painter: &Painter, rect: Rect, radius: f32, points: &[Pos2], color: Color32, fade: f32) {
    let mut mesh = egui::Mesh::default();
    for point in points {
        let dx = (radius - (point.x - rect.left()).min(rect.right() - point.x)).max(0.0);
        let inset = radius - (radius * radius - dx * dx).max(0.0).sqrt();
        let bottom = rect.bottom() - inset;
        let top = point.y.clamp(rect.top() + inset, bottom);
        mesh.colored_vertex(egui::pos2(point.x, top), color);
        mesh.colored_vertex(egui::pos2(point.x, bottom), color.gamma_multiply(fade));
    }
    for i in 0..points.len().saturating_sub(1) as u32 {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    painter.add(mesh);
}

pub fn sparkline(painter: &Painter, rect: Rect, values: &[f64], color: Color32, dark: bool) {
    if values.len() < 2 {
        return;
    }
    let max = values.iter().copied().fold(1.0_f64, f64::max);
    let normalized: Vec<f32> = values
        .iter()
        .map(|v| (v / max).clamp(0.0, 1.0) as f32)
        .collect();
    // 保持相邻样本的单调性，又让接点斜率连续；不会在两个点之间造出尖峰。
    let slope = |i: usize| {
        if i == 0 {
            return normalized[1] - normalized[0];
        }
        if i + 1 == normalized.len() {
            return normalized[i] - normalized[i - 1];
        }
        let before = normalized[i] - normalized[i - 1];
        let after = normalized[i + 1] - normalized[i];
        if before * after <= 0.0 {
            0.0
        } else {
            2.0 * before * after / (before + after)
        }
    };
    let step = rect.width() / (crate::engine::TREND_SAMPLES - 1) as f32;
    let left = rect.right() - (values.len() - 1) as f32 * step;
    // 紧凑卡片只在数值下方留一条趋势带，避免曲线穿过网速文字。
    let baseline = rect.bottom() - 6.0;
    let amplitude = 12.0;
    let mut points = Vec::with_capacity(values.len() * 6);
    for i in 0..values.len() - 1 {
        for sub in 0..6 {
            let t = sub as f32 / 6.0;
            let t2 = t * t;
            let t3 = t2 * t;
            let v = (2.0 * t3 - 3.0 * t2 + 1.0) * normalized[i]
                + (t3 - 2.0 * t2 + t) * slope(i)
                + (-2.0 * t3 + 3.0 * t2) * normalized[i + 1]
                + (t3 - t2) * slope(i + 1);
            points.push(egui::pos2(
                left + (i as f32 + t) * step,
                baseline - v.clamp(0.0, 1.0) * amplitude,
            ));
        }
    }
    points.push(egui::pos2(
        rect.right(),
        baseline - normalized[normalized.len() - 1] * amplitude,
    ));
    area(
        painter,
        rect,
        12.0,
        &points,
        color.gamma_multiply(if dark { 0.25 } else { 0.20 }),
        0.05,
    );
    painter.add(egui::Shape::line(
        points,
        Stroke::new(1.4_f32, color.gamma_multiply(0.85)),
    ));
}

pub fn direction(painter: &Painter, center: Pos2, down: bool, size: f32, color: Color32) {
    let d = if down { 1.0 } else { -1.0 };
    let stroke = Stroke::new(1.7_f32, color);
    painter.line_segment(
        [
            center - egui::vec2(0.0, size * d),
            center + egui::vec2(0.0, size * d),
        ],
        stroke,
    );
    painter.add(egui::Shape::line(
        vec![
            center + egui::vec2(-size * 0.72, size * 0.28 * d),
            center + egui::vec2(0.0, size * d),
            center + egui::vec2(size * 0.72, size * 0.28 * d),
        ],
        stroke,
    ));
}

pub fn network_mark(painter: &Painter, rect: Rect, p: &Palette) {
    for i in 0..3 {
        let height = 8.0 + i as f32 * 4.5;
        let bar = Rect::from_min_size(
            egui::pos2(
                rect.left() + 3.0 + i as f32 * 6.0,
                rect.center().y + 8.5 - height,
            ),
            egui::vec2(4.2, height),
        );
        gradient(painter, bar, 2.1, p.upload, p.accent);
    }
}
