//! 设置页共用的颜色控件：统一行高、即时预览和独立选色面板。

use eframe::egui::{
    self, ecolor::HsvaGamma, Align, Color32, FontId, Id, Rect, Sense, Stroke, Vec2,
};
use tm_core::color::Rgb;

use crate::surface::{self, mix};
use crate::theme::{self, Lang, Palette};

const CONTROL_H: f32 = 28.0;
const PICKER_W: f32 = 288.0;

#[derive(Clone)]
struct PickerState {
    hsv: HsvaGamma,
    color: Rgb,
}

impl PickerState {
    fn new(color: Rgb) -> Self {
        Self {
            hsv: color32(color).into(),
            color,
        }
    }

    fn sync(&mut self, color: Rgb) {
        // 只在外部色值变化时转换，保留黑色、灰色下用户选中的色相。
        if self.color != color {
            *self = Self::new(color);
        }
    }

    fn commit(&mut self, current: &mut Option<Rgb>) {
        let c = Color32::from(self.hsv);
        self.color = Rgb(c.r(), c.g(), c.b());
        *current = Some(self.color);
    }
}

#[derive(Clone)]
struct HexDraft {
    text: String,
    color: Rgb,
}

/// 三个控件共用同一条中线与高度，避免默认组件的内边距影响对齐。
pub fn controls(
    ui: &mut egui::Ui,
    rect: Rect,
    p: &Palette,
    lang: Lang,
    label: &str,
    current: &mut Option<Rgb>,
    automatic: Rgb,
) -> bool {
    let before = *current;
    let state_id = ui.id().with("color-picker");
    let swatch_rect = Rect::from_center_size(
        egui::pos2(rect.left() + 17.0, rect.center().y),
        Vec2::new(34.0, CONTROL_H),
    );
    let reset_rect = Rect::from_center_size(
        egui::pos2(rect.right() - 29.0, rect.center().y),
        Vec2::new(58.0, CONTROL_H),
    );
    let input_rect = Rect::from_min_max(
        egui::pos2(swatch_rect.right() + 8.0, swatch_rect.top()),
        egui::pos2(reset_rect.left() - 8.0, swatch_rect.bottom()),
    );
    let swatch = ui
        .interact(swatch_rect, ui.id().with("swatch"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(lang.t("color.choose_color"));
    swatch.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ColorButton, ui.is_enabled(), label)
    });
    ui.painter().rect(
        swatch_rect,
        7.0,
        mix(p.surface, p.chip, 0.4),
        Stroke::new(0.8_f32, if swatch.hovered() { p.accent } else { p.border }),
        egui::StrokeKind::Inside,
    );
    ui.painter().rect_filled(
        swatch_rect.shrink2(egui::vec2(5.0, 5.0)),
        4.0,
        color32(current.unwrap_or(automatic)),
    );
    surface::focus(ui.painter(), swatch_rect, &swatch, p);
    if swatch.clicked() {
        ui.data_mut(|data| {
            data.insert_temp(state_id, PickerState::new(current.unwrap_or(automatic)));
        });
    }

    hex_field(ui, input_rect, p, lang, current, automatic);
    if button(
        ui,
        reset_rect,
        ui.id().with("reset"),
        lang.t("color.default"),
        p,
        false,
    )
    .on_hover_text(lang.t("color.restore_the_color_for_the_system_theme"))
    .clicked()
    {
        *current = None;
    }

    #[cfg(debug_assertions)]
    if crate::ui_capture::open_color_picker(ui.ctx(), label, lang) {
        ui.data_mut(|data| {
            data.insert_temp(state_id, PickerState::new(current.unwrap_or(automatic)));
        });
    }

    let state = ui.data(|data| data.get_temp::<PickerState>(state_id));
    if let Some(mut state) = state {
        state.sync(current.unwrap_or(automatic));
        let close = picker(
            ui.ctx(),
            state_id,
            p,
            lang,
            label,
            &mut state,
            current,
            automatic,
        );
        ui.data_mut(|data| {
            if close {
                data.remove::<PickerState>(state_id);
            } else {
                data.insert_temp(state_id, state);
            }
        });
    }
    *current != before
}

