mod groups;
mod help;
#[cfg(test)]
mod tests;

use super::*;
use crate::engine::connections::{Details, DISPLAY_LIMIT};
use groups::{Group, Kind};
use tm_core::{Connection, ConnectionEndpoint, ConnectionState};

const INSET: f32 = 12.0;
const STATE_WIDTH: f32 = 78.0;
const TOGGLE_WIDTH: f32 = 106.0;
const REVEAL_SECONDS: f32 = 0.2;

#[derive(Clone, Default)]
struct DetailsUi {
    expanded: Option<egui::Id>,
    initialized: bool,
    copied: Option<(egui::Id, Instant)>,
}

impl App {
    pub(super) fn connections_body(&mut self, ui: &mut egui::Ui, p: &Palette, lang: Lang) {
        let Some(app) = self.connection_app.clone() else {
            return;
        };
        let details = self.rx.connections.details(&app);
        #[cfg(debug_assertions)]
        let details = crate::ui_capture::connection_details(details);
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), egui::Sense::hover());
        let back = egui::Rect::from_min_size(header.min + egui::vec2(0.0, 6.0), Vec2::splat(30.0));
        if icon_button(ui, back, "connections-back", p, paint_back_icon)
            .on_hover_text(lang.t("common.back"))
            .clicked()
            || (self.view == View::Connections
                && !egui::Popup::is_any_open(ui.ctx())
                && ui.input(|i| i.key_pressed(egui::Key::Escape)))
        {
            self.goto(View::Main);
        }
        let title = egui::Rect::from_min_max(
            header.min + egui::vec2(42.0, 0.0),
            header.right_top() + egui::vec2(-40.0, 24.0),
        );
        ellipsis_text(
            ui.painter(),
            title,
            &pretty_name(app.as_str()),
            17.0,
            p.text,
            Align::Min,
        );
        ui.painter().text(
            header.min + egui::vec2(42.0, 33.0),
            egui::Align2::LEFT_CENTER,
            lang.t("connections.title"),
            egui::FontId::proportional(11.0),
            p.subtext,
        );
        let info = egui::Rect::from_min_size(
            header.right_top() + egui::vec2(-30.0, 6.0),
            Vec2::splat(30.0),
        );
        let response = icon_button(ui, info, "connections-info", p, paint_info);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, lang.t("connections.help"))
        });
        #[cfg(windows)]
        let restriction = self.network_access.rule(app.as_str()).map(|rule| {
            if self.network_access.verified && rule.complete {
                lang.t("ui.blocked_use_allow_to_restore_access")
            } else {
                lang.t("ui.network_rules_need_verification")
            }
        });
        #[cfg(not(windows))]
        let restriction = None;
        help::show(
            &response,
            header,
            &app,
            details.as_ref(),
            restriction,
            p,
            lang,
        );
        ui.add_space(14.0);
        #[cfg(not(windows))]
        let error = self.latest.as_ref().and_then(|s| s.error.as_deref());
        #[cfg(windows)]
        let error = None;
        details_body(ui, &app, details.as_ref(), error, p, lang);
    }
}

fn paint_info(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    painter.circle_stroke(center, 7.0, Stroke::new(1.0, color));
    painter.circle_filled(center + egui::vec2(0.0, -3.2), 0.8, color);
    painter.line_segment(
        [
            center + egui::vec2(0.0, -0.5),
            center + egui::vec2(0.0, 4.0),
        ],
        Stroke::new(1.2, color),
    );
}

