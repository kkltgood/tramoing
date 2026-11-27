//! 错误提示与日志导出入口；运行中使用面板弹框，启动失败使用原生弹框。
use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use tm_diagnostics::ErrorNotice;

use crate::theme::{Lang, Palette};

#[derive(Default, Debug, PartialEq, Eq)]
pub enum Action {
    #[default]
    None,
    Export,
    Reveal,
    Dismiss,
}

pub struct Prompt {
    notices: Vec<ErrorNotice>,
    sampler_healthy: bool,
    received_at: Instant,
}

impl Prompt {
    pub fn new(notices: Vec<ErrorNotice>) -> Self {
        let mut prompt = Self {
            notices: Vec::new(),
            sampler_healthy: false,
            received_at: Instant::now(),
        };
        prompt.append(notices);
        prompt
    }

    pub fn append(&mut self, notices: Vec<ErrorNotice>) {
        if !notices.is_empty() {
            self.received_at = Instant::now();
            self.sampler_healthy = false;
        }
        for notice in notices {
            if self.notices.len() < 8 && !self.notices.contains(&notice) {
                self.notices.push(notice);
            }
        }
    }

    pub fn context(&self) -> String {
        self.notices
            .iter()
            .map(|notice| format!("[{}] {}", notice.target, notice.message))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn observe_sampler(&mut self, published_at: Option<Instant>, healthy: bool) {
        // Logging wakes the UI before the engine publishes its failure snapshot.
        // The preceding successful snapshot must not turn a new error green.
        self.sampler_healthy = healthy && published_at.is_some_and(|at| at > self.received_at);
    }

    fn sampler_only(&self) -> bool {
        !self.notices.is_empty()
            && self
                .notices
                .iter()
                .all(|notice| notice.target == "tramoing::sampler")
    }

    fn recovered(&self) -> bool {
        self.sampler_only() && self.sampler_healthy
    }

    pub fn show(
        &self,
        ctx: &egui::Context,
        p: &Palette,
        lang: Lang,
        busy: bool,
        result: Option<&Result<PathBuf, String>>,
    ) -> Action {
        let mut action = Action::None;
        let viewport = ctx.content_rect();
        let width = (viewport.width() - 32.0).clamp(1.0, 346.0);
        let inset = if width < 280.0 { 14 } else { 20 };
        let recovered = self.recovered();
        let signal = if recovered { p.download } else { p.warn };
        let modal_id = egui::Id::new("error-diagnostics");
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            modal_id.with("shade"),
        ))
        .rect_filled(
            viewport,
            16,
            Color32::from_rgba_unmultiplied(14, 22, 39, if p.dark { 166 } else { 108 }),
        );
        let modal = egui::Modal::new(modal_id)
            .frame(
                egui::Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0_f32, p.border))
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
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
                let header = egui::Frame::NONE
                    .fill(signal.gamma_multiply(if p.dark { 0.10 } else { 0.07 }))
                    .corner_radius(egui::CornerRadius {
                        nw: 13,
                        ne: 13,
                        sw: 0,
                        se: 0,
                    })
                    .inner_margin(egui::Margin::symmetric(inset, 9))
                    .show(ui, |ui| {
                        ui.set_min_width((width - 2.0 * f32::from(inset)).max(1.0));
                        ui.horizontal(|ui| {
                            let (mark, _) =
                                ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                            ui.painter().circle_stroke(
                                mark.center(),
                                7.0,
                                Stroke::new(1.5_f32, signal),
                            );
                            if recovered {
                                ui.painter().line(
                                    vec![
                                        mark.center() + egui::vec2(-3.0, 0.0),
                                        mark.center() + egui::vec2(-0.7, 2.2),
                                        mark.center() + egui::vec2(3.6, -2.4),
                                    ],
                                    Stroke::new(1.5_f32, signal),
                                );
                            } else {
                                ui.painter().line_segment(
                                    [
                                        mark.center() + egui::vec2(0.0, -3.0),
                                        mark.center() + egui::vec2(0.0, 0.5),
                                    ],
                                    Stroke::new(1.5_f32, signal),
                                );
                                ui.painter().circle_filled(
                                    mark.center() + egui::vec2(0.0, 3.0),
                                    0.9,
                                    signal,
                                );
                            }
                            ui.label(
                                RichText::new(lang.t(if recovered {
                                    "errors.recovered_status"
                                } else {
                                    "errors.attention_status"
                                }))
                                .size(12.0)
                                .color(signal),
                            );
                        });
                    });
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(inset, 18))
                    .show(ui, |ui| {
                        let stack = ui.available_width() < 250.0;
                        let footer_height = if stack { 80.0 } else { 36.0 };
                        let body_height = viewport.height()
                            - 32.0
                            - header.response.rect.height()
                            - 36.0
                            - 16.0
                            - footer_height;
                        egui::ScrollArea::vertical()
                            .id_salt("error-message")
                            .max_height(body_height.max(28.0))
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                self.message(ui, p, lang, busy, result);
                            });
                        ui.add_space(16.0);
                        let saved = matches!(result, Some(Ok(_)));
                        let button_width = if stack {
                            ui.available_width()
                        } else {
                            (ui.available_width() - 8.0) / 2.0
                        };
                        let mut actions = |ui: &mut egui::Ui| {
                            if prompt_button(
                                ui,
                                p,
                                lang.t(if saved {
                                    "common.show_file"
                                } else {
                                    "diagnostics.export_logs"
                                }),
                                button_width,
                                true,
                                !busy,
                            ) {
                                action = if saved {
                                    Action::Reveal
                                } else {
                                    Action::Export
                                };
                            }
                            if stack {
                                ui.add_space(8.0);
                            }
                            let dismiss = if result.is_some() || recovered {
                                lang.t("common.close")
                            } else {
                                lang.t("common.not_now")
                            };
                            if prompt_button(ui, p, dismiss, button_width, false, !busy) {
                                action = Action::Dismiss;
                            }
                        };
                        if stack {
                            actions(ui);
                        } else {
                            ui.horizontal(actions);
                        }
                    });
            });
        if !busy && modal.should_close() {
            action = Action::Dismiss;
        }
        action
    }

    fn message(
        &self,
        ui: &mut egui::Ui,
        p: &Palette,
        lang: Lang,
        busy: bool,
        result: Option<&Result<PathBuf, String>>,
    ) {
        let (title, body) = if self.recovered() {
            (
                "errors.collector_recovered",
                "errors.collector_recovered_body",
            )
        } else if self.sampler_only() {
            (
                "errors.collector_interrupted",
                "errors.collector_interrupted_body",
            )
        } else {
            ("errors.something_went_wrong", "errors.diagnostic_help")
        };
        ui.add(
            egui::Label::new(
                RichText::new(lang.t(title))
                    .font(crate::theme::display_font(22.0))
                    .color(p.text),
            )
            .wrap(),
        );
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(
                RichText::new(lang.t(body))
                    .size(13.0)
                    .line_height(Some(19.0))
                    .color(p.subtext),
            )
            .wrap(),
        );
        ui.add_space(12.0);
        egui::Frame::NONE
            .fill(p.chip)
            .corner_radius(8)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_width((ui.available_width()).max(1.0));
                egui::CollapsingHeader::new(
                    RichText::new(lang.t("ui.error_details"))
                        .size(12.0)
                        .color(p.subtext),
                )
                .id_salt("error-details")
                .default_open(!self.sampler_only())
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for notice in &self.notices {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&notice.message).size(12.0).color(p.text),
                            )
                            .wrap()
                            .selectable(true),
                        );
                    }
                });
            });
        ui.add_space(12.0);
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new(lang.t("errors.exporting_logs"))
                        .size(12.0)
                        .color(p.subtext),
                );
            });
        } else if let Some(result) = result {
            let (text, color) = match result {
                Ok(path) => (
                    lang.format(
                        "errors.export_result",
                        &[("path", &path.display().to_string())],
                    ),
                    p.subtext,
                ),
                Err(error) => (
                    lang.format("errors.export_failed_detail", &[("error", error)]),
                    p.warn,
                ),
            };
            ui.add(
                egui::Label::new(RichText::new(text).size(11.5).color(color))
                    .wrap()
                    .selectable(true),
            );
            ui.add_space(8.0);
        }
        ui.add(
            egui::Label::new(
                RichText::new(lang.t("errors.the_report_is_saved_locally_and_is_not"))
                    .size(11.0)
                    .line_height(Some(16.0))
                    .color(p.unit),
            )
            .wrap(),
        );
    }
}

