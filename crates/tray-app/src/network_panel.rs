//! 添加应用与联网控制状态的独立面板；表单不会直接修改系统网络权限。

use eframe::egui::{self, Align, Color32, CornerRadius, FontId, Layout, RichText, Stroke, Vec2};

use crate::network_access::NetworkAccess;
use crate::surface::{self, mix};
use crate::theme::{self, Lang, Palette};

#[derive(Default)]
pub struct NetworkPanel {
    open: bool,
    path: String,
    error: Option<&'static str>,
    focus_path: bool,
}

impl NetworkPanel {
    pub fn open(&mut self) {
        self.open = true;
        self.focus_path = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    #[cfg(debug_assertions)]
    pub fn preview_invalid_path(&mut self) {
        self.path = r"C:\Apps\missing-program.exe".into();
        self.error = validated_path(&self.path).err();
    }

    pub fn entry(&mut self, ui: &mut egui::Ui, access: &NetworkAccess, p: &Palette, lang: Lang) {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), egui::Sense::click());
        let tint = if response.is_pointer_button_down_on() {
            0.19
        } else if response.hovered() || self.open {
            0.13
        } else {
            0.05
        };
        ui.painter().rect(
            rect.shrink(1.0),
            9.0,
            mix(p.surface, p.accent, tint),
            Stroke::new(0.7_f32, mix(p.border, p.accent, 0.16)),
            egui::StrokeKind::Inside,
        );
        paint_add_app(ui.painter(), rect.center(), p.accent);
        if access.error.is_some() {
            ui.painter()
                .circle_filled(rect.right_top() + Vec2::new(-3.0, 3.0), 2.5, p.warn);
        }
        surface::focus(ui.painter(), rect, &response, p);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                lang.t("network_panel.add_application"),
            )
        });
        if response.clicked() {
            self.open();
        }
        response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(lang.t("network_panel.add_application_network_access"));
    }

    /// 返回经校验的完整路径，由主界面添加到列表并保存。
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        access: &mut NetworkAccess,
        p: &Palette,
        lang: Lang,
    ) -> Option<String> {
        if !self.open {
            return None;
        }
        // 遮罩保留主窗口圆角，避免透明窗口的四角变成方块。
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("network-panel-shade"),
        ))
        .rect_filled(
            ctx.content_rect(),
            20.0,
            Color32::from_black_alpha(if p.dark { 100 } else { 35 }),
        );
        let frame = egui::Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(18)
            .inner_margin(20)
            .shadow(egui::epaint::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(45),
            });
        let mut added = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("add-application"))
            .backdrop_color(Color32::TRANSPARENT)
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(306.0);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                // 给表单滚动条预留空隙，避免覆盖输入框和刷新按钮。
                let scroll = &mut ui.spacing_mut().scroll;
                scroll.floating = false;
                scroll.bar_width = 4.0;
                scroll.bar_inner_margin = 8.0;
                scroll.bar_outer_margin = 0.0;
                ui.visuals_mut().override_text_color = Some(p.text);
                ui.visuals_mut().selection.bg_fill = p.selection;
                ui.visuals_mut().selection.stroke = Stroke::new(1.0_f32, p.on_selection);
                let widgets = &mut ui.visuals_mut().widgets;
                for visuals in [
                    &mut widgets.inactive,
                    &mut widgets.hovered,
                    &mut widgets.active,
                ] {
                    visuals.corner_radius = CornerRadius::same(9);
                    visuals.bg_stroke = Stroke::NONE;
                    visuals.expansion = 0.0;
                }
                ui.horizontal(|ui| {
                    let (mark, _) = ui.allocate_exact_size(Vec2::splat(36.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(mark, 11.0, mix(p.surface, p.accent, 0.10));
                    paint_add_app(ui.painter(), mark.center(), p.accent);
                    ui.add_space(3.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(lang.t("network_panel.add_application"))
                                .font(theme::display_font(17.0))
                                .color(p.text),
                        );
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new(lang.t("network_panel.keep_an_app_within_reach"))
                                .size(11.0)
                                .color(p.subtext),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = small_icon_button(
                            ui,
                            "close",
                            lang.t("common.close"),
                            true,
                            p,
                            paint_close,
                        )
                        .clicked();
                    });
                });
                ui.add_space(22.0);
                egui::ScrollArea::vertical()
                    .id_salt("network-panel-body")
                    .max_height((ctx.content_rect().height() - 136.0).max(180.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(lang.t("network_panel.application_path"))
                                .size(12.0)
                                .strong()
                                .color(p.text),
                        );
                        ui.add_space(8.0);
                        let input = ui.add_sized(
                            [ui.available_width(), 40.0],
                            egui::TextEdit::singleline(&mut self.path)
                                .id_salt("application-path")
                                .font(FontId::proportional(12.0))
                                .text_color(p.text)
                                .background_color(p.panel_bottom)
                                .margin(egui::Margin::symmetric(12, 11))
                                .hint_text(
                                    RichText::new(lang.t("network_panel.paste_the_full_exe_path"))
                                        .color(p.subtext),
                                ),
                        );
                        if self.focus_path {
                            input.request_focus();
                            self.focus_path = false;
                        }
                        if input.changed() {
                            self.error = None;
                        }
                        ui.painter().rect_stroke(
                            input.rect,
                            9.0,
                            Stroke::new(
                                1.0_f32,
                                if self.error.is_some() {
                                    p.warn
                                } else if input.has_focus() {
                                    p.accent
                                } else {
                                    p.border
                                },
                            ),
                            egui::StrokeKind::Inside,
                        );
                        let enter =
                            input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(7.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    lang.t("network_panel.for_example_c_apps_app_exe_quoted_paths"),
                                )
                                .size(10.5)
                                .color(p.subtext),
                            )
                            .wrap(),
                        );
                        if let Some(error) = self.error.map(|key| lang.t(key)) {
                            ui.add_space(8.0);
                            ui.add(
                                egui::Label::new(RichText::new(error).size(11.0).color(p.warn))
                                    .wrap(),
                            );
                        }
                        ui.add_space(16.0);
                        let submit = primary_button(
                            ui,
                            lang.t("network_panel.add_to_list"),
                            !self.path.trim().is_empty(),
                            p,
                        );
                        if submit.clicked() || enter {
                            match validated_path(&self.path) {
                                Ok(path) => {
                                    added = Some(path);
                                    self.path.clear();
                                    self.error = None;
                                }
                                Err(error) => {
                                    self.error = Some(error);
                                    self.focus_path = true;
                                }
                            }
                        }
                        ui.add_space(9.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    lang.t("network_panel.the_app_will_be_pinned_use_its_network"),
                                )
                                .size(11.0)
                                .color(p.subtext),
                            )
                            .wrap(),
                        );
                        ui.add_space(19.0);
                        let (line, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 1.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().line_segment(
                            [line.left_center(), line.right_center()],
                            Stroke::new(0.7_f32, p.border),
                        );
                        ui.add_space(14.0);
                        control_status(ui, access, p, lang);
                        ui.add_space(10.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(lang.t(
                                    "network_panel.blocks_remain_until_you_restore_network_access",
                                ))
                                .size(10.5)
                                .color(p.subtext),
                            )
                            .wrap(),
                        );
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    lang.t(
                                        "network_panel.blocking_a_proxy_affects_apps_using_it_set",
                                    ),
                                )
                                .size(10.5)
                                .color(p.subtext),
                            )
                            .wrap(),
                        );
                    });
            });
        if close || added.is_some() || modal.should_close() {
            self.close();
        }
        added
    }
}

