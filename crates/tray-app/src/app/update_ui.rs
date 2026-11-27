use super::{theme, App, Lang, Palette};
use crate::settings::UpdateCheckResult;
use crate::update::{InstallAction, Phase};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2};

#[cfg(test)]
mod interaction_tests;

pub(super) enum UpdatePrompt {
    Restart {
        version: String,
        defer_error: Option<String>,
    },
    Download {
        error: String,
        browser_error: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Restart,
    Later,
    Manual,
    Resume,
}

impl UpdatePrompt {
    pub(super) fn restart(version: String) -> Self {
        Self::Restart {
            version,
            defer_error: None,
        }
    }

    pub(super) fn download_failed(error: String) -> Self {
        Self::Download {
            error,
            browser_error: None,
        }
    }

    fn show(
        &self,
        ctx: &egui::Context,
        palette: &Palette,
        lang: Lang,
        can_install: bool,
    ) -> Option<Choice> {
        let ready = matches!(self, Self::Restart { .. });
        let modal_id = egui::Id::new(("update-prompt", ready));
        let viewport = ctx.content_rect();
        let edge_gap = 16.0;
        let width = (viewport.width() - 2.0 * edge_gap).clamp(1.0, 346.0);
        let inset = if width < 280.0 { 14 } else { 20 };
        let margin = egui::Margin::symmetric(inset, 18);
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            modal_id.with("shade"),
        ))
        .rect_filled(
            viewport,
            super::PANEL_RADIUS,
            Color32::from_rgba_unmultiplied(14, 22, 39, if palette.dark { 166 } else { 108 }),
        );
        egui::Modal::new(modal_id)
            .frame(
                egui::Frame::NONE
                    .fill(palette.surface)
                    .stroke(Stroke::new(1.0_f32, palette.border))
                    .corner_radius(14)
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 8],
                        blur: 24,
                        spread: 0,
                        color: Color32::from_black_alpha(65),
                    }),
            )
            .backdrop_color(Color32::TRANSPARENT)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = egui::vec2(7.0, 0.0);
                let header = self.status_strip(ui, palette, lang, inset);
                let mut choice = header.inner;
                egui::Frame::NONE.inner_margin(margin).show(ui, |ui| {
                    let buttons = if ready {
                        [
                            (Choice::Restart, "update.restart_now", ButtonRole::Primary),
                            (Choice::Later, "update.next_launch", ButtonRole::Secondary),
                        ]
                    } else {
                        [
                            (Choice::Manual, "update.manual", ButtonRole::External),
                            (Choice::Resume, "update.resume", ButtonRole::Secondary),
                        ]
                    };
                    let row_width = buttons
                        .iter()
                        .map(|(_, key, role)| role.width(ui, lang.t(key)))
                        .sum::<f32>()
                        + ui.spacing().item_spacing.x;
                    let stack = row_width > ui.available_width();
                    let footer_height = if stack {
                        2.0 * ACTION_HEIGHT + 4.0
                    } else {
                        ACTION_HEIGHT
                    };
                    let body_limit = viewport.height()
                        - header.response.rect.height()
                        - margin.sum().y
                        - ACTION_GAP
                        - footer_height
                        - 2.0 * edge_gap;
                    egui::ScrollArea::vertical()
                        .id_salt("update-message")
                        .max_height(body_limit.max(28.0))
                        .auto_shrink([false, true])
                        .show(ui, |ui| self.message(ui, palette, lang, can_install));
                    ui.add_space(ACTION_GAP);
                    let mut actions = |ui: &mut egui::Ui| {
                        let min_width = if stack { ui.available_width() } else { 0.0 };
                        for (index, (action, key, role)) in buttons.into_iter().enumerate() {
                            if index != 0 && stack {
                                ui.add_space(4.0);
                            }
                            if dialog_button(
                                ui,
                                palette,
                                lang.t(key),
                                min_width,
                                role,
                                action != Choice::Restart || can_install,
                            ) {
                                choice = Some(action);
                            }
                        }
                    };
                    if stack {
                        actions(ui);
                    } else {
                        ui.horizontal(actions);
                    }
                });
                choice
            })
            .inner
    }

    fn status_strip(
        &self,
        ui: &mut egui::Ui,
        palette: &Palette,
        lang: Lang,
        inset: i8,
    ) -> egui::InnerResponse<Option<Choice>> {
        let ready = matches!(self, Self::Restart { .. });
        let signal = if ready {
            palette.download
        } else {
            palette.warn
        };
        let ink = if palette.dark {
            signal
        } else {
            super::mix(signal, palette.text, 0.32)
        };
        egui::Frame::NONE
            .fill(super::mix(
                palette.surface,
                signal,
                if palette.dark { 0.09 } else { 0.07 },
            ))
            .corner_radius(egui::CornerRadius {
                nw: 13,
                ne: 13,
                sw: 0,
                se: 0,
            })
            .inner_margin(egui::Margin::symmetric(inset, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    let status = lang.t(if ready {
                        "update.verified"
                    } else {
                        "update.retry_status"
                    });
                    let trailing_width = if ready {
                        (ui.available_width() * 0.35).min(104.0)
                    } else {
                        ButtonRole::Dismiss.width(ui, lang.t("update.later"))
                    };
                    let gap = ui.spacing().item_spacing.x;
                    let status_width = (ui.available_width() - trailing_width - gap).max(1.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(status_width, 26.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let font = egui::FontId::proportional(12.0);
                            if MARK_WIDTH + gap + text_width(ui, status, font.clone())
                                <= status_width
                            {
                                transfer_mark(ui, ink, palette.surface, ready);
                            }
                            ui.add(
                                egui::Label::new(RichText::new(status).font(font).color(ink))
                                    .wrap(),
                            );
                        },
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| match self {
                            Self::Restart { version, .. } => {
                                let version = format!("v{version}");
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&version)
                                            .monospace()
                                            .size(11.5)
                                            .color(palette.subtext),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&version);
                                None
                            }
                            Self::Download { .. } => dialog_button(
                                ui,
                                palette,
                                lang.t("update.later"),
                                0.0,
                                ButtonRole::Dismiss,
                                true,
                            )
                            .then_some(Choice::Later),
                        },
                    )
                    .inner
                })
                .inner
            })
    }

    fn message(&self, ui: &mut egui::Ui, palette: &Palette, lang: Lang, can_install: bool) {
        let (title, body) = match self {
            Self::Restart { .. } => ("update.prompt_title", "update.prompt_body"),
            Self::Download { .. } => ("update.fallback_title", "update.fallback_body"),
        };
        ui.add(
            egui::Label::new(
                RichText::new(lang.t(title))
                    .font(theme::display_font(23.0))
                    .color(palette.text),
            )
            .wrap(),
        );
        ui.add_space(7.0);
        ui.add(
            egui::Label::new(
                RichText::new(lang.t(body))
                    .size(13.0)
                    .line_height(Some(19.0))
                    .color(palette.subtext),
            )
            .wrap(),
        );
        let notice = match self {
            Self::Restart {
                defer_error: Some(error),
                ..
            } => Some(lang.format("update.defer_error", &[("error", error)])),
            Self::Restart { .. } if !can_install => {
                Some(lang.t("sensors.finish_install_first").to_owned())
            }
            Self::Download { browser_error, .. } => browser_error.clone(),
            _ => None,
        };
        if let Some(notice) = notice {
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(RichText::new(notice).size(12.0).color(palette.warn))
                    .wrap()
                    .selectable(true),
            );
        }
        if let Self::Download { error, .. } = self {
            ui.add_space(6.0);
            egui::CollapsingHeader::new(
                RichText::new(lang.t("update.details"))
                    .size(11.5)
                    .color(palette.subtext),
            )
            .id_salt("download-failure-details")
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(error)
                            .monospace()
                            .size(11.5)
                            .color(palette.text),
                    )
                    .wrap()
                    .selectable(true),
                );
            });
        }
    }
}