fn prompt_button(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    width: f32,
    primary: bool,
    enabled: bool,
) -> bool {
    let (fill, ink) = if primary && p.dark {
        (p.selection_top, p.panel_bottom)
    } else if primary {
        (p.selection, p.on_selection)
    } else {
        (Color32::TRANSPARENT, p.subtext)
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).size(12.5).color(ink))
            .fill(fill)
            .stroke(if primary {
                Stroke::NONE
            } else {
                Stroke::new(1.0_f32, p.border)
            })
            .corner_radius(8)
            .min_size(Vec2::new(width, 36.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .clicked()
}

pub const REPORT_FLAG: &str = "--report-error";

fn native_dialog(title: &str, detail: &str, confirm: &str, cancel: Option<&str>) -> bool {
    #[cfg(target_os = "macos")]
    {
        tm_macos::dialog::show(title, detail, confirm, cancel)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_YESNO,
        };
        let _ = confirm; // MessageBox 使用系统本地化的“是 / 否”按钮。
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let detail: Vec<u16> = detail.encode_utf16().chain(Some(0)).collect();
        let buttons = if cancel.is_some() { MB_YESNO } else { MB_OK };
        // SAFETY: 字符串以 NUL 结尾并在阻塞调用期间保持存活，不使用窗口句柄。
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                detail.as_ptr(),
                title.as_ptr(),
                buttons | MB_ICONERROR | MB_SETFOREGROUND,
            ) == IDYES
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (title, detail, confirm, cancel);
        false
    }
}