fn validated_path(input: &str) -> Result<String, &'static str> {
    let path = input.trim().trim_matches('"');
    if tm_windows::network_policy::validate_path(path).is_err() {
        return Err("network_panel.enter_a_full_path_to_a_local_exe");
    }
    if !tm_windows::network_control::is_program(std::path::Path::new(path)) {
        return Err("network_panel.this_file_was_not_found_check_the_path");
    }
    Ok(path.to_owned())
}

fn control_status(ui: &mut egui::Ui, access: &mut NetworkAccess, p: &Palette, lang: Lang) {
    let (status, color) = if access.error.is_some() {
        (lang.t("common.unavailable"), p.warn)
    } else if access.is_demo() {
        (lang.t("network_panel.demo_mode"), p.subtext)
    } else if access.busy() || !access.verified {
        (lang.t("network_panel.checking"), p.accent)
    } else {
        (lang.t("network_panel.ready"), p.download)
    };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(lang.t("network_panel.network_access"))
                .size(12.0)
                .strong()
                .color(p.text),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if small_icon_button(
                ui,
                "refresh",
                lang.t("network_panel.refresh_network_status"),
                !access.busy() && !access.is_demo(),
                p,
                paint_refresh,
            )
            .clicked()
            {
                access.refresh(ui.ctx());
            }
            ui.label(RichText::new(status).size(11.0).color(color));
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(6.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 2.5, color);
        });
    });
    if let Some(error) = &access.error {
        ui.add_space(5.0);
        ui.add(
            egui::Label::new(
                RichText::new(lang.t("network_panel.you_can_add_apps_now_and_manage_access"))
                    .size(11.0)
                    .color(p.subtext),
            )
            .wrap(),
        );
        ui.collapsing(
            RichText::new(lang.t("network_panel.view_details"))
                .size(10.5)
                .color(p.warn),
            |ui| {
                ui.add(egui::Label::new(RichText::new(error).size(10.5).color(p.subtext)).wrap());
            },
        );
    } else if access.is_demo() {
        ui.add(
            egui::Label::new(
                RichText::new(
                    lang.t("network_panel.demo_mode_does_not_change_network_permissions"),
                )
                .size(10.5)
                .color(p.subtext),
            )
            .wrap(),
        );
    }
}

fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool, p: &Palette) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), egui::Sense::click());
        let fill = if !enabled {
            mix(p.surface, p.subtext, 0.10)
        } else if response.is_pointer_button_down_on() {
            p.selection
        } else if response.hovered() {
            mix(p.selection, p.selection_top, 0.45)
        } else {
            p.selection
        };
        ui.painter().rect_filled(rect, 10.0, fill);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            FontId::proportional(13.0),
            if enabled { p.on_selection } else { p.subtext },
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
        surface::focus(ui.painter(), rect, &response, p);
        if enabled {
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            response
        }
    })
    .inner
}

fn small_icon_button(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    enabled: bool,
    p: &Palette,
    paint: fn(&egui::Painter, egui::Pos2, Color32),
) -> egui::Response {
    ui.push_id(id, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(26.0), egui::Sense::click());
            if response.hovered() {
                ui.painter()
                    .rect_filled(rect, 8.0, mix(p.surface, p.accent, 0.10));
            }
            paint(
                ui.painter(),
                rect.center(),
                if response.hovered() {
                    p.accent
                } else {
                    p.subtext
                },
            );
            surface::focus(ui.painter(), rect, &response, p);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label)
            });
            response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(label)
        })
        .inner
    })
    .inner
}

fn paint_add_app(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    for offset in [
        Vec2::new(-3.8, -3.8),
        Vec2::new(3.8, -3.8),
        Vec2::new(-3.8, 3.8),
    ] {
        painter.rect_stroke(
            egui::Rect::from_center_size(center + offset, Vec2::splat(4.7)),
            1.2,
            stroke,
            egui::StrokeKind::Middle,
        );
    }
    let plus = center + Vec2::splat(3.8);
    painter.line_segment(
        [plus - Vec2::new(2.5, 0.0), plus + Vec2::new(2.5, 0.0)],
        stroke,
    );
    painter.line_segment(
        [plus - Vec2::new(0.0, 2.5), plus + Vec2::new(0.0, 2.5)],
        stroke,
    );
}

fn paint_close(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    for end in [Vec2::new(3.5, 3.5), Vec2::new(3.5, -3.5)] {
        painter.line_segment([center - end, center + end], Stroke::new(1.3_f32, color));
    }
}