fn details_body(
    ui: &mut egui::Ui,
    app: &AppKey,
    details: Option<&Details>,
    error: Option<&str>,
    p: &Palette,
    lang: Lang,
) {
    if let Some(error) = error {
        collection_error(ui, error, p, lang);
        return;
    }
    let Some(details) = details else {
        ui.label(RichText::new(lang.t("connections.loading")).color(p.subtext));
        return;
    };
    if details.updated_at.elapsed() > Duration::from_secs(6) {
        ui.label(RichText::new(lang.t("connections.stale")).color(p.warn));
        return;
    }
    let report = match &details.result {
        Ok(report) => report,
        Err(error) => {
            collection_error(ui, error, p, lang);
            return;
        }
    };
    let groups = groups::collect(&report.connections);
    let incomplete = report.warning.is_some() || report.unverified_processes > 0;
    let (overview, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), egui::Sense::hover());
    let count = if details.total > DISPLAY_LIMIT {
        lang.format(
            "connections.shown",
            &[
                ("shown", &report.connections.len().to_string()),
                ("total", &details.total.to_string()),
            ],
        )
    } else {
        lang.format(
            "connections.records",
            &[("count", &details.total.to_string())],
        )
    };
    ui.painter().text(
        overview.left_center(),
        egui::Align2::LEFT_CENTER,
        count,
        theme::display_font(12.0),
        p.text,
    );
    ui.painter().text(
        overview.right_center(),
        egui::Align2::RIGHT_CENTER,
        lang.format(
            "connections.groups",
            &[("count", &groups.len().to_string())],
        ),
        egui::FontId::proportional(11.0),
        p.subtext,
    );
    ui.painter().line_segment(
        [overview.left_bottom(), overview.right_bottom()],
        Stroke::new(0.7, p.border),
    );
    ui.add_space(8.0);
    let id = ui.id().with(("connections-state", app.as_str()));
    let mut state = ui
        .ctx()
        .data_mut(|data| data.get_temp::<DetailsUi>(id).unwrap_or_default());
    if !state.initialized && !groups.is_empty() {
        state.expanded = (groups.len() == 1).then(|| groups[0].id());
        state.initialized = true;
    }
    if state
        .expanded
        .is_some_and(|id| !groups.iter().any(|group| group.id() == id))
    {
        state.expanded = None;
    }
    #[cfg(debug_assertions)]
    if crate::ui_capture::enabled()
        && matches!(
            std::env::var("TM_CAPTURE_STATE").as_deref(),
            Ok("connections-expanded" | "connections-long")
        )
    {
        state.expanded = groups.first().map(Group::id);
    }
    let bounds = ui.max_rect();
    let page_clip = ui.clip_rect();
    let height = (ui.available_height() - 28.0).max(1.0);
    let (list_rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );
    // Keep cards aligned with the page on both sides. The scrollbar uses part
    // of the existing panel padding instead of taking width away from cards.
    let mut list_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("connections-scroll")
            .max_rect(list_rect.with_max_x(list_rect.right() + SCROLL_GUTTER)),
    );
    list_ui.set_clip_rect(
        page_clip
            .with_max_x(page_clip.right() + SCROLL_GUTTER)
            .intersect(ui.ctx().content_rect()),
    );
    let scroll = &mut list_ui.spacing_mut().scroll;
    scroll.floating = true;
    scroll.floating_allocated_width = 0.0;
    scroll.floating_width = 3.0;
    scroll.bar_width = 6.0;
    scroll.handle_min_length = 24.0;
    scroll.foreground_color = false;
    scroll.dormant_background_opacity = 0.0;
    scroll.active_background_opacity = 0.0;
    scroll.interact_background_opacity = 0.0;
    scroll.dormant_handle_opacity = 1.0;
    scroll.active_handle_opacity = 1.0;
    scroll.interact_handle_opacity = 1.0;
    let visuals = list_ui.visuals_mut();
    visuals.widgets.inactive.bg_fill = mix(p.border, p.subtext, 0.35);
    visuals.widgets.hovered.bg_fill = mix(p.border, p.accent, 0.65);
    visuals.widgets.active.bg_fill = p.accent;
    egui::ScrollArea::vertical()
        .id_salt(("connections-list", app.as_str()))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .max_height(height)
        .auto_shrink([false, false])
        .show(&mut list_ui, |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(page_clip));
            ui.set_width(list_rect.width());
            if incomplete {
                ui.add(
                    egui::Label::new(
                        RichText::new(lang.t("connections.incomplete"))
                            .size(11.0)
                            .color(p.warn),
                    )
                    .wrap(),
                );
                ui.add_space(8.0);
            }
            if groups.is_empty() {
                ui.add(
                    egui::Label::new(
                        RichText::new(lang.t(if incomplete {
                            "connections.empty_incomplete"
                        } else {
                            "connections.empty"
                        }))
                        .color(p.subtext),
                    )
                    .wrap(),
                );
            }
            let mut previous = None;
            for group in &groups {
                if previous != Some(group.kind) {
                    if previous.is_some() {
                        ui.add_space(8.0);
                    }
                    section_heading(ui, group.kind, p, lang);
                    previous = Some(group.kind);
                }
                ui.push_id((app.as_str(), group.id()), |ui| {
                    group_row(ui, group, &mut state, p, lang)
                });
                ui.add_space(6.0);
            }
        });
    ui.ctx().data_mut(|data| data.insert_temp(id, state));
    let footer = egui::Rect::from_min_max(
        egui::pos2(bounds.left(), bounds.bottom() - 22.0),
        bounds.max,
    );
    ui.painter().line_segment(
        [footer.left_top(), footer.right_top()],
        Stroke::new(0.7, p.border),
    );
    if !groups.is_empty() {
        ui.painter().text(
            footer.left_center() + egui::vec2(0.0, 3.0),
            egui::Align2::LEFT_CENTER,
            lang.t("connections.group_hint"),
            egui::FontId::proportional(10.0),
            p.unit,
        );
    }
    ui.painter().text(
        footer.right_center() + egui::vec2(0.0, 3.0),
        egui::Align2::RIGHT_CENTER,
        lang.t("connections.refresh_short"),
        egui::FontId::proportional(10.0),
        p.unit,
    );
}