#[allow(clippy::too_many_arguments)]
fn picker(
    ctx: &egui::Context,
    id: Id,
    p: &Palette,
    lang: Lang,
    label: &str,
    state: &mut PickerState,
    current: &mut Option<Rgb>,
    automatic: Rgb,
) -> bool {
    ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, id.with("shade")))
        .rect_filled(
            ctx.content_rect(),
            20.0,
            Color32::from_black_alpha(if p.dark { 115 } else { 45 }),
        );
    let frame = egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(16)
        .inner_margin(16)
        .shadow(egui::epaint::Shadow {
            offset: [0, 6],
            blur: 20,
            spread: 0,
            color: Color32::from_black_alpha(55),
        });
    let mut close = false;
    let modal = egui::Modal::new(id)
        .frame(frame)
        .backdrop_color(Color32::TRANSPARENT)
        .show(ctx, |ui| {
            ui.set_width(PICKER_W);
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            let header = region(ui, 34.0);
            let title = lang.format("color_picker.title", &[("label", label)]);
            ui.painter().text(
                header.min,
                egui::Align2::LEFT_TOP,
                title,
                theme::display_font(15.0),
                p.text,
            );
            ui.painter().text(
                header.left_bottom(),
                egui::Align2::LEFT_BOTTOM,
                lang.t("color.changes_apply_instantly"),
                FontId::proportional(10.5),
                p.subtext,
            );
            let cross = Rect::from_center_size(
                egui::pos2(header.right() - 13.0, header.top() + 13.0),
                Vec2::splat(26.0),
            );
            let response = ui.interact(cross, id.with("close"), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, lang.t("common.close"))
            });
            ui.painter().rect_filled(cross, 7.0, p.chip);
            let c = cross.center();
            for sign in [-1.0, 1.0] {
                ui.painter().line_segment(
                    [
                        c + egui::vec2(-3.5, -3.5 * sign),
                        c + egui::vec2(3.5, 3.5 * sign),
                    ],
                    Stroke::new(1.3_f32, p.subtext),
                );
            }
            surface::focus(ui.painter(), cross, &response, p);
            close |= response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(lang.t("common.close"))
                .clicked();
            ui.add_space(12.0);
            let sv = region(ui, 126.0);
            let mut edited = saturation_value(ui, sv, p, lang, &mut state.hsv);
            ui.add_space(8.0);
            let hue = region(ui, 24.0);
            edited |= hue_slider(ui, hue, p, lang, &mut state.hsv.h);
            if edited {
                state.commit(current);
            }
            ui.add_space(10.0);
            let caption = region(ui, 16.0);
            ui.painter().text(
                caption.left_center(),
                egui::Align2::LEFT_CENTER,
                lang.t("color.color_presets"),
                FontId::proportional(10.5),
                p.subtext,
            );
            ui.add_space(5.0);
            let palette = region(ui, 28.0);
            let presets = [
                tm_core::color::download(p.dark),
                tm_core::color::upload(p.dark),
                tm_core::color::gpu(p.dark),
                Rgb(238, 111, 168),
                Rgb(245, 158, 74),
                Rgb(238, 199, 83),
                Rgb(20, 28, 43),
                Rgb(241, 245, 255),
            ];
            for (index, color) in presets.into_iter().enumerate() {
                let rect = Rect::from_min_size(
                    palette.min + egui::vec2(index as f32 * (PICKER_W - 28.0) / 7.0, 0.0),
                    Vec2::splat(28.0),
                );
                let response = ui.interact(rect, id.with(("preset", index)), Sense::click());
                let selected = current.unwrap_or(automatic) == color;
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::RadioButton,
                        true,
                        selected,
                        color.to_hex(),
                    )
                });
                ui.painter()
                    .rect_filled(rect.shrink(3.0), 6.0, color32(color));
                if selected || response.hovered() {
                    ui.painter().rect_stroke(
                        rect,
                        8.0,
                        Stroke::new(1.2_f32, if selected { p.accent } else { p.subtext }),
                        egui::StrokeKind::Inside,
                    );
                }
                if selected {
                    let ink = if color.0 as u32 + color.1 as u32 + color.2 as u32 > 440 {
                        Color32::from_rgb(20, 28, 43)
                    } else {
                        Color32::WHITE
                    };
                    ui.painter().add(egui::Shape::line(
                        vec![
                            rect.center() + egui::vec2(-4.0, 0.0),
                            rect.center() + egui::vec2(-1.0, 3.0),
                            rect.center() + egui::vec2(5.0, -3.0),
                        ],
                        Stroke::new(1.5_f32, ink),
                    ));
                }
                surface::focus(ui.painter(), rect, &response, p);
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(color.to_hex())
                    .clicked()
                {
                    *current = Some(color);
                }
            }
            ui.add_space(12.0);
            let values = region(ui, 36.0);
            let preview = Rect::from_min_size(values.min, Vec2::splat(36.0));
            let input = Rect::from_center_size(
                egui::pos2(values.left() + 108.0, values.center().y),
                Vec2::new(116.0, CONTROL_H),
            );
            hex_field(ui, input, p, lang, current, automatic);
            let color = current.unwrap_or(automatic);
            ui.painter().rect(
                preview,
                9.0,
                color32(color),
                Stroke::new(0.7_f32, p.border),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                egui::pos2(values.left() + 182.0, values.top() + 2.0),
                egui::Align2::LEFT_TOP,
                "RGB",
                FontId::proportional(9.5),
                p.unit,
            );
            ui.painter().text(
                egui::pos2(values.left() + 182.0, values.bottom() - 2.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{}, {}, {}", color.0, color.1, color.2),
                FontId::monospace(10.0),
                p.text,
            );
            state.sync(color);
            ui.add_space(12.0);
            let line = region(ui, 1.0);
            ui.painter().line_segment(
                [line.left_center(), line.right_center()],
                Stroke::new(0.7_f32, p.border),
            );
            ui.add_space(10.0);
            let footer = region(ui, CONTROL_H);
            let reset = Rect::from_min_size(footer.min, Vec2::new(104.0, CONTROL_H));
            if button(
                ui,
                reset,
                id.with("reset"),
                lang.t("color.reset_default"),
                p,
                false,
            )
            .clicked()
            {
                *current = None;
                state.sync(automatic);
            }
            let done = Rect::from_min_size(
                egui::pos2(footer.right() - 70.0, footer.top()),
                Vec2::new(70.0, CONTROL_H),
            );
            close |= button(ui, done, id.with("done"), lang.t("color.done"), p, true).clicked();
        });
    close || modal.should_close()
}