fn paint_refresh(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let points = (0..=24)
        .map(|i| center + Vec2::angled((45.0 + 290.0 * i as f32 / 24.0).to_radians()) * 5.0)
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.2_f32, color)));
    painter.add(egui::Shape::line(
        vec![
            center + Vec2::new(1.3, -2.0),
            center + Vec2::new(4.6, -2.0),
            center + Vec2::new(4.6, -5.3),
        ],
        Stroke::new(1.2_f32, color),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanelUi {
        ctx: egui::Context,
        panel: NetworkPanel,
        access: NetworkAccess,
        frame: usize,
        entry_at: egui::Pos2,
        background_at: egui::Pos2,
        background_clicks: usize,
        dark: bool,
    }

    impl PanelUi {
        fn new(dark: bool) -> Self {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
            Self {
                ctx,
                panel: NetworkPanel::default(),
                access: NetworkAccess::new(true),
                frame: 0,
                entry_at: egui::Pos2::ZERO,
                background_at: egui::Pos2::ZERO,
                background_clicks: 0,
                dark,
            }
        }

        fn run(&mut self, events: Vec<egui::Event>) -> Option<String> {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(408.0, 482.0),
                )),
                time: Some(self.frame as f64 / 60.0),
                events,
                ..Default::default()
            };
            self.frame += 1;
            let p = theme::palette(self.dark);
            let mut added = None;
            let _ = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.entry_at = ui.cursor().min + Vec2::splat(14.0);
                    self.panel.entry(ui, &self.access, &p, Lang::EN);
                    let background = ui.button("Background action");
                    self.background_at = background.rect.center();
                    self.background_clicks += usize::from(background.clicked());
                });
                if let Some(path) = self.panel.show(ctx, &mut self.access, &p, Lang::EN) {
                    added = Some(path);
                }
            });
            added
        }

        fn click(&mut self, at: egui::Pos2) {
            for pressed in [true, false] {
                self.run(vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
        }

        fn open(&mut self) {
            self.run(vec![]);
            self.click(self.entry_at);
            // Area 的尺寸和焦点在后续帧稳定。
            for _ in 0..3 {
                self.run(vec![]);
            }
            assert!(self.panel.open);
        }

        fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) -> Option<String> {
            let event = |pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            };
            let added = self.run(vec![event(true)]);
            self.run(vec![event(false)]);
            added
        }
    }

    #[test]
    fn form_accepts_typing_and_enter_then_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("My App.exe");
        std::fs::write(&path, []).unwrap();
        let expected = path.to_str().unwrap();
        for dark in [false, true] {
            let mut ui = PanelUi::new(dark);
            ui.open();
            ui.run(vec![egui::Event::Paste(format!("  \"{expected}\"  "))]);
            assert!(ui.panel.path.contains(expected), "打开后输入框应获得焦点");
            assert_eq!(
                ui.key(egui::Key::Enter, egui::Modifiers::NONE).as_deref(),
                Some(expected)
            );
            assert!(!ui.panel.open);
            assert!(ui.panel.path.is_empty());
            assert!(ui.panel.error.is_none());
            assert_eq!(ui.background_clicks, 0);
            assert!(ui.access.rules.is_empty(), "添加表单不能直接修改网络规则");
        }
    }

    #[test]
    fn invalid_input_keeps_form_open_and_can_be_corrected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.exe");
        let text = path.to_str().unwrap();
        let mut ui = PanelUi::new(false);
        ui.open();
        ui.run(vec![egui::Event::Text(text.to_owned())]);
        assert!(ui.key(egui::Key::Enter, egui::Modifiers::NONE).is_none());
        assert!(ui.panel.open);
        assert!(ui.panel.error.is_some());
        assert_eq!(ui.panel.path, text);
        std::fs::write(&path, []).unwrap();
        ui.key(egui::Key::A, egui::Modifiers::COMMAND);
        ui.run(vec![egui::Event::Paste(text.to_owned())]);
        assert!(ui.panel.error.is_none(), "修改输入后应清除旧错误");
        assert_eq!(
            ui.key(egui::Key::Enter, egui::Modifiers::NONE).as_deref(),
            Some(text)
        );
        assert!(!ui.panel.open);
    }

    #[test]
    fn escape_and_backdrop_close_without_activating_background() {
        let mut ui = PanelUi::new(true);
        ui.open();
        ui.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!ui.panel.open);
        ui.open();
        ui.click(ui.background_at);
        assert!(!ui.panel.open);
        assert_eq!(ui.background_clicks, 0);
        ui.run(vec![]);
        ui.click(ui.background_at);
        assert_eq!(ui.background_clicks, 1, "关闭后背景应恢复交互");
    }
}
