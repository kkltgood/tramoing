//! UI → 原生任务栏控件。egui 不再创建网速 viewport。
use eframe::egui;
use tm_windows::badge::{Content, Performance, Speed, TaskbarBadge};

pub struct BadgeState {
    pub speed: Option<(f64, f64)>,
    pub performance: Option<(Option<f64>, Option<f64>)>,
    pub light: bool,
    pub colored: bool,
    pub performance_colored: bool,
    pub colors: tm_core::color::DisplayColors,
    pub performance_first: bool,
}

pub struct Badge {
    native: TaskbarBadge,
    pub error: Option<&'static str>,
}

impl Badge {
    pub fn new() -> Self {
        Self {
            native: TaskbarBadge::new(crate::app::badge_clicked, crate::app::badge_reordered),
            error: None,
        }
    }

    pub fn show(&mut self, _ctx: &egui::Context, state: Option<BadgeState>) {
        let result = self.native.update(state.map(|s| Content {
            speed: s.speed.map(|(rx, tx)| Speed {
                down: crate::app::menu_speed(rx),
                up: crate::app::menu_speed(tx),
                colored: s.colored,
            }),
            performance: s.performance.map(|(cpu, gpu)| Performance {
                cpu: crate::performance::menu_percent(cpu),
                gpu: crate::performance::menu_percent(gpu),
                colored: s.performance_colored,
            }),
            light: s.light,
            colors: s.colors,
            performance_first: s.performance_first,
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