fn section_heading(ui: &mut egui::Ui, kind: Kind, p: &Palette, lang: Lang) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), egui::Sense::hover());
    let (title, note) = match kind {
        Kind::Local => (
            "connections.section_local",
            "connections.section_local_note",
        ),
        Kind::Remote => (
            "connections.section_remote",
            "connections.section_remote_note",
        ),
        Kind::Listener => (
            "connections.section_listener",
            "connections.section_listener_note",
        ),
        Kind::Bound => (
            "connections.section_bound",
            "connections.section_bound_note",
        ),
    };
    ui.painter().text(
        rect.min + egui::vec2(0.0, 12.0),
        egui::Align2::LEFT_CENTER,
        lang.t(title),
        theme::display_font(12.0),
        p.text,
    );
    ellipsis_text(
        ui.painter(),
        egui::Rect::from_min_max(
            rect.min + egui::vec2(0.0, 24.0),
            rect.max - egui::vec2(0.0, 4.0),
        ),
        lang.t(note),
        10.0,
        p.subtext,
        Align::Min,
    );
}

fn collection_error(ui: &mut egui::Ui, error: &str, p: &Palette, lang: Lang) {
    ui.add(egui::Label::new(RichText::new(lang.t("connections.failed")).color(p.warn)).wrap());
    ui.add_space(6.0);
    ui.add(egui::Label::new(RichText::new(error).size(11.0).color(p.subtext)).wrap());
}

