use super::*;

pub(super) fn show(
    response: &egui::Response,
    header: egui::Rect,
    app: &AppKey,
    details: Option<&Details>,
    restriction: Option<&str>,
    p: &Palette,
    lang: Lang,
) {
    let popup_id = response.id.with(app.as_str());
    #[cfg(debug_assertions)]
    if crate::ui_capture::enabled()
        && std::env::var("TM_CAPTURE_STATE").as_deref() == Ok("connections-help")
    {
        egui::Popup::open_id(&response.ctx, popup_id);
    }
    let width = (header.width() - 26.0).min(328.0);
    egui::Popup::from_toggle_button_response(response)
        .id(popup_id)
        .at_position(header.right_bottom())
        .align(egui::RectAlign::BOTTOM_END)
        .gap(8.0)
        .width(width + 26.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .frame(
            egui::Frame::NONE
                .fill(p.surface)
                .stroke(Stroke::new(1.0_f32, p.border))
                .corner_radius(12)
                .inner_margin(12)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 5],
                    blur: 16,
                    spread: 0,
                    color: Color32::from_black_alpha(if p.dark { 64 } else { 24 }),
                }),
        )
        .show(|ui| {
            ui.set_width(width);
            ui.style_mut().override_font_id = Some(egui::FontId::proportional(11.0));
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            ui.spacing_mut().scroll.bar_width = 4.0;
            ui.spacing_mut().scroll.foreground_color = false;
            ui.visuals_mut().override_text_color = Some(p.text);
            ui.visuals_mut().widgets.inactive.bg_fill = p.border;
            ui.visuals_mut().widgets.hovered.bg_fill = p.accent;
            ui.visuals_mut().widgets.active.bg_fill = p.accent;
            ui.visuals_mut().selection.bg_fill = mix(p.surface, p.accent, 0.22);
            ui.visuals_mut().selection.stroke = Stroke::new(1.0_f32, p.text);
            ui.visuals_mut().widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
            let (heading, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::hover());
            ui.painter().text(
                heading.left_center(),
                egui::Align2::LEFT_CENTER,
                lang.t("connections.help"),
                theme::display_font(14.0),
                p.text,
            );
            let close = egui::Rect::from_min_size(
                heading.right_top() - egui::vec2(26.0, 0.0),
                Vec2::splat(26.0),
            );
            let close = icon_button(ui, close, "connections-help-close", p, paint_close);
            close.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, lang.t("common.close"))
            });
            if close.clicked() {
                ui.close();
            }
            egui::ScrollArea::vertical()
                .id_salt("connections-help-body")
                .max_height(300.0)
                // A popup starts from an estimated height. Let normal content
                // grow beyond that estimate; auto-shrink still fits short text.
                .min_scrolled_height(300.0)
                .show(ui, |ui| body(ui, app, details, restriction, p, lang));
        });
}

fn body(
    ui: &mut egui::Ui,
    app: &AppKey,
    details: Option<&Details>,
    restriction: Option<&str>,
    p: &Palette,
    lang: Lang,
) {
    // Do not present counts from a failed or stale snapshot as current facts.
    let current =
        details.filter(|d| d.updated_at.elapsed() <= Duration::from_secs(6) && d.result.is_ok());
    if let Some(details) = current {
        ui.columns(2, |columns| {
            for (column, (label, count)) in columns.iter_mut().zip([
                ("connections.state.established", details.established),
                ("connections.summary_listening", details.listening),
            ]) {
                let width = column.available_width();
                egui::Frame::NONE
                    .fill(p.chip)
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(column, |ui| {
                        ui.set_width((width - 20.0).max(1.0));
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(lang.t(label)).size(10.0).color(p.subtext));
                        ui.label(RichText::new(count.to_string()).size(19.0).color(p.accent));
                    });
            }
        });
    }
    for key in ["connections.note", "connections.proxy_note"] {
        ui.add(egui::Label::new(RichText::new(lang.t(key)).color(p.subtext)).wrap());
    }
    #[cfg(windows)]
    ui.add(egui::Label::new(RichText::new(lang.t("connections.udp_note")).color(p.subtext)).wrap());
    if let Some(details) = current {
        if let Ok(report) = &details.result {
            if report.unverified_processes > 0 {
                ui.add(
                    egui::Label::new(
                        RichText::new(lang.format(
                            "connections.unverified_processes",
                            &[("count", &report.unverified_processes.to_string())],
                        ))
                        .color(p.warn),
                    )
                    .wrap(),
                );
            }
            if let Some(warning) = &report.warning {
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("connections.collection_details")).color(p.warn),
                )
                .id_salt("collection-details")
                .show(ui, |ui| {
                    ui.add(egui::Label::new(warning).wrap().selectable(true));
                });
            }
        }
    }
    if let Some(restriction) = restriction {
        ui.add(egui::Label::new(RichText::new(restriction).color(p.warn)).wrap());
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(lang.t("connections.app_path"))
                .size(10.0)
                .color(p.subtext),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let copied_id = ui.id().with("path-copied");
            let copied = ui.ctx().data(|data| {
                data.get_temp::<Instant>(copied_id)
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
            });
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(lang.t(if copied {
                            "connections.copied"
                        } else {
                            "connections.copy_path"
                        }))
                        .size(10.0)
                        .color(p.accent),
                    )
                    .fill(p.chip)
                    .stroke(Stroke::new(0.7_f32, p.border))
                    .corner_radius(6)
                    .min_size(Vec2::new(64.0, 24.0)),
                )
                .clicked()
            {
                ui.ctx().copy_text(app.as_str().to_owned());
                ui.ctx()
                    .data_mut(|data| data.insert_temp(copied_id, Instant::now()));
                ui.ctx().request_repaint_after(Duration::from_secs(2));
            }
        });
    });
    ui.add(
        egui::Label::new(
            RichText::new(app.as_str())
                .monospace()
                .size(10.5)
                .color(p.text),
        )
        .wrap()
        .selectable(true),
    );
}

fn paint_close(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    for sign in [-1.0, 1.0] {
        painter.line_segment(
            [
                rect.center() + egui::vec2(-3.5, -3.5 * sign),
                rect.center() + egui::vec2(3.5, 3.5 * sign),
            ],
            Stroke::new(1.2_f32, color),
        );
    }
}
