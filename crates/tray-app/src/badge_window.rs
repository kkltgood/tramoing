//! UI → 原生任务栏控件。egui 不再创建网速 viewport。
use eframe::egui;
use tm_windows::badge::{Content, Performance, Speed, TaskbarBadge};

pub struct BadgeState {
    pub speed: Option<(f64, f64)>,
    pub performance: bool,
    pub reading: Option<crate::performance::Reading>,
    pub temperatures: bool,
    pub light: bool,
    pub colored: bool,
    pub performance_colored: bool,
    pub colors: tm_core::color::DisplayColors,
    pub order: tm_core::taskbar::TaskbarOrder,
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
            performance: s.performance.then(|| Performance {
                cpu: crate::performance::menu_percent(s.reading.as_ref().and_then(|r| r.cpu)),
                gpu: crate::performance::menu_percent(s.reading.as_ref().and_then(|r| r.gpu)),
                temperatures: s.temperatures.then(|| {
                    (
                        crate::performance::menu_temperature(
                            s.reading.as_ref().and_then(|r| r.temperatures.cpu),
                        ),
                        crate::performance::menu_temperature(
                            s.reading.as_ref().and_then(|r| r.temperatures.gpu),
                        ),
                    )
                }),
                colored: s.performance_colored,
            }),
            light: s.light,
            colors: s.colors,
            order: s.order,
        }));
        let error = result.err();
        if error != self.error {
            if let Some(e) = error {
                log::error!(target: "tramoing::taskbar", "{e}");
            }
            self.error = error;
        }
    }
}