fn group_row(ui: &mut egui::Ui, group: &Group<'_>, state: &mut DetailsUi, p: &Palette, lang: Lang) {
    let id = group.id();
    let open = state.expanded == Some(id);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 58.0), egui::Sense::click());
    let status = group
        .uniform_state()
        .map(|state| state_label(state, lang))
        .unwrap_or(lang.t("connections.mixed_states"));
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::CollapsingHeader,
            true,
            open,
            format!("{} · {} · {status}", group.endpoint, group.rows.len()),
        )
    });
    if response.clicked() {
        state.expanded = if open { None } else { Some(id) };
    }
    let open = state.expanded == Some(id);
    let animation_id = ui.id().with("group-reveal");
    let height = ui.ctx().data(|data| data.get_temp::<f32>(animation_id));
    if height.is_none() {
        // Seed closed so the initial single-group reveal also animates.
        ui.ctx().animate_bool_with_time(animation_id, false, 0.0);
    }
    let duration = if ui.style().animation_time > 0.0 {
        REVEAL_SECONDS
    } else {
        0.0
    };
    let progress = ui
        .ctx()
        .animate_bool_with_time(animation_id, open, duration);
    // Use the same curve in both directions, keeping quick reversals continuous.
    let openness = progress * progress * (3.0 - 2.0 * progress);
    #[cfg(debug_assertions)]
    let openness = crate::ui_capture::connection_reveal(openness);
    let background = mix(
        p.surface,
        p.accent,
        0.04 * if response.hovered() { 1.0 } else { openness },
    );
    ui.painter().rect(
        rect,
        8.0,
        background,
        Stroke::new(0.7, mix(p.border, p.accent, 0.35 * openness)),
        egui::StrokeKind::Inside,
    );
    surface::focus(ui.painter(), rect, &response, p);
    let toggle = egui::Rect::from_center_size(
        egui::pos2(rect.right() - INSET - TOGGLE_WIDTH / 2.0, rect.center().y),
        egui::vec2(TOGGLE_WIDTH, 28.0),
    );
    let title_rect = egui::Rect::from_min_max(
        rect.min + egui::vec2(INSET, 8.0),
        egui::pos2(toggle.left() - 10.0, rect.top() + 32.0),
    );
    endpoint_title(ui, title_rect, group.endpoint, p);
    ui.painter().rect(
        toggle,
        6.0,
        mix(p.chip, p.accent, 0.04 * openness),
        Stroke::new(0.7, p.border),
        egui::StrokeKind::Inside,
    );
    let divider = toggle.right() - 26.0;
    let status_rect = egui::Rect::from_min_max(
        toggle.min + egui::vec2(6.0, 0.0),
        egui::pos2(divider - 6.0, toggle.bottom()),
    );
    let color = if group.uniform_state() == Some(ConnectionState::Established) {
        p.accent
    } else {
        p.subtext
    };
    ellipsis_text(
        ui.painter(),
        status_rect,
        status,
        10.0,
        color,
        Align::Center,
    );
    ui.painter().line_segment(
        [
            egui::pos2(divider, toggle.top() + 7.0),
            egui::pos2(divider, toggle.bottom() - 7.0),
        ],
        Stroke::new(0.7, p.border),
    );
    let center = egui::pos2(divider + 13.0, toggle.center().y);
    let rotation = egui::emath::Rot2::from_angle(openness * std::f32::consts::FRAC_PI_2);
    let points = [
        egui::vec2(-1.5, -3.0),
        egui::vec2(1.5, 0.0),
        egui::vec2(-1.5, 3.0),
    ]
    .map(|point| center + rotation * point);
    ui.painter().add(egui::Shape::line(
        points.to_vec(),
        Stroke::new(1.3, p.subtext),
    ));
    let meta = lang.format(
        "connections.group_meta",
        &[
            ("protocol", group.protocol.label()),
            ("count", &group.rows.len().to_string()),
        ],
    );
    ui.painter().text(
        rect.min + egui::vec2(INSET, 42.0),
        egui::Align2::LEFT_CENTER,
        meta,
        egui::FontId::proportional(10.0),
        p.subtext,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let full_height = if open || openness > 0.0 {
        let mut body = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("group-body")
                .max_rect(ui.available_rect_before_wrap()),
        );
        if openness < 1.0 {
            let mut clip = body.clip_rect();
            clip.max.y = clip
                .max
                .y
                .min(body.max_rect().top() + height.unwrap_or_default() * openness);
            body.set_clip_rect(clip);
        }
        if !open || openness < 1.0 {
            // Hidden rows must not receive pointer or keyboard input. Keep
            // the parent page's opacity rather than the disabled tint.
            let opacity = body.opacity();
            body.disable();
            body.set_opacity(opacity);
        }
        body.multiply_opacity(openness);
        group_body(&mut body, group, state, p, lang);
        let full_height = body.min_rect().height();
        // Match the clip height, including when a fresh snapshot changes the
        // row count mid-animation. Cache the new measurement for the next frame.
        let visible_height = if openness < 1.0 {
            height.unwrap_or_default() * openness
        } else {
            full_height
        };
        ui.allocate_space(egui::vec2(ui.available_width(), visible_height));
        full_height
    } else {
        height.unwrap_or_default()
    };
    ui.ctx()
        .data_mut(|data| data.insert_temp(animation_id, full_height));
}

