//! Game overlay controls are deliberately separate from taskbar preferences.
use super::*;
use tm_core::color::Rgb;

impl App {
    pub(super) fn sync_game_overlay(&mut self, ctx: &egui::Context) {
        let network = self.latest.as_ref().and_then(|snapshot| {
            snapshot
                .error
                .is_none()
                .then_some((snapshot.total_rx, snapshot.total_tx))
        });
        if self
            .game_overlay
            .update(&mut self.settings.game_overlay, network, self.lang)
        {
            self.settings.save();
        }
        if self.settings.game_overlay.enabled {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    pub(super) fn game_overlay_options(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        changed: &mut bool,
    ) {
        settings_group(ui, p, "game-overlay", lang.t("overlay.title"), |ui| {
            *changed |= toggle_row(
                ui,
                p,
                lang.t("overlay.enable"),
                &mut self.settings.game_overlay.enabled,
            );
            settings_note(ui, p, lang.t("overlay.independent_short"))
                .on_hover_text(lang.t("overlay.independent"));
            settings_note(ui, p, lang.t("overlay.compatibility_short"))
                .on_hover_text(lang.t("overlay.fullscreen_hint"));
            if let Some(error) = &self.game_overlay.error {
                settings_note(ui, p, lang.t("overlay.window_failed")).on_hover_text(error);
            }
        });
        ui.add_space(8.0);
        settings_group(
            ui,
            p,
            "overlay-metrics",
            lang.t("overlay.metrics_colors"),
            |ui| {
                settings_note(ui, p, lang.t("overlay.color_help"))
                    .on_hover_text(lang.t("overlay.colors_hint"));
                let config = &mut self.settings.game_overlay;
                *changed |= metric_row(
                    ui,
                    p,
                    lang,
                    lang.t("overlay.frame_rate_label"),
                    &mut config.show_fps,
                    &mut config.fps_color,
                    crate::game_overlay::FPS_COLOR,
                );
                let fps_help = format!(
                    "{}\n{}",
                    lang.t("overlay.fps_summary"),
                    lang.t("overlay.fps_tracking_short"),
                );
                let mut fps_detail = lang.t("overlay.fps_hint").to_owned();
                if config.enabled && config.show_fps {
                    let (key, detail) = self.game_overlay.fps_status();
                    fps_detail.push_str(&format!("\n{}\n{detail}", lang.t(key)));
                }
                settings_note(ui, p, &fps_help).on_hover_text(fps_detail);
                settings_separator(ui, p);
                *changed |= metric_row(
                    ui,
                    p,
                    lang,
                    "CPU",
                    &mut config.show_cpu,
                    &mut config.colors.cpu,
                    tm_core::color::cpu(true),
                );
                *changed |= metric_row(
                    ui,
                    p,
                    lang,
                    "GPU",
                    &mut config.show_gpu,
                    &mut config.colors.gpu,
                    tm_core::color::gpu(true),
                );
                settings_separator(ui, p);
                *changed |= toggle_row(ui, p, lang.t("overlay.network"), &mut config.show_network);
                let (row, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 34.0),
                    egui::Sense::hover(),
                );
                let half = (row.width() - 12.0) / 2.0;
                let left = egui::Rect::from_min_size(row.min, egui::vec2(half, row.height()));
                let right = egui::Rect::from_min_size(
                    egui::pos2(row.left() + half + 12.0, row.top()),
                    left.size(),
                );
                *changed |= network_color(
                    ui,
                    left,
                    p,
                    lang,
                    "download",
                    lang.t("network.download"),
                    &mut config.colors.download,
                    tm_core::color::download(true),
                );
                *changed |= network_color(
                    ui,
                    right,
                    p,
                    lang,
                    "upload",
                    lang.t("network.upload"),
                    &mut config.colors.upload,
                    tm_core::color::upload(true),
                );
                settings_separator(ui, p);
                *changed |= toggle_row(
                    ui,
                    p,
                    lang.t("ui.show_temperature"),
                    &mut config.show_temperature,
                );
                settings_note(ui, p, lang.t("overlay.sensor_short"))
                    .on_hover_text(lang.t("overlay.sensor_hint"));
                let showing_temperature = config.enabled
                    && config.show_temperature
                    && (config.show_cpu || config.show_gpu);
                let pending_components = self.sensor_components.is_busy()
                    || matches!(
                        self.sensor_components.phase,
                        crate::sensor_components::Phase::Paused { .. }
                            | crate::sensor_components::Phase::Ready { .. }
                    );
                // Keep progress, pause and follow-up actions reachable after hiding
                // temperatures. An idle, disabled overlay must not inspect components.
                if showing_temperature || pending_components {
                    ui.add_space(2.0);
                    self.sensor_component_controls(ui, p, lang);
                }
            },
        );
        ui.add_space(8.0);
        settings_group(
            ui,
            p,
            "overlay-position",
            lang.t("overlay.position_title"),
            |ui| {
                let config = &mut self.settings.game_overlay;
                let (lock_row, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 48.0),
                    egui::Sense::hover(),
                );
                let mut switch_ui =
                    ui.new_child(egui::UiBuilder::new().max_rect(lock_row).id_salt("lock"));
                *changed |= toggle_row_at(
                    &mut switch_ui,
                    egui::Rect::from_min_size(lock_row.min, egui::vec2(lock_row.width(), 28.0)),
                    p,
                    lang.t("overlay.lock_short"),
                    &mut config.locked,
                );
                ui.painter().text(
                    lock_row.min + egui::vec2(2.0, 26.0),
                    egui::Align2::LEFT_TOP,
                    lang.t("overlay.drag_short"),
                    egui::FontId::proportional(10.0),
                    p.subtext,
                );
                settings_separator(ui, p);
                *changed |=
                    percentage_row(ui, p, &mut config.x_percent, lang.t("overlay.horizontal"));
                *changed |=
                    percentage_row(ui, p, &mut config.y_percent, lang.t("overlay.vertical"));
                ui.add_space(6.0);
                let (footer, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 28.0),
                    egui::Sense::hover(),
                );
                let reset_rect = egui::Rect::from_min_size(
                    egui::pos2(footer.right() - 108.0, footer.top()),
                    egui::vec2(108.0, 28.0),
                );
                let hint_rect = egui::Rect::from_min_max(
                    footer.min,
                    egui::pos2(reset_rect.left() - 8.0, footer.bottom()),
                );
                ellipsis_text(
                    ui.painter(),
                    hint_rect,
                    lang.t("overlay.position_short"),
                    10.0,
                    p.subtext,
                    Align::Min,
                );
                ui.interact(
                    hint_rect,
                    ui.id().with("position-help"),
                    egui::Sense::hover(),
                )
                .on_hover_text(lang.t("overlay.coordinates"));
                let mut reset_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(reset_rect)
                        .id_salt("reset-position"),
                );
                if settings_action(&mut reset_ui, p, lang.t("overlay.reset_position"), true)
                    .clicked()
                {
                    config.x = 24;
                    config.y = 24;
                    config.x_percent = 200;
                    config.y_percent = 200;
                    *changed = true;
                }
            },
        );
        ui.add_space(8.0);
        settings_group(
            ui,
            p,
            "overlay-appearance",
            lang.t("overlay.appearance_title"),
            |ui| {
                let config = &mut self.settings.game_overlay;
                let mut font = f64::from(config.font_size);
                if slider_row(
                    ui,
                    p,
                    lang.t("overlay.font_size"),
                    &mut font,
                    12.0..=32.0,
                    1.0,
                    "",
                ) {
                    config.font_size = font.round() as u16;
                    *changed = true;
                }
                let mut opacity = f64::from(config.opacity);
                if slider_row(
                    ui,
                    p,
                    lang.t("overlay.opacity_short"),
                    &mut opacity,
                    20.0..=100.0,
                    1.0,
                    "%",
                ) {
                    config.opacity = opacity.round() as u8;
                    *changed = true;
                }
            },
        );
        ui.add_space(12.0);
    }
}