fn color32(color: Rgb) -> Color32 {
    Color32::from_rgb(color.0, color.1, color.2)
}

fn region(ui: &mut egui::Ui, height: f32) -> Rect {
    ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover())
        .0
}

fn button(
    ui: &mut egui::Ui,
    rect: Rect,
    id: Id,
    text: &str,
    p: &Palette,
    primary: bool,
) -> egui::Response {
    let response = ui.interact(rect, id, Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), text));
    ui.painter().rect(
        rect,
        7.0,
        if primary {
            p.selection
        } else {
            mix(
                p.surface,
                p.accent,
                if response.hovered() { 0.13 } else { 0.06 },
            )
        },
        Stroke::new(0.7_f32, mix(p.border, p.accent, 0.15)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(10.5),
        if primary { p.on_selection } else { p.accent },
    );
    surface::focus(ui.painter(), rect, &response, p);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn hex_field(
    ui: &mut egui::Ui,
    rect: Rect,
    p: &Palette,
    lang: Lang,
    current: &mut Option<Rgb>,
    automatic: Rgb,
) {
    let id = ui.id().with("hex-input");
    let draft_id = id.with("draft");
    let color = current.unwrap_or(automatic);
    let mut draft = ui
        .data(|data| data.get_temp::<HexDraft>(draft_id))
        .filter(|draft| draft.color == color)
        .unwrap_or_else(|| HexDraft {
            text: color.to_hex(),
            color,
        });
    ui.painter()
        .rect_filled(rect, 7.0, mix(p.surface, p.chip, 0.55));
    let input = ui.put(
        rect,
        egui::TextEdit::singleline(&mut draft.text)
            .id(id)
            .frame(false)
            .font(FontId::monospace(11.0))
            .text_color(p.text)
            .margin(egui::Margin::symmetric(8, 0))
            .vertical_align(Align::Center)
            .min_size(rect.size())
            .char_limit(7)
            .hint_text("#RRGGBB"),
    );
    let parsed = Rgb::from_hex(&draft.text);
    if input.changed() {
        if let Some(color) = parsed {
            *current = Some(color);
            draft.color = color;
        }
    }
    ui.painter().rect_stroke(
        rect,
        7.0,
        Stroke::new(
            0.8_f32,
            if parsed.is_none() {
                p.warn
            } else if input.has_focus() {
                p.accent
            } else {
                p.border
            },
        ),
        egui::StrokeKind::Inside,
    );
    let focused = input.has_focus();
    input.on_hover_text(lang.t("color.enter_a_hex_color_such_as_43d394"));
    ui.data_mut(|data| {
        if focused {
            data.insert_temp(draft_id, draft);
        } else {
            data.remove::<HexDraft>(draft_id);
        }
    });
}

fn saturation_value(
    ui: &mut egui::Ui,
    rect: Rect,
    p: &Palette,
    lang: Lang,
    hsv: &mut HsvaGamma,
) -> bool {
    let before = *hsv;
    let response = ui.interact(
        rect,
        ui.id().with("saturation-value"),
        Sense::click_and_drag(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Slider,
            true,
            lang.t("color.saturation_and_brightness"),
        )
    });
    if let Some(pos) = response.interact_pointer_pos() {
        hsv.s = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        hsv.v = (1.0 - (pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
    }
    if response.has_focus() {
        hsv.s =
            (hsv.s + arrow_delta(ui, egui::Key::ArrowRight, egui::Key::ArrowLeft)).clamp(0.0, 1.0);
        hsv.v = (hsv.v + arrow_delta(ui, egui::Key::ArrowUp, egui::Key::ArrowDown)).clamp(0.0, 1.0);
    }
    gradient(ui.painter(), rect, 8.0, 32, 24, |x, y| {
        HsvaGamma {
            s: x,
            v: 1.0 - y,
            ..*hsv
        }
        .into()
    });
    ui.painter().rect_stroke(
        rect,
        8.0,
        Stroke::new(0.7_f32, p.border),
        egui::StrokeKind::Inside,
    );
    let at = egui::pos2(
        rect.left() + hsv.s * rect.width(),
        rect.bottom() - hsv.v * rect.height(),
    );
    marker(ui.painter(), rect.shrink(6.0).clamp(at), 5.0, (*hsv).into());
    surface::focus(ui.painter(), rect, &response, p);
    response
        .on_hover_cursor(egui::CursorIcon::Crosshair)
        .on_hover_text(lang.t("color.left_right_saturation_up_down_brightness"));
    *hsv != before
}

fn hue_slider(ui: &mut egui::Ui, rect: Rect, p: &Palette, lang: Lang, hue: &mut f32) -> bool {
    let before = *hue;
    let response = ui.interact(rect, ui.id().with("hue"), Sense::click_and_drag());
    response.widget_info(|| {
        egui::WidgetInfo::slider(ui.is_enabled(), *hue as f64 * 360.0, lang.t("color.hue"))
    });
    if let Some(pos) = response.interact_pointer_pos() {
        *hue = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    }
    if response.has_focus() {
        *hue =
            (*hue + arrow_delta(ui, egui::Key::ArrowRight, egui::Key::ArrowLeft)).clamp(0.0, 1.0);
    }
    let track = rect.shrink2(egui::vec2(0.0, 5.0));
    let at = |h| {
        Color32::from(HsvaGamma {
            h,
            s: 1.0,
            v: 1.0,
            a: 1.0,
        })
    };
    gradient(ui.painter(), track, 7.0, 60, 10, |x, _| at(x));
    marker(
        ui.painter(),
        egui::pos2(
            (rect.left() + *hue * rect.width()).clamp(rect.left() + 6.0, rect.right() - 6.0),
            rect.center().y,
        ),
        7.0,
        at(*hue),
    );
    surface::focus(ui.painter(), rect, &response, p);
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(lang.t("color.drag_or_use_left_right_arrows_to_adjust"));
    *hue != before
}

fn arrow_delta(ui: &egui::Ui, increase: egui::Key, decrease: egui::Key) -> f32 {
    ui.input_mut(|input| {
        let modifiers = input.modifiers;
        let step = if modifiers.shift { 0.1 } else { 0.01 };
        (i32::from(input.consume_key(modifiers, increase))
            - i32::from(input.consume_key(modifiers, decrease))) as f32
            * step
    })
}

fn marker(painter: &egui::Painter, at: egui::Pos2, radius: f32, color: Color32) {
    painter.circle_filled(at, radius, color);
    painter.circle_stroke(
        at,
        radius + 0.8,
        Stroke::new(2.5_f32, Color32::from_black_alpha(100)),
    );
    painter.circle_stroke(at, radius, Stroke::new(1.6_f32, Color32::WHITE));
}

/// 网格边界沿圆角收拢，色板与色相条都不会露出方角。
fn gradient(
    painter: &egui::Painter,
    rect: Rect,
    radius: f32,
    columns: u32,
    rows: u32,
    color_at: impl Fn(f32, f32) -> Color32,
) {
    let mut mesh = egui::Mesh::default();
    for row in 0..=rows {
        let y = row as f32 / rows as f32;
        let edge = (radius - (y * rect.height()).min((1.0 - y) * rect.height())).max(0.0);
        let inset = radius - (radius * radius - edge * edge).max(0.0).sqrt();
        for column in 0..=columns {
            let x = (inset + column as f32 / columns as f32 * (rect.width() - 2.0 * inset))
                / rect.width();
            mesh.colored_vertex(
                egui::pos2(
                    rect.left() + x * rect.width(),
                    rect.top() + y * rect.height(),
                ),
                color_at(x, y),
            );
            if row < rows && column < columns {
                let index = row * (columns + 1) + column;
                mesh.add_triangle(index, index + 1, index + columns + 1);
                mesh.add_triangle(index + 1, index + columns + 2, index + columns + 1);
            }
        }
    }
    painter.add(mesh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>, mut draw: impl FnMut(&mut egui::Ui)) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(408.0, 482.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| draw(ui));
            },
        );
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    struct Controls {
        ctx: egui::Context,
        palette: Palette,
        lang: Lang,
        current: Option<Rgb>,
        root: Id,
        background_clicked: bool,
    }

    impl Controls {
        fn new(dark: bool, lang: Lang) -> Self {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            let mut harness = Self {
                ctx,
                palette: theme::palette(dark),
                lang,
                current: None,
                root: Id::NULL,
                background_clicked: false,
            };
            harness.run(vec![]);
            harness
        }

        fn run(&mut self, events: Vec<egui::Event>) {
            frame(&self.ctx, events, |ui| {
                self.root = ui.id();
                self.background_clicked |= ui
                    .interact(
                        Rect::from_min_size(egui::pos2(8.0, 8.0), Vec2::splat(30.0)),
                        ui.id().with("background"),
                        Sense::click(),
                    )
                    .clicked();
                controls(
                    ui,
                    Rect::from_min_size(egui::pos2(150.0, 200.0), Vec2::new(218.0, 30.0)),
                    &self.palette,
                    self.lang,
                    "GPU",
                    &mut self.current,
                    Rgb(67, 211, 148),
                );
            });
        }

        fn click(&mut self, id: Id) {
            let pos = self
                .ctx
                .read_response(id)
                .expect("visible control")
                .rect
                .center();
            self.run(pointer(pos, true));
            self.run(pointer(pos, false));
            // Modal 首次出现后，Area 的尺寸和居中位置在后续帧稳定。
            for _ in 0..3 {
                self.run(vec![]);
            }
        }

        fn is_open(&self) -> bool {
            self.ctx.data(|data| {
                data.get_temp::<PickerState>(self.root.with("color-picker"))
                    .is_some()
            })
        }
    }

    #[test]
    fn presets_apply_immediately_and_reset_restores_automatic_color() {
        for dark in [false, true] {
            for lang in [Lang::ZH_CN, Lang::EN] {
                let mut ui = Controls::new(dark, lang);
                ui.click(ui.root.with("swatch"));
                assert!(ui.is_open());
                assert_eq!(ui.current, None, "opening must preserve automatic colors");
                let picker = ui.root.with("color-picker");
                ui.click(picker.with(("preset", 3_usize)));
                assert_eq!(ui.current, Some(Rgb(238, 111, 168)));
                assert!(
                    ui.is_open(),
                    "choosing a preset should allow further adjustments"
                );
                ui.click(picker.with("reset"));
                assert_eq!(ui.current, None);
                ui.click(picker.with("done"));
                assert!(!ui.is_open());
            }
        }
    }

    #[test]
    fn keyboard_and_backdrop_close_without_activating_background() {
        let mut ui = Controls::new(true, Lang::EN);
        let swatch = ui.root.with("swatch");
        ui.ctx.memory_mut(|memory| memory.request_focus(swatch));
        ui.run(vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        ui.run(vec![]);
        assert!(ui.is_open());
        ui.run(vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        ui.run(vec![]);
        assert!(!ui.is_open());
        ui.click(swatch);
        assert!(ui.is_open());
        ui.run(pointer(egui::pos2(20.0, 20.0), true));
        ui.run(pointer(egui::pos2(20.0, 20.0), false));
        assert!(!ui.is_open());
        assert!(!ui.background_clicked);
        assert_eq!(ui.current, None);
    }

    #[test]
    fn incomplete_hex_drafts_do_not_replace_the_last_valid_color() {
        let mut ui = Controls::new(true, Lang::EN);
        let input = ui.root.with("hex-input");
        let command = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        ui.ctx.memory_mut(|memory| memory.request_focus(input));
        ui.run(vec![
            key(egui::Key::A, command),
            egui::Event::Paste("#12".into()),
        ]);
        assert_eq!(ui.current, None);
        ui.run(vec![egui::Event::Text("ABEF".into())]);
        assert_eq!(ui.current, Some(Rgb(18, 171, 239)));
        ui.run(vec![
            key(egui::Key::A, command),
            egui::Event::Paste("#GGGGGG".into()),
        ]);
        assert_eq!(ui.current, Some(Rgb(18, 171, 239)));
        ui.ctx.memory_mut(|memory| memory.surrender_focus(input));
        ui.run(vec![]);
        assert!(ui
            .ctx
            .data(|data| data.get_temp::<HexDraft>(input.with("draft")).is_none()));
        ui.click(ui.root.with("reset"));
        assert_eq!(ui.current, None);
    }

    #[test]
    fn hue_survives_black_and_keyboard_and_drag_change_the_color() {
        let ctx = egui::Context::default();
        let p = theme::palette(true);
        let mut state = PickerState::new(Rgb(0, 0, 0));
        let mut current = Some(state.color);
        let mut root = Id::NULL;
        let field = Rect::from_min_size(egui::pos2(40.0, 40.0), Vec2::new(288.0, 126.0));
        let hue = Rect::from_min_size(egui::pos2(40.0, 180.0), Vec2::new(288.0, 24.0));
        let mut run = |events| {
            frame(&ctx, events, |ui| {
                root = ui.id();
                state.sync(current.unwrap());
                let changed = saturation_value(ui, field, &p, Lang::EN, &mut state.hsv)
                    | hue_slider(ui, hue, &p, Lang::EN, &mut state.hsv.h);
                if changed {
                    state.commit(&mut current);
                }
            });
            (state.clone(), current, root)
        };
        run(vec![]);
        let blue = egui::pos2(hue.left() + hue.width() * 2.0 / 3.0, hue.center().y);
        run(pointer(blue, true));
        let (chosen, current, root) = run(pointer(blue, false));
        assert_eq!(current, Some(Rgb(0, 0, 0)));
        assert!((chosen.hsv.h - 2.0 / 3.0).abs() < 0.001);
        ctx.memory_mut(|memory| memory.request_focus(root.with("saturation-value")));
        let (brightened, current, _) = run(vec![key(egui::Key::ArrowUp, egui::Modifiers::NONE)]);
        assert!(brightened.hsv.v > 0.0);
        assert_eq!(brightened.hsv.h, chosen.hsv.h);
        assert_ne!(current, Some(Rgb(0, 0, 0)));
        run(pointer(field.center(), true));
        let (_, current, _) = run(vec![egui::Event::PointerMoved(
            field.right_top() + egui::vec2(20.0, -20.0),
        )]);
        assert_eq!(
            current,
            Some(Rgb(0, 0, 255)),
            "dragging beyond the field clamps to the edge"
        );
    }
}