fn group_body(
    ui: &mut egui::Ui,
    group: &Group<'_>,
    state: &mut DetailsUi,
    p: &Palette,
    lang: Lang,
) {
    let id = group.id();
    ui.add_space(1.0);
    egui::Frame::new()
        .fill(mix(p.surface, p.chip, 0.4))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            detail_field(
                ui,
                lang.t(
                    if group.kind == Kind::Listener || group.kind == Kind::Bound {
                        "connections.local"
                    } else {
                        "connections.remote"
                    },
                ),
                &group.endpoint.to_string(),
                p,
            );
            ui.add_space(8.0);
            let copied = state
                .copied
                .is_some_and(|(key, at)| key == id && at.elapsed() < Duration::from_secs(2));
            let (tools, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::hover());
            ui.painter().text(
                tools.left_center(),
                egui::Align2::LEFT_CENTER,
                lang.t("connections.record_list"),
                egui::FontId::proportional(11.0),
                p.subtext,
            );
            let copy_rect = egui::Rect::from_min_max(
                tools.right_top() - egui::vec2(108.0, 0.0),
                tools.right_bottom(),
            );
            let label = lang.t(if copied {
                "connections.copied"
            } else {
                "connections.copy_group"
            });
            if ui
                .put(
                    copy_rect,
                    egui::Button::new(RichText::new(label).size(11.0).color(p.accent))
                        .fill(p.chip)
                        .corner_radius(6),
                )
                .clicked()
            {
                ui.ctx().copy_text(
                    group
                        .rows
                        .iter()
                        .map(|row| connection_text(row, lang))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                );
                state.copied = Some((id, Instant::now()));
            }
            ui.add_space(6.0);
            record_table(ui, group, p, lang);
        });
}

struct Columns {
    local: f32,
    pid: f32,
    state: f32,
    right: f32,
}
impl Columns {
    fn new(left: f32, width: f32) -> Self {
        let right = left + width;
        Self {
            local: left,
            pid: right - STATE_WIDTH - 62.0,
            state: right - STATE_WIDTH,
            right,
        }
    }
}

fn record_table(ui: &mut egui::Ui, group: &Group<'_>, p: &Palette, lang: Lang) {
    let columns = Columns::new(ui.cursor().left(), ui.available_width());
    let (header, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 23.0), egui::Sense::hover());
    for (x, label) in [
        (columns.local, lang.t("connections.local")),
        (columns.pid, "PID"),
        (columns.state, lang.t("connections.status")),
    ] {
        ui.painter().text(
            egui::pos2(x, header.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(10.0),
            p.unit,
        );
    }
    for row in &group.rows {
        let address = ui.painter().layout(
            row.local.to_string(),
            egui::FontId::monospace(11.0),
            p.text,
            (columns.pid - columns.local - 10.0).max(1.0),
        );
        let height = (address.size().y + 16.0).max(32.0);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), height),
            egui::Sense::hover(),
        );
        ui.painter().line_segment(
            [rect.left_top(), rect.right_top()],
            Stroke::new(0.5, p.border),
        );
        ui.painter()
            .galley(rect.min + egui::vec2(0.0, 8.0), address, p.text);
        let baseline = rect.top() + 8.0;
        ui.painter().text(
            egui::pos2(columns.pid, baseline),
            egui::Align2::LEFT_TOP,
            row.pid.to_string(),
            egui::FontId::monospace(11.0),
            p.subtext,
        );
        let state_rect = egui::Rect::from_min_max(
            egui::pos2(columns.state, baseline),
            egui::pos2(columns.right, baseline + 14.0),
        );
        ellipsis_text(
            ui.painter(),
            state_rect,
            state_label(row.state, lang),
            10.0,
            if row.state == ConnectionState::Established {
                p.accent
            } else {
                p.subtext
            },
            Align::Min,
        );
    }
}