/// 主窗口创建失败时仍然可用。只在用户选择导出后收集系统信息。
pub fn report_fatal(message: &str) {
    let lang = crate::i18n::resolve(crate::settings::Settings::load().lang);
    let detail = lang.format(
        "errors.fatal_detail",
        &[("message", &message.chars().take(2048).collect::<String>())],
    );
    if !native_dialog(
        lang.t("errors.tramoing_encountered_an_error"),
        &detail,
        lang.t("diagnostics.export_logs"),
        Some(lang.t("common.not_now")),
    ) {
        return;
    }
    match crate::diagnostics::export_report(message) {
        Ok(path) => {
            let detail = lang.format(
                "errors.report_saved_detail",
                &[("path", &path.display().to_string())],
            );
            if native_dialog(
                lang.t("diagnostics.logs_exported"),
                &detail,
                lang.t("common.show_file"),
                Some(lang.t("common.close")),
            ) {
                if let Err(error) = crate::diagnostics::reveal(&path) {
                    native_dialog(
                        lang.t("errors.could_not_show_file"),
                        &lang.format(
                            "errors.show_file_detail",
                            &[("error", &error), ("path", &path.display().to_string())],
                        ),
                        lang.t("common.close"),
                        None,
                    );
                }
            }
        }
        Err(error) => {
            log::error!(target: "tramoing::diagnostics", "导出失败：{error}");
            native_dialog(
                lang.t("errors.log_export_failed"),
                &error,
                lang.t("common.close"),
                None,
            );
        }
    }
}

fn crash_marker() -> PathBuf {
    tm_diagnostics::log_path()
        .and_then(|path| path.parent())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(tm_store::data_dir)
        .join("pending-crash.txt")
}

pub fn take_pending_crash() -> Option<String> {
    use std::io::Read;
    let path = crash_marker();
    let mut text = String::new();
    std::fs::File::open(&path)
        .ok()?
        .take(8192)
        .read_to_string(&mut text)
        .ok()?;
    let _ = std::fs::remove_file(path);
    Some(text)
}

