//! UI → 原生任务栏控件。egui 不再创建网速 viewport。
use eframe::egui;
use tm_windows::badge::{Content, TaskbarBadge};

pub struct BadgeState {
    pub rx: f64,
    pub tx: f64,
    pub light: bool,
    pub colored: bool,
}

pub struct Badge {
    native: TaskbarBadge,
    pub error: Option<&'static str>,
}

impl Badge {
    pub fn new() -> Self {
        Self {
            native: TaskbarBadge::new(crate::app::badge_clicked),
            error: None,
        }
    }

    pub fn show(&mut self, _ctx: &egui::Context, state: Option<BadgeState>) {
        let result = self.native.update(state.map(|s| Content {
            down: crate::app::menu_speed(s.rx),
            up: crate::app::menu_speed(s.tx),
            light: s.light,
            colored: s.colored,
        }));
        let error = result.err();
        if error != self.error {
            if let Some(e) = error {
                crate::update::log_line(&format!("[taskbar] {e}"));
            }
            self.error = error;
        }
    }
}