fn detail_field(ui: &mut egui::Ui, label: &str, value: &str, p: &Palette) {
    let width = ui.available_width();
    let label_width = 52.0;
    let value = ui.painter().layout(
        value.into(),
        egui::FontId::monospace(11.0),
        p.text,
        (width - label_width).max(1.0),
    );
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(width, value.size().y.max(16.0)),
        egui::Sense::hover(),
    );
    ui.painter().text(
        rect.min,
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::proportional(10.0),
        p.subtext,
    );
    ui.painter()
        .galley(rect.min + egui::vec2(label_width, 0.0), value, p.text);
}

fn endpoint_title(ui: &mut egui::Ui, rect: egui::Rect, endpoint: &ConnectionEndpoint, p: &Palette) {
    let address = if endpoint.address.is_empty() {
        "*".into()
    } else {
        endpoint.address.clone()
    };
    let font = egui::FontId::monospace(14.0);
    let ipv6 = endpoint.address.contains(':');
    let prefix =
        ui.painter()
            .layout_no_wrap(if ipv6 { "[" } else { "" }.into(), font.clone(), p.text);
    // Only elide the address body. Keep the closing IPv6 bracket and port
    // visible, so a long address cannot be mistaken for a different endpoint.
    let port = ui.painter().layout_no_wrap(
        format!("{}:{}", if ipv6 { "]" } else { "" }, endpoint.port),
        font.clone(),
        p.accent,
    );
    let mut job = egui::text::LayoutJob::simple(address, font, p.text, f32::INFINITY);
    job.wrap.max_width = (rect.width() - prefix.size().x - port.size().x).max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let address = ui.painter().layout_job(job);
    let pos = rect.left_center() - egui::vec2(0.0, address.size().y / 2.0);
    ui.painter().galley(pos, prefix.clone(), p.text);
    let pos = pos + egui::vec2(prefix.size().x, 0.0);
    ui.painter().galley(pos, address.clone(), p.text);
    ui.painter()
        .galley(pos + egui::vec2(address.size().x, 0.0), port, p.accent);
}

fn remote_text(connection: &Connection, lang: Lang) -> String {
    connection
        .remote
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            lang.t(if connection.state == ConnectionState::Listen {
                "connections.no_peer"
            } else {
                "connections.remote_unavailable"
            })
            .to_owned()
        })
}

fn connection_text(connection: &Connection, lang: Lang) -> String {
    format!(
        "{} · {}\n{}: {}\n{}: {}\nPID {}",
        connection.protocol.label(),
        state_label(connection.state, lang),
        lang.t("connections.remote"),
        remote_text(connection, lang),
        lang.t("connections.local"),
        connection.local,
        connection.pid
    )
}

fn state_label(state: ConnectionState, lang: Lang) -> &'static str {
    lang.t(match state {
        ConnectionState::Unknown => "connections.state.unknown",
        ConnectionState::Listen => "connections.state.listen",
        ConnectionState::SynSent => "connections.state.syn_sent",
        ConnectionState::SynReceived => "connections.state.syn_received",
        ConnectionState::Established => "connections.state.established",
        ConnectionState::FinWait1 => "connections.state.fin_wait_1",
        ConnectionState::FinWait2 => "connections.state.fin_wait_2",
        ConnectionState::CloseWait => "connections.state.close_wait",
        ConnectionState::Closing => "connections.state.closing",
        ConnectionState::LastAck => "connections.state.last_ack",
        ConnectionState::TimeWait => "connections.state.time_wait",
        ConnectionState::Closed => "connections.state.closed",
        ConnectionState::Bound => "connections.state.bound",
    })
}