fn transfer_mark(ui: &mut egui::Ui, ink: Color32, cutout: Color32, ready: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(MARK_WIDTH, 18.0), egui::Sense::hover());
    let painter = ui.painter();
    let start = rect.left_center() + egui::vec2(2.0, 0.0);
    let end = rect.right_center() - egui::vec2(8.0, 0.0);
    let stroke = Stroke::new(1.5_f32, ink);
    painter.circle_filled(start, 2.0, ink);
    painter.line_segment(
        [
            start,
            if ready {
                end
            } else {
                start + egui::vec2(7.0, 0.0)
            },
        ],
        stroke,
    );
    painter.circle_filled(end, 7.5, ink);
    let stroke = Stroke::new(1.6_f32, cutout);
    if ready {
        painter.line(
            vec![
                end + egui::vec2(-3.5, 0.0),
                end + egui::vec2(-1.0, 2.5),
                end + egui::vec2(3.7, -2.6),
            ],
            stroke,
        );
    } else {
        for offset in [-2.0, 2.0] {
            painter.line_segment(
                [
                    end + egui::vec2(offset, -3.0),
                    end + egui::vec2(offset, 3.0),
                ],
                stroke,
            );
        }
    }
}

const ACTION_HEIGHT: f32 = 37.0;
const ACTION_GAP: f32 = 16.0;
const MARK_WIDTH: f32 = 32.0;
const EXTERNAL_ARROW: f32 = 10.0;