#[allow(clippy::too_many_arguments)]
fn metric_row(
    ui: &mut egui::Ui,
    p: &Palette,
    lang: Lang,
    label: &str,
    enabled: &mut bool,
    color: &mut Option<Rgb>,
    automatic: Rgb,
) -> bool {
    ui.push_id(("overlay-metric", label), |ui| {
        let (row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), egui::Sense::hover());
        let chip = egui::Rect::from_center_size(
            egui::pos2(row.right() - 51.0, row.center().y),
            egui::vec2(102.0, 28.0),
        );
        // Choosing a color must not also toggle this metric's visibility.
        let toggle =
            egui::Rect::from_min_max(row.min, egui::pos2(chip.left() - 12.0, row.bottom()));
        let visible_changed = toggle_row_at(ui, toggle, p, label, enabled);
        let color_changed =
            crate::color_picker::compact_controls(ui, chip, p, lang, label, color, automatic);
        visible_changed || color_changed
    })
    .inner
}

#[allow(clippy::too_many_arguments)]
fn network_color(
    ui: &mut egui::Ui,
    row: egui::Rect,
    p: &Palette,
    lang: Lang,
    id: &str,
    label: &str,
    color: &mut Option<Rgb>,
    automatic: Rgb,
) -> bool {
    ui.push_id(("overlay-network-color", id), |ui| {
        let chip = egui::Rect::from_center_size(
            egui::pos2(row.right() - 51.0, row.center().y),
            egui::vec2(102.0, 28.0),
        );
        let label_rect =
            egui::Rect::from_min_max(row.min, egui::pos2(chip.left() - 5.0, row.bottom()));
        ellipsis_text(ui.painter(), label_rect, label, 11.0, p.subtext, Align::Min);
        crate::color_picker::compact_controls(ui, chip, p, lang, label, color, automatic)
    })
    .inner
}

