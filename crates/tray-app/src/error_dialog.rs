//! 错误提示与日志导出入口；运行中使用面板弹框，启动失败使用原生弹框。
use std::path::PathBuf;

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
}

impl Prompt {
    pub fn new(notices: Vec<ErrorNotice>) -> Self {
        let mut prompt = Self {
            notices: Vec::new(),
        };
        prompt.append(notices);
        prompt
    }

    pub fn append(&mut self, notices: Vec<ErrorNotice>) {
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

    pub fn show(
        &self,
        ctx: &egui::Context,
        p: &Palette,
        lang: Lang,
        busy: bool,
        result: Option<&Result<PathBuf, String>>,
    ) -> Action {
        let mut action = Action::None;
        let modal = egui::Modal::new(egui::Id::new("error-diagnostics"))
            .frame(
                egui::Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(14)
                    .inner_margin(18),
            )
            .backdrop_color(Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(310.0);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);
                ui.label(
                    RichText::new(lang.t("errors.something_went_wrong"))
                        .size(17.0)
                        .color(p.text),
                );
                egui::ScrollArea::vertical()
                    .id_salt("error-details")
                    .max_height(96.0)
                    .show(ui, |ui| {
                        for notice in &self.notices {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&notice.message).size(12.0).color(p.warn),
                                )
                                .wrap(),
                            );
                        }
                    });
                ui.add(
                    egui::Label::new(
                        RichText::new(lang.t("errors.export_logs_to_help_diagnose_this_problem"))
                            .color(p.text),
                    )
                    .wrap(),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(lang.t("errors.the_report_is_saved_locally_and_is_not"))
                            .size(11.0)
                            .color(p.subtext),
                    )
                    .wrap(),
                );
                if busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(lang.t("errors.exporting_logs"));
                    });
                } else if let Some(result) = result {
                    egui::ScrollArea::vertical()
                        .id_salt("error-export-result")
                        .max_height(70.0)
                        .show(ui, |ui| {
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
                                egui::Label::new(RichText::new(text).size(11.0).color(color))
                                    .wrap()
                                    .selectable(true),
                            );
                        });
                }
                ui.horizontal(|ui| {
                    let dismiss = if result.is_some() {
                        lang.t("common.close")
                    } else {
                        lang.t("common.not_now")
                    };
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(dismiss).min_size(Vec2::new(105.0, 32.0)),
                        )
                        .clicked()
                    {
                        action = Action::Dismiss;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let saved = matches!(result, Some(Ok(_)));
                        let label = if saved {
                            lang.t("common.show_file")
                        } else {
                            lang.t("diagnostics.export_logs")
                        };
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(RichText::new(label).color(Color32::WHITE))
                                    .fill(p.accent)
                                    .min_size(Vec2::new(132.0, 32.0)),
                            )
                            .clicked()
                        {
                            action = if saved {
                                Action::Reveal
                            } else {
                                Action::Export
                            };
                        }
                    });
                });
            });
        if !busy && modal.should_close() {
            action = Action::Dismiss;
        }
        action
    }
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
    fn dialog_requires_consent_allows_retry_and_only_reveals_a_completed_report() {
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
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            Vec2::new(408.0, 482.0),
                        )),
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
                if frame < 3 {
                    assert_eq!(action, Action::None);
                } else {
                    assert_eq!(action, expected, "button={label} busy={busy}");
                }
            }
        }
    }
}