fn text_width(ui: &egui::Ui, label: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(label.to_owned(), font, Color32::WHITE)
        .size()
        .x
        .ceil()
}

#[derive(Clone, Copy)]
enum ButtonRole {
    Primary,
    External,
    Secondary,
    Dismiss,
}

impl ButtonRole {
    fn primary(self) -> bool {
        matches!(self, Self::Primary | Self::External)
    }

    fn font(self) -> egui::FontId {
        match self {
            Self::Primary | Self::External => theme::display_font(12.5),
            Self::Secondary => egui::FontId::proportional(12.5),
            Self::Dismiss => egui::FontId::proportional(11.5),
        }
    }

    fn padding(self) -> f32 {
        if self.primary() {
            14.0
        } else {
            8.0
        }
    }

    fn width(self, ui: &egui::Ui, label: &str) -> f32 {
        let icon = if matches!(self, Self::External) {
            EXTERNAL_ARROW + ui.spacing().icon_spacing
        } else {
            0.0
        };
        text_width(ui, label, self.font()) + 2.0 * self.padding() + icon
    }
}

fn dialog_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    width: f32,
    role: ButtonRole,
    enabled: bool,
) -> bool {
    let primary = role.primary();
    let dismiss = matches!(role, ButtonRole::Dismiss);
    ui.scope(|ui| {
        if !enabled {
            ui.disable();
        }
        let (fill, ink) = if primary && palette.dark {
            (palette.selection_top, palette.panel_bottom)
        } else if primary {
            (palette.selection, palette.on_selection)
        } else {
            (Color32::TRANSPARENT, palette.subtext)
        };
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.inactive.weak_bg_fill = fill;
        widgets.hovered.weak_bg_fill = if primary {
            super::mix(fill, ink, 0.08)
        } else {
            super::mix(palette.surface, palette.accent, 0.09)
        };
        widgets.active.weak_bg_fill = if primary {
            super::mix(fill, ink, 0.14)
        } else {
            super::mix(palette.surface, palette.accent, 0.15)
        };
        ui.spacing_mut().button_padding.x = role.padding();
        let mut atoms = egui::Atoms::new(RichText::new(label).font(role.font()).color(ink));
        let arrow = egui::Id::new("release-page-arrow");
        if matches!(role, ButtonRole::External) {
            atoms.push_right(egui::Atom::custom(arrow, Vec2::splat(EXTERNAL_ARROW)));
        }
        let button = egui::Button::new(atoms)
            .min_size(egui::vec2(
                width,
                if dismiss { 26.0 } else { ACTION_HEIGHT },
            ))
            .stroke(Stroke::NONE)
            .corner_radius(8)
            .atom_ui(ui);
        if let Some(rect) = button.rect(arrow) {
            let rect = rect.shrink(1.5);
            let stroke = Stroke::new(1.4_f32, ink);
            ui.painter()
                .line_segment([rect.left_bottom(), rect.right_top()], stroke);
            ui.painter().line(
                vec![rect.left_top(), rect.right_top(), rect.right_bottom()],
                stroke,
            );
        }
        let response = button
            .response
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.has_focus() {
            ui.painter().rect_stroke(
                response.rect.expand(2.0),
                10,
                Stroke::new(1.5_f32, palette.accent),
                egui::StrokeKind::Outside,
            );
        }
        response.clicked()
    })
    .inner
}