fn percentage_row(ui: &mut egui::Ui, p: &Palette, value: &mut u16, label: &str) -> bool {
    let mut percent = f64::from(*value) / 100.0;
    if slider_row(ui, p, label, &mut percent, 0.0..=100.0, 0.1, "%") {
        *value = (percent * 100.0).round().clamp(0.0, 10_000.0) as u16;
        true
    } else {
        false
    }
}

fn slider_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    suffix: &str,
) -> bool {
    ui.push_id(("overlay-slider", label), |ui| {
        let (row, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), egui::Sense::hover());
        let value_rect = egui::Rect::from_center_size(
            egui::pos2(row.right() - 30.0, row.center().y),
            egui::vec2(60.0, 28.0),
        );
        let slider_rect = egui::Rect::from_min_max(
            egui::pos2(row.left() + 96.0, row.top() + 4.0),
            egui::pos2(value_rect.left() - 10.0, row.bottom() - 4.0),
        );
        let label_rect =
            egui::Rect::from_min_max(row.min, egui::pos2(slider_rect.left() - 10.0, row.bottom()));
        ellipsis_text(ui.painter(), label_rect, label, 12.0, p.text, Align::Min);
        ui.interact(label_rect, ui.id().with("label"), egui::Sense::hover())
            .on_hover_text(label);
        let mut slider_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(slider_rect)
                .id_salt("slider"),
        );
        slider_ui.spacing_mut().slider_width = slider_rect.width();
        // Keep egui's pointer/keyboard interaction and paint a matching compact rail.
        let visuals = slider_ui.visuals_mut();
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            widget.bg_fill = Color32::TRANSPARENT;
            widget.bg_stroke = Stroke::NONE;
            widget.fg_stroke = Stroke::NONE;
        }
        visuals.selection.bg_fill = Color32::TRANSPARENT;
        let slider = slider_ui.put(
            slider_rect,
            egui::Slider::new(value, range.clone())
                .step_by(step)
                .handle_shape(egui::style::HandleShape::Circle)
                .trailing_fill(false)
                .show_value(false),
        );
        slider.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Slider, ui.is_enabled(), label)
        });
        let knob_inset = slider.rect.height() / 2.5;
        let rail = egui::Rect::from_center_size(
            slider.rect.center(),
            egui::vec2(slider.rect.width() - knob_inset * 2.0, 4.0),
        );
        let fraction =
            ((*value - *range.start()) / (*range.end() - *range.start())).clamp(0.0, 1.0) as f32;
        let knob = egui::pos2(
            egui::lerp(rail.left()..=rail.right(), fraction),
            rail.center().y,
        );
        ui.painter()
            .rect_filled(rail, 2.0, mix(p.border, p.subtext, 0.18));
        let active = egui::Rect::from_min_max(rail.min, egui::pos2(knob.x, rail.bottom()));
        ui.painter().rect_filled(active, 2.0, p.accent);
        if slider.hovered() || slider.dragged() || slider.has_focus() {
            ui.painter()
                .circle_filled(knob, 10.0, p.accent.gamma_multiply(0.16));
        }
        ui.painter().circle_filled(knob, 6.0, p.accent);
        ui.painter().circle_filled(knob, 2.3, p.on_selection);
        surface::focus(ui.painter(), slider.rect, &slider, p);
        let mut numeric_ui =
            ui.new_child(egui::UiBuilder::new().max_rect(value_rect).id_salt("value"));
        numeric_ui.spacing_mut().interact_size = value_rect.size();
        numeric_ui
            .style_mut()
            .text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
        let visuals = numeric_ui.visuals_mut();
        visuals.override_text_color = Some(p.text);
        visuals.text_edit_bg_color = Some(p.chip);
        visuals.selection.bg_fill = p.selection;
        for (widget, highlighted) in [
            (&mut visuals.widgets.inactive, false),
            (&mut visuals.widgets.hovered, true),
            (&mut visuals.widgets.active, true),
        ] {
            widget.bg_fill = p.chip;
            widget.weak_bg_fill = p.chip;
            widget.bg_stroke = Stroke::new(0.8_f32, if highlighted { p.accent } else { p.border });
            widget.corner_radius = CornerRadius::same(7);
        }
        let numeric = numeric_ui.put(
            value_rect,
            egui::DragValue::new(value)
                .range(range)
                .speed(step)
                .custom_formatter(|value, _| {
                    if (value - value.round()).abs() < 0.0001 {
                        format!("{value:.0}")
                    } else {
                        format!("{value:.1}")
                    }
                })
                .max_decimals(if step < 1.0 { 1 } else { 0 })
                .suffix(suffix),
        );
        numeric.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::DragValue, ui.is_enabled(), label)
        });
        slider.changed() || numeric.changed()
    })
    .inner
}