/// release 使用 panic=abort；在 hook 返回前启动独立进程，避免依赖已损坏的 UI。
pub fn install_crash_reporter() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    let previous = std::panic::take_hook();
    let marker = crash_marker();
    std::panic::set_hook(Box::new(move |info| {
        // 先同步保存 panic 和回溯。
        previous(info);
        // debug 中后台线程通常只会展开退出，仍由存活的主界面接收日志事件。
        if cfg!(panic = "unwind") && std::thread::current().name() != Some("main") {
            return;
        }
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        // Preserve raw panic details; the report process resolves the current language.
        let message: String = info.to_string().chars().take(2048).collect();
        let _ = std::fs::write(&marker, &message);
        if let Ok(executable) = std::env::current_exe() {
            let mut command = std::process::Command::new(executable);
            command
                .arg(REPORT_FLAG)
                .arg(&message)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let _ = command.spawn(); // 启动失败时下次启动仍可读 pending-crash。
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label_position(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
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
    fn sampler_recovery_does_not_hide_other_errors_or_discard_report_context() {
        let notice = ErrorNotice {
            target: "tramoing::sampler".into(),
            message: "Collection interrupted".into(),
        };
        let mut prompt = Prompt::new(vec![notice]);
        let context = prompt.context();
        assert!(!prompt.recovered());
        prompt.observe_sampler(Some(prompt.received_at), true);
        assert!(!prompt.recovered());
        prompt.observe_sampler(None, true);
        assert!(!prompt.recovered());
        let after = prompt.received_at + std::time::Duration::from_secs(1);
        prompt.observe_sampler(Some(after), true);
        assert!(prompt.recovered());
        assert_eq!(prompt.context(), context);
        prompt.observe_sampler(Some(after), false);
        assert!(!prompt.recovered());
        prompt.observe_sampler(Some(after), true);
        prompt.append(vec![ErrorNotice {
            target: "tramoing::store".into(),
            message: "Disk full".into(),
        }]);
        assert!(!prompt.recovered());
        assert!(prompt.context().contains("Disk full"));
    }

    #[test]
    fn dialog_requires_consent_allows_retry_and_only_reveals_a_completed_report() {
        for size in [Vec2::new(408.0, 482.0), Vec2::new(280.0, 330.0)] {
            for (busy, result, label, expected) in [
                (false, None, "Not now", Action::Dismiss),
                (false, None, "Export logs", Action::Export),
                (
                    false,
                    Some(Err("disk full".to_owned())),
                    "Export logs",
                    Action::Export,
                ),
                (
                    false,
                    Some(Ok(PathBuf::from("report.txt"))),
                    "Show file",
                    Action::Reveal,
                ),
                (true, None, "Export logs", Action::None),
                (true, None, "Not now", Action::None),
            ] {
                let ctx = egui::Context::default();
                crate::theme::install_cjk(&ctx);
                let prompt = Prompt::new(vec![ErrorNotice {
                    target: "sampler".into(),
                    message: "Connection interrupted".into(),
                }]);
                let mut position = None;
                for frame in 0..4 {
                    let mut events = Vec::new();
                    if frame >= 2 {
                        let pos = position.expect("dialog button must be visible");
                        events.push(egui::Event::PointerMoved(pos));
                        events.push(egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: frame == 2,
                            modifiers: egui::Modifiers::NONE,
                        });
                    }
                    let mut action = Action::None;
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            time: Some(frame as f64 / 60.0),
                            events,
                            ..Default::default()
                        },
                        |ctx| {
                            action = prompt.show(
                                ctx,
                                &crate::theme::palette(true),
                                Lang::EN,
                                busy,
                                result.as_ref(),
                            );
                        },
                    );
                    position = output
                        .shapes
                        .iter()
                        .find_map(|shape| label_position(&shape.shape, label));
                    if let Some(position) = position {
                        assert!(
                            egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains(position),
                            "{label} outside {size:?}"
                        );
                    }
                    if frame < 3 {
                        assert_eq!(action, Action::None);
                    } else {
                        assert_eq!(action, expected, "button={label} busy={busy}");
                    }
                }
            }
        }
    }
}