impl App {
    pub(super) fn record_update_check_result(&mut self) {
        if let Some(result) = self.update.take_check_result() {
            self.settings.last_check_result = match result {
                Ok(Some(version)) => UpdateCheckResult::Available(version),
                Ok(None) => UpdateCheckResult::Latest,
                Err(error) => UpdateCheckResult::Failed(error),
            };
            self.settings.save();
        }
    }

    pub(super) fn update_check_history(&self, ui: &mut egui::Ui, palette: &Palette, lang: Lang) {
        let time = self.settings.last_check_local_time();
        let message = if self.settings.last_check == 0 {
            lang.t("update.never_checked").to_owned()
        } else {
            let time = time
                .as_deref()
                .unwrap_or_else(|| lang.t("update.time_unknown"));
            let result = history_result(
                lang,
                &self.settings.last_check_result,
                self.update.phase == Phase::Checking,
            );
            lang.format("update.history", &[("time", time), ("result", &result)])
        };
        super::settings_note(ui, palette, &message);
    }

    pub(super) fn has_update_prompt(&self) -> bool {
        self.update_prompt.is_some()
    }

    fn can_install_update(&self) -> bool {
        #[cfg(windows)]
        {
            !self.sensor_components.is_installing()
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    pub(super) fn service_update_install_action(&mut self, ctx: &egui::Context) {
        if self.has_update_prompt() {
            return;
        }
        if let Some(error) = self.update.take_download_error() {
            self.update_prompt = Some(UpdatePrompt::download_failed(error));
            self.reveal_panel_for_prompt(ctx);
            return;
        }
        if !self.can_install_update() {
            return;
        }
        match self.update.take_install_action() {
            Some(InstallAction::Prompt { version }) => {
                self.update_prompt = Some(UpdatePrompt::restart(version));
                self.reveal_panel_for_prompt(ctx);
            }
            Some(InstallAction::InstallOnLaunch) => self.start_verified_install(ctx),
            None => {}
        }
    }

    fn start_verified_install(&mut self, ctx: &egui::Context) {
        if self.can_install_update() && self.update.install() {
            crate::update::log_line("升级程序已启动，正在保存历史并退出");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    pub(super) fn show_update_prompt(
        &mut self,
        ctx: &egui::Context,
        palette: &Palette,
        lang: Lang,
    ) {
        let Some(prompt) = &self.update_prompt else {
            return;
        };
        if matches!(prompt, UpdatePrompt::Restart { version: expected, .. } if !matches!(&self.update.phase, Phase::Ready { version, .. } if version == expected))
        {
            self.update_prompt = None;
            return;
        }
        match prompt.show(ctx, palette, lang, self.can_install_update()) {
            Some(Choice::Restart) => {
                self.update_prompt = None;
                self.start_verified_install(ctx);
            }
            Some(Choice::Later) => {
                if let Some(UpdatePrompt::Restart { defer_error, .. }) = &mut self.update_prompt {
                    if let Err(error) = self.update.defer_install() {
                        *defer_error = Some(error);
                        return;
                    }
                }
                self.update_prompt = None;
            }
            Some(Choice::Manual) => match crate::update::open_url(self.update.release_page()) {
                Ok(()) => self.update_prompt = None,
                Err(error) => {
                    if let Some(UpdatePrompt::Download { browser_error, .. }) =
                        &mut self.update_prompt
                    {
                        *browser_error = Some(error);
                    }
                }
            },
            Some(Choice::Resume) => {
                self.update_prompt = None;
                self.update.resume_download();
            }
            None => {}
        }
    }
}

fn history_result(lang: Lang, result: &UpdateCheckResult, checking: bool) -> String {
    match result {
        UpdateCheckResult::Unknown => lang.t("update.history_unknown").to_owned(),
        UpdateCheckResult::Pending if checking => lang.t("ui.checking").to_owned(),
        UpdateCheckResult::Pending => lang.t("update.history_interrupted").to_owned(),
        UpdateCheckResult::Latest => lang.t("update.history_latest").to_owned(),
        UpdateCheckResult::Available(version) => {
            lang.format("update.history_available", &[("version", version)])
        }
        UpdateCheckResult::Failed(error) => lang.format("update.check_error", &[("error", error)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_history_distinguishes_unrecorded_and_interrupted_results() {
        for lang in [Lang::EN, Lang::ZH_CN] {
            for (result, active, key) in [
                (UpdateCheckResult::Unknown, false, "update.history_unknown"),
                (
                    UpdateCheckResult::Pending,
                    false,
                    "update.history_interrupted",
                ),
                (UpdateCheckResult::Pending, true, "ui.checking"),
                (UpdateCheckResult::Latest, false, "update.history_latest"),
            ] {
                assert_eq!(history_result(lang, &result, active), lang.t(key));
            }
            assert!(
                history_result(lang, &UpdateCheckResult::Available("1.2.0".into()), false)
                    .contains("1.2.0")
            );
            assert!(
                history_result(lang, &UpdateCheckResult::Failed("offline".into()), false)
                    .contains("offline")
            );
        }
    }

    pub(super) fn label_position(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(text.pos + text.galley.size() / 2.0)
            }
            egui::Shape::Vec(shapes) => {
                shapes.iter().find_map(|shape| label_position(shape, label))
            }
            _ => None,
        }
    }

    #[test]
    fn only_explicit_enabled_update_choices_activate() {
        let restart = UpdatePrompt::restart("2.1.0".to_owned());
        let failure = UpdatePrompt::Download {
            error: "offline".into(),
            browser_error: None,
        };
        for (prompt, label, can_install, expected) in [
            (&restart, "update.restart_now", true, Some(Choice::Restart)),
            (&restart, "update.next_launch", true, Some(Choice::Later)),
            (&restart, "update.restart_now", false, None),
            (&failure, "update.manual", false, Some(Choice::Manual)),
            (&failure, "update.resume", true, Some(Choice::Resume)),
            (&failure, "update.later", false, Some(Choice::Later)),
        ] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            let palette = theme::palette(true);
            let mut button = None;
            for frame in 0..8 {
                let events = match frame {
                    2 => vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    3 | 4 | 6 | 7 => {
                        let pos = if frame < 5 {
                            egui::pos2(4.0, 4.0)
                        } else {
                            button.expect("visible choice")
                        };
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: frame == 3 || frame == 6,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ]
                    }
                    _ => Vec::new(),
                };
                let mut choice = None;
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(408.0, 482.0),
                        )),
                        time: Some(frame as f64 / 60.0),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        choice = prompt.show(ctx, &palette, Lang::EN, can_install);
                    },
                );
                button = output
                    .shapes
                    .iter()
                    .find_map(|shape| label_position(&shape.shape, Lang::EN.t(label)));
                assert_eq!(
                    choice.as_ref(),
                    if frame == 7 { expected.as_ref() } else { None },
                    "{label}, frame {frame}"
                );
            }
        }
    }
}
