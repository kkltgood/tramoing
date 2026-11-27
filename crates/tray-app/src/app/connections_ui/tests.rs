use super::*;
use tm_core::{ConnectionProtocol, ConnectionReport};

fn rows(long: bool) -> Vec<Connection> {
    (0..4)
        .map(|i| Connection {
            pid: 4012 + i / 2,
            protocol: if long {
                ConnectionProtocol::Tcp6
            } else {
                ConnectionProtocol::Tcp4
            },
            local: ConnectionEndpoint {
                address: if long {
                    "fe80::1234:5678:9abc:def0%en0"
                } else {
                    "127.0.0.1"
                }
                .into(),
                port: 52148 + i as u16,
            },
            remote: Some(ConnectionEndpoint {
                address: if long {
                    "2001:db8:1234:5678:9abc:def0:1234:5678"
                } else {
                    "127.0.0.1"
                }
                .into(),
                port: if long { 443 } else { 7897 },
            }),
            state: ConnectionState::Established,
        })
        .collect()
}

fn details(rows: Vec<Connection>) -> Details {
    Details {
        updated_at: Instant::now(),
        total: rows.len(),
        established: rows
            .iter()
            .filter(|row| row.state == ConnectionState::Established)
            .count(),
        listening: 0,
        result: Ok(ConnectionReport {
            connections: rows,
            warning: None,
            unverified_processes: 0,
        }),
    }
}

fn render(
    ctx: &egui::Context,
    details: &Details,
    error: Option<&str>,
    lang: Lang,
    dark: bool,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    ctx.style_mut(|style| style.animation_time = 0.0);
    render_at(ctx, details, error, lang, dark, events, None)
}

fn render_at(
    ctx: &egui::Context,
    details: &Details,
    error: Option<&str>,
    lang: Lang,
    dark: bool,
    events: Vec<egui::Event>,
    time: Option<f64>,
) -> egui::FullOutput {
    ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(WINDOW_W, PANEL_H),
            )),
            events,
            time,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    details_body(
                        ui,
                        &AppKey::from("test"),
                        Some(details),
                        error,
                        &theme::palette(dark),
                        lang,
                    );
                });
        },
    )
}

fn texts<'a>(
    output: &'a egui::FullOutput,
    label: &'a str,
) -> impl Iterator<Item = (&'a egui::epaint::TextShape, egui::Rect)> + 'a {
    output
        .shapes
        .iter()
        .filter_map(move |shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some((text, shape.clip_rect))
            }
            _ => None,
        })
}
fn center(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
    texts(output, label)
        .next()
        .unwrap_or_else(|| panic!("missing {label}"))
        .0
        .visual_bounding_rect()
        .center()
}
fn pointer(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(pos),
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}
fn key(key: egui::Key) -> Vec<egui::Event> {
    [true, false]
        .into_iter()
        .map(|pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        })
        .collect()
}

#[test]
fn duplicate_targets_expand_into_aligned_rows_and_copy_every_record() {
    for lang in [Lang::EN, Lang::ZH_CN] {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            let mut details = details(rows(false));
            render(&ctx, &details, None, lang, dark, vec![]);
            let output = render(&ctx, &details, None, lang, dark, vec![]);
            let mut x = None;
            for row in &details.result.as_ref().unwrap().connections {
                let label = row.local.to_string();
                let (text, clip) = texts(&output, &label).next().unwrap();
                let rect = text.visual_bounding_rect();
                assert!(
                    clip.contains_rect(rect),
                    "record must be visible: {rect:?} {clip:?}"
                );
                assert!(!text.galley.elided);
                assert!(x.is_none_or(|x: f32| (x - rect.left()).abs() < 0.1));
                x = Some(rect.left());
            }
            let pid_x = center(&output, "PID").x;
            let local_x = center(&output, lang.t("connections.local")).x;
            let state_x = center(&output, lang.t("connections.status")).x;
            assert!(local_x < pid_x && pid_x < state_x);
            let status = texts(&output, lang.t("connections.state.established"))
                .next()
                .unwrap()
                .0
                .visual_bounding_rect();
            let control = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.rect.contains_rect(status) && rect.rect.height() < 40.0 =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("state belongs to a compact control");
            let header = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.rect.contains_rect(control)
                            && rect.rect.height() < 100.0
                            && rect.rect.contains(center(&output, ":7897")) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("state control belongs to the group header");
            assert!(
                (control.center().y - header.center().y).abs() < 0.1,
                "state control is vertically centered in the group header"
            );
            assert!(
                (status.center().y - control.center().y).abs() < 2.0,
                "state text is vertically centered in its control"
            );
            details.result.as_mut().unwrap().connections[0].state = ConnectionState::CloseWait;
            let output = render(&ctx, &details, None, lang, dark, vec![]);
            assert!(texts(&output, lang.t("connections.mixed_states"))
                .next()
                .is_some());
            let at = center(&output, lang.t("connections.copy_group"));
            render(&ctx, &details, None, lang, dark, pointer(at, true));
            let output = render(&ctx, &details, None, lang, dark, pointer(at, false));
            let expected = details
                .result
                .as_ref()
                .unwrap()
                .connections
                .iter()
                .map(|row| connection_text(row, lang))
                .collect::<Vec<_>>()
                .join("\n\n");
            assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &expected)));
        }
    }
}

#[test]
fn long_ipv6_keeps_port_and_full_details_with_no_column_overlap() {
    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    let details = details(rows(true));
    render(&ctx, &details, None, Lang::EN, false, vec![]);
    let output = render(&ctx, &details, None, Lang::EN, false, vec![]);
    assert!(texts(&output, "]:443").next().is_some());
    let remote = details.result.as_ref().unwrap().connections[0]
        .remote
        .as_ref()
        .unwrap()
        .to_string();
    let (text, clip) = texts(&output, &remote).next().unwrap();
    assert!(!text.galley.elided && clip.contains_rect(text.visual_bounding_rect()));
    let local = details.result.as_ref().unwrap().connections[0]
        .local
        .to_string();
    let local_rect = texts(&output, &local)
        .next()
        .unwrap()
        .0
        .visual_bounding_rect();
    let pid_rect = texts(&output, "4012")
        .next()
        .unwrap()
        .0
        .visual_bounding_rect();
    assert!(local_rect.right() < pid_rect.left());
}

#[test]
fn keyboard_and_pointer_can_collapse_reopen_and_copy_a_group() {
    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    let details = details(rows(false));
    render(&ctx, &details, None, Lang::EN, true, vec![]);
    render(&ctx, &details, None, Lang::EN, true, key(egui::Key::Tab));
    render(&ctx, &details, None, Lang::EN, true, key(egui::Key::Enter));
    let output = render(&ctx, &details, None, Lang::EN, true, vec![]);
    assert!(texts(&output, Lang::EN.t("connections.copy_group"))
        .next()
        .is_none());
    let at = center(&output, ":7897");
    for pressed in [true, false] {
        render(&ctx, &details, None, Lang::EN, true, pointer(at, pressed));
    }
    let output = render(&ctx, &details, None, Lang::EN, true, vec![]);
    assert!(texts(&output, Lang::EN.t("connections.copy_group"))
        .next()
        .is_some());
    render(&ctx, &details, None, Lang::EN, true, key(egui::Key::Tab));
    let output = render(&ctx, &details, None, Lang::EN, true, key(egui::Key::Enter));
    assert!(output
        .platform_output
        .commands
        .iter()
        .any(|command| matches!(command, egui::OutputCommand::CopyText(_))));
}

#[test]
fn stale_and_failed_snapshots_do_not_show_old_rows() {
    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    let mut details = details(rows(false));
    details.updated_at = Instant::now() - Duration::from_secs(10);
    for error in [None, Some("collector offline")] {
        let output = render(&ctx, &details, error, Lang::EN, true, vec![]);
        assert!(texts(&output, ":7897").next().is_none());
        assert!(texts(
            &output,
            Lang::EN.t(if error.is_some() {
                "connections.failed"
            } else {
                "connections.stale"
            })
        )
        .next()
        .is_some());
        details.updated_at = Instant::now();
    }
}

#[test]
fn empty_snapshots_distinguish_absence_from_incomplete_collection() {
    for lang in [Lang::EN, Lang::ZH_CN] {
        for dark in [false, true] {
            for (unverified_processes, warning) in [
                (0, None),
                (25, None),
                (0, Some("TCP/IPv6: Windows error 5")),
                (25, Some("TCP/IPv6: Windows error 5")),
            ] {
                let ctx = egui::Context::default();
                theme::install_cjk(&ctx);
                let mut details = details(vec![]);
                let report = details.result.as_mut().unwrap();
                report.unverified_processes = unverified_processes;
                report.warning = warning.map(str::to_owned);
                render(&ctx, &details, None, lang, dark, vec![]);
                let output = render(&ctx, &details, None, lang, dark, vec![]);
                let incomplete = unverified_processes > 0 || warning.is_some();
                for (label, visible) in [
                    ("connections.empty", !incomplete),
                    ("connections.empty_incomplete", incomplete),
                    ("connections.incomplete", incomplete),
                    ("connections.group_hint", false),
                    ("connections.refresh_short", true),
                ] {
                    assert_eq!(
                        texts(&output, lang.t(label)).next().is_some(),
                        visible,
                        "{label}: unverified={unverified_processes}, warning={warning:?}"
                    );
                }
                if let Some(warning) = warning {
                    assert!(texts(&output, warning).next().is_none());
                }
            }
        }
    }
}

fn render_help(
    ctx: &egui::Context,
    app: &AppKey,
    details: &Details,
    lang: Lang,
    dark: bool,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, egui::Pos2) {
    ctx.style_mut(|style| style.animation_time = 0.0);
    let mut info_center = egui::Pos2::ZERO;
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(WINDOW_W, PANEL_H),
            )),
            events,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                .show(ctx, |ui| {
                    let p = theme::palette(dark);
                    let (header, _) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 42.0),
                        egui::Sense::hover(),
                    );
                    let info = egui::Rect::from_min_size(
                        header.right_top() + egui::vec2(-30.0, 6.0),
                        Vec2::splat(30.0),
                    );
                    info_center = info.center();
                    let response = icon_button(ui, info, "connections-info", &p, paint_info);
                    help::show(&response, header, app, Some(details), None, &p, lang);
                });
        },
    );
    (output, info_center)
}

fn help_close_center(output: &egui::FullOutput, lang: Lang) -> egui::Pos2 {
    let heading = center(output, lang.t("connections.help"));
    output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::LineSegment { points, .. } => {
                let delta = points[1] - points[0];
                let at = points[0].lerp(points[1], 0.5);
                (delta.x.abs() > 1.0
                    && (delta.x.abs() - delta.y.abs()).abs() < 0.1
                    && (at.y - heading.y).abs() < 2.0
                    && at.x > heading.x)
                    .then_some(at)
            }
            _ => None,
        })
        .expect("the help heading has a visible close icon on its right")
}

fn help_background(shape: &egui::Shape, heading: egui::Pos2) -> Option<egui::Rect> {
    match shape {
        egui::Shape::Vec(shapes) => shapes
            .iter()
            .find_map(|shape| help_background(shape, heading)),
        egui::Shape::Rect(rect)
            if rect.rect.contains(heading)
                && rect.rect.height() > 100.0
                && rect.stroke.width > 0.0 =>
        {
            Some(rect.rect)
        }
        _ => None,
    }
}

fn scroll_pointer(at: egui::Pos2, direction: f32) -> Vec<egui::Event> {
    let mut events = vec![egui::Event::PointerMoved(at)];
    // A batch of small trackpad deltas keeps this interaction deterministic
    // without pending wheel inertia moving the button between press and release.
    events.extend((0..20).map(|_| egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, direction * 6.0),
        modifiers: egui::Modifiers::NONE,
    }));
    events
}

#[test]
fn help_popup_keeps_internal_clicks_open_copies_full_path_and_closes_explicitly() {
    for lang in [Lang::EN, Lang::ZH_CN] {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            let app = AppKey::from(r"C:\Program Files\测试应用\verge-mihomo.exe");
            let details = details(rows(false));
            let frame = |events| render_help(&ctx, &app, &details, lang, dark, events);
            let (_, info_at) = frame(vec![]);
            frame(pointer(info_at, true));
            frame(pointer(info_at, false));
            let (output, _) = frame(vec![]);
            assert!(egui::Popup::is_any_open(&ctx));
            let heading_at = center(&output, lang.t("connections.help"));
            for pressed in [true, false] {
                frame(pointer(heading_at, pressed));
            }
            assert!(egui::Popup::is_any_open(&ctx));
            let (output, _) = frame(vec![]);
            let copy_at = center(&output, lang.t("connections.copy_path"));
            frame(pointer(copy_at, true));
            let (output, _) = frame(pointer(copy_at, false));
            assert!(output.platform_output.commands.iter().any(
                |command| matches!(command, egui::OutputCommand::CopyText(text) if text == app.as_str())
            ));
            assert!(egui::Popup::is_any_open(&ctx));
            let (output, _) = frame(vec![]);
            assert!(texts(&output, lang.t("connections.copied"))
                .next()
                .is_some());
            let close_at = help_close_center(&output, lang);
            for pressed in [true, false] {
                frame(pointer(close_at, pressed));
            }
            assert!(!egui::Popup::is_any_open(&ctx));
            let (output, _) = frame(vec![]);
            assert!(texts(&output, lang.t("connections.help")).next().is_none());
            for pressed in [true, false] {
                frame(pointer(info_at, pressed));
            }
            assert!(egui::Popup::is_any_open(&ctx));
            frame(key(egui::Key::Escape));
            assert!(!egui::Popup::is_any_open(&ctx));
        }
    }
}

#[test]
fn help_popup_wraps_long_paths_within_the_panel_and_keeps_copy_reachable() {
    for lang in [Lang::EN, Lang::ZH_CN] {
        for dark in [false, true] {
            let ctx = egui::Context::default();
            theme::install_cjk(&ctx);
            let path = format!(
                "C:\\Program Files\\{}verge-mihomo.exe",
                "很长的应用程序安装目录-without-spaces\\".repeat(24)
            );
            let app = AppKey::new(path);
            let details = details(rows(false));
            let frame = |events| render_help(&ctx, &app, &details, lang, dark, events);
            let (_, info_at) = frame(vec![]);
            for pressed in [true, false] {
                frame(pointer(info_at, pressed));
            }
            frame(vec![]);
            let (mut output, _) = frame(vec![]);
            let heading_at = center(&output, lang.t("connections.help"));
            let panel = output
                .shapes
                .iter()
                .find_map(|shape| help_background(&shape.shape, heading_at))
                .expect("help popup background");
            let viewport =
                egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(WINDOW_W, PANEL_H));
            assert!(
                viewport.contains_rect(panel),
                "popup is outside the window: {panel:?}"
            );
            let (path_text, clip) = texts(&output, app.as_str()).next().unwrap();
            let path_bounds = path_text.visual_bounding_rect();
            assert!(
                !path_text.galley.elided,
                "the full path must remain available"
            );
            assert!(path_text.galley.rows.len() > 1);
            assert!(path_bounds.left() >= clip.left() - 0.5);
            assert!(path_bounds.right() <= clip.right() + 0.5);
            assert!(
                path_bounds.bottom() > clip.bottom(),
                "the fixture must require scrolling to read the end of the path"
            );

            let scroll_at = panel.center();
            let mut reached_end = false;
            for _ in 0..24 {
                frame(scroll_pointer(scroll_at, -1.0));
                output = frame(vec![]).0;
                let (path_text, clip) = texts(&output, app.as_str()).next().unwrap();
                let tail = path_text
                    .galley
                    .rows
                    .last()
                    .unwrap()
                    .rect()
                    .translate(path_text.pos.to_vec2());
                if clip.contains_rect(tail) {
                    reached_end = true;
                    break;
                }
            }
            assert!(reached_end, "the full path must be reachable by scrolling");
            assert!(egui::Popup::is_any_open(&ctx));
            assert_eq!(center(&output, lang.t("connections.help")), heading_at);

            // The path can be much longer than the body. Scroll back to the
            // actual visible copy control before verifying clipboard output.
            let mut copy_visible = false;
            for _ in 0..24 {
                frame(scroll_pointer(scroll_at, 1.0));
                output = frame(vec![]).0;
                if texts(&output, lang.t("connections.copy_path"))
                    .any(|(text, clip)| clip.contains_rect(text.visual_bounding_rect()))
                {
                    copy_visible = true;
                    break;
                }
            }
            assert!(
                copy_visible,
                "the copy button must be reachable after scrolling"
            );
            let (copy_text, clip) = texts(&output, lang.t("connections.copy_path"))
                .next()
                .unwrap();
            assert!(clip.contains_rect(copy_text.visual_bounding_rect()));
            let copy_at = copy_text.visual_bounding_rect().center();
            frame(pointer(copy_at, true));
            let (output, _) = frame(pointer(copy_at, false));
            assert!(output.platform_output.commands.iter().any(
                |command| matches!(command, egui::OutputCommand::CopyText(text) if text == app.as_str())
            ));
        }
    }
}

fn copied(output: &egui::FullOutput) -> bool {
    output
        .platform_output
        .commands
        .iter()
        .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
}

struct AnimatedGroup {
    ctx: egui::Context,
    rows: Vec<Connection>,
    state: DetailsUi,
    time: f64,
}

impl AnimatedGroup {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let rows = rows(false);
        let expanded = Some(groups::collect(&rows)[0].id());
        Self {
            ctx,
            rows,
            state: DetailsUi {
                expanded,
                initialized: true,
                ..Default::default()
            },
            time: 0.0,
        }
    }

    fn set_open(&mut self, open: bool) {
        self.state.expanded = open.then(|| groups::collect(&self.rows)[0].id());
    }

    fn frame(&mut self, elapsed: f64, events: Vec<egui::Event>) -> (egui::FullOutput, f32) {
        self.time += elapsed;
        let mut height = 0.0;
        let output = self.ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(WINDOW_W, PANEL_H),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.inner_margin(PANEL_PADDING))
                    .show(ctx, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let top = ui.cursor().top();
                        let groups = groups::collect(&self.rows);
                        ui.push_id("animated-group", |ui| {
                            group_row(
                                ui,
                                &groups[0],
                                &mut self.state,
                                &theme::palette(true),
                                Lang::EN,
                            );
                        });
                        height = ui.cursor().top() - top;
                        ui.label("following group");
                    });
            },
        );
        (output, height)
    }

    fn settle(&mut self) -> (egui::FullOutput, f32) {
        let mut output = self.frame(0.02, vec![]);
        for _ in 1..20 {
            output = self.frame(0.02, vec![]);
        }
        output
    }
}

#[test]
fn initial_single_group_animates_and_snapshot_updates_keep_it_open() {
    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    let mut details = details(rows(false));
    let last_local = details.result.as_ref().unwrap().connections[3]
        .local
        .to_string();
    let initial = render_at(&ctx, &details, None, Lang::EN, true, vec![], Some(0.0));
    assert!(
        texts(&initial, &last_local)
            .all(|(text, clip)| !clip.contains_rect(text.visual_bounding_rect())),
        "the first single-group snapshot should reveal instead of appearing fully open"
    );
    let mut output = initial;
    for frame in 1..=20 {
        output = render_at(
            &ctx,
            &details,
            None,
            Lang::EN,
            true,
            vec![],
            Some(f64::from(frame) * 0.02),
        );
    }
    let (text, clip) = texts(&output, &last_local).next().unwrap();
    assert!(clip.contains_rect(text.visual_bounding_rect()));
    let before = text.visual_bounding_rect();
    details.result.as_mut().unwrap().connections[0].state = ConnectionState::CloseWait;
    details.result.as_mut().unwrap().connections[0].pid += 1;
    details.updated_at = Instant::now();
    let updated = render_at(&ctx, &details, None, Lang::EN, true, vec![], Some(0.42));
    let (text, clip) = texts(&updated, &last_local).next().unwrap();
    assert!(clip.contains_rect(text.visual_bounding_rect()));
    assert_eq!(text.visual_bounding_rect(), before);
    assert!(texts(&updated, Lang::EN.t("connections.mixed_states"))
        .next()
        .is_some());
}

#[test]
fn reveal_animates_layout_and_clipping_in_both_directions_without_reversal_jumps() {
    let mut group = AnimatedGroup::new();
    let (_, first_height) = group.frame(0.02, vec![]);
    let (opening, opening_height) = group.frame(0.02, vec![]);
    assert!(opening_height > first_height);
    let last_local = group.rows[3].local.to_string();
    let (text, clip) = texts(&opening, &last_local).next().unwrap();
    assert!(!clip.contains_rect(text.visual_bounding_rect()));
    assert!(clip.bottom() <= center(&opening, "following group").y);
    let (opened, full_height) = group.settle();
    assert!(full_height > opening_height + 20.0);
    let (text, clip) = texts(&opened, &last_local).next().unwrap();
    assert!(clip.contains_rect(text.visual_bounding_rect()));

    group.set_open(false);
    let (_, closing_start) = group.frame(0.02, vec![]);
    assert!(closing_start < full_height && closing_start > first_height);
    let (_, closing_middle) = group.frame(0.02, vec![]);
    assert!(closing_middle < closing_start);
    let (closing, before_reversal) = group.frame(0.02, vec![]);
    let (text, clip) = texts(&closing, &last_local).next().unwrap();
    assert!(!clip.contains_rect(text.visual_bounding_rect()));
    assert!(
        clip.bottom() <= center(&closing, "following group").y,
        "clipped content must not paint over the following row"
    );

    group.set_open(true);
    let (_, after_reversal) = group.frame(0.001, vec![]);
    assert!(after_reversal >= before_reversal);
    assert!(
        after_reversal - before_reversal < 4.0,
        "reversing after 1 ms must continue from the visible height: {before_reversal} -> {after_reversal}"
    );
    group.set_open(false);
    let (_, reversed_again) = group.frame(0.001, vec![]);
    assert!(reversed_again <= after_reversal);
    assert!((reversed_again - after_reversal).abs() < 4.0);
    let (closed, closed_height) = group.settle();
    assert!((closed_height - first_height).abs() < 0.1);
    assert!(texts(&closed, Lang::EN.t("connections.copy_group"))
        .next()
        .is_none());
    group.set_open(true);
    let (_, reopened_height) = group.settle();
    assert!((reopened_height - full_height).abs() < 0.1);
}

#[test]
fn changing_record_count_during_reveal_keeps_clip_and_following_row_aligned() {
    let mut group = AnimatedGroup::new();
    group.settle();
    group.set_open(false);
    group.frame(0.02, vec![]);
    group.frame(0.02, vec![]);

    for count in [1, 8] {
        group.rows = (0..count)
            .map(|i| {
                let mut row = rows(false)[0].clone();
                row.local.port += i;
                row
            })
            .collect();
        for _ in 0..2 {
            let (output, _) = group.frame(0.01, vec![]);
            let (_, clip) = texts(&output, "127.0.0.1:7897").next().unwrap();
            let next_top = texts(&output, "following group")
                .next()
                .unwrap()
                .0
                .visual_bounding_rect()
                .top();
            assert!(
                clip.bottom() <= next_top + 0.1,
                "a snapshot with {count} records must not paint over the next group"
            );
            assert!(
                next_top - clip.bottom() < 4.0,
                "layout must follow the visible body when the snapshot grows"
            );
        }
    }
}

#[test]
fn clipped_copy_button_cannot_receive_pointer_or_keyboard_input() {
    let mut group = AnimatedGroup::new();
    let (opened, _) = group.settle();
    let copy_at = center(&opened, Lang::EN.t("connections.copy_group"));
    group.frame(0.02, key(egui::Key::Tab));
    let header_focus = group.ctx.memory(|memory| memory.focused()).unwrap();
    group.frame(0.02, key(egui::Key::Tab));
    let copy_focus = group.ctx.memory(|memory| memory.focused()).unwrap();
    assert_ne!(copy_focus, header_focus);

    group.set_open(false);
    let mut hidden = false;
    for _ in 0..20 {
        let (closing, _) = group.frame(0.01, vec![]);
        if texts(&closing, Lang::EN.t("connections.copy_group"))
            .any(|(_, clip)| !clip.contains(copy_at))
        {
            hidden = true;
            break;
        }
    }
    assert!(hidden, "copy button should be clipped during closing");
    for pressed in [true, false] {
        let (output, _) = group.frame(0.001, pointer(copy_at, pressed));
        assert!(!copied(&output));
    }
    group
        .ctx
        .memory_mut(|memory| memory.request_focus(copy_focus));
    let (output, _) = group.frame(0.001, key(egui::Key::Enter));
    assert!(!copied(&output));
    assert_ne!(
        group.ctx.memory(|memory| memory.focused()),
        Some(copy_focus)
    );
    let (closed, _) = group.settle();
    assert!(!copied(&closed));
}

#[test]
fn expanding_scrollable_ipv6_details_keeps_header_columns_stationary() {
    // A real nonempty, expanded list can fit without needing a scrollbar.
    for dark in [false, true] {
        let ctx = egui::Context::default();
        theme::install_cjk(&ctx);
        let short = details(vec![rows(true)[0].clone()]);
        render(&ctx, &short, None, Lang::EN, dark, vec![]);
        let output = render(&ctx, &short, None, Lang::EN, dark, vec![]);
        assert!(texts(&output, Lang::EN.t("connections.copy_group"))
            .next()
            .is_some());
        assert!(
            !has_list_scrollbar(&output),
            "a short nonempty list must not paint a scrollbar"
        );
        let card = connection_card(&output);
        let left_margin = card.left();
        let right_margin = WINDOW_W - card.right();
        assert!(
            (left_margin - right_margin).abs() < 0.1,
            "a short list must have symmetric margins: left={left_margin}, right={right_margin}"
        );
        assert!((left_margin - f32::from(PANEL_PADDING)).abs() < 0.1);
    }

    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    let mut records = rows(true);
    let extra = records
        .iter()
        .cloned()
        .map(|mut row| {
            row.local.port += 10;
            row
        })
        .collect::<Vec<_>>();
    records.extend(extra);
    let details = details(records);
    let initial = render_at(&ctx, &details, None, Lang::EN, false, vec![], Some(0.0));
    let initial_port = center(&initial, "]:443");
    let initial_status = center(&initial, Lang::EN.t("connections.state.established"));
    assert!(!has_list_scrollbar(&initial));
    let mut final_output = initial;
    for frame in 1..=20 {
        let output = render_at(
            &ctx,
            &details,
            None,
            Lang::EN,
            false,
            vec![],
            Some(f64::from(frame) * 0.02),
        );
        assert_eq!(center(&output, "]:443"), initial_port);
        assert_eq!(
            center(&output, Lang::EN.t("connections.state.established")),
            initial_status
        );
        final_output = output;
    }
    let last_local = details
        .result
        .as_ref()
        .unwrap()
        .connections
        .last()
        .unwrap()
        .local
        .to_string();
    let (text, clip) = texts(&final_output, &last_local).next().unwrap();
    assert!(
        !clip.contains_rect(text.visual_bounding_rect()),
        "fixture must overflow vertically to exercise scrollbar space"
    );
    assert!(
        has_list_scrollbar(&final_output),
        "expanded overflowing records must have a visible scrollbar"
    );
    let card = connection_card(&final_output);
    assert!(
        list_scrollbars(&final_output).all(|bar| bar.left() > card.right()),
        "the scrollbar must stay in the right margin without covering the connection card"
    );

    let columns = ["connections.local", "connections.status"]
        .map(|label| center(&final_output, Lang::EN.t(label)).x);
    let pid_x = center(&final_output, "PID").x;
    let scroll_at = egui::pos2(WINDOW_W / 2.0, PANEL_H / 2.0);
    let mut time = 0.4;
    let mut frame = |events| {
        time += 0.02;
        render_at(&ctx, &details, None, Lang::EN, false, events, Some(time))
    };
    let mut reached_last = false;
    for _ in 0..12 {
        frame(scroll_pointer(scroll_at, -1.0));
        let output = frame(vec![]);
        assert_eq!(center(&output, "]:443").x, initial_port.x);
        assert_eq!(
            ["connections.local", "connections.status"].map(|label| center(
                &output,
                Lang::EN.t(label)
            )
            .x),
            columns
        );
        assert_eq!(center(&output, "PID").x, pid_x);
        let (text, clip) = texts(&output, &last_local).next().unwrap();
        if clip.contains_rect(text.visual_bounding_rect()) {
            reached_last = true;
            break;
        }
    }
    assert!(
        reached_last,
        "the wheel must reveal the final connection record"
    );

    // Return to the group header and collapse it through the actual hit target.
    for _ in 0..12 {
        frame(scroll_pointer(scroll_at, 1.0));
        final_output = frame(vec![]);
        if (center(&final_output, "]:443").y - initial_port.y).abs() < 0.1 {
            break;
        }
    }
    let collapse_at = center(&final_output, "]:443");
    assert_eq!(collapse_at, initial_port);
    for pressed in [true, false] {
        frame(pointer(collapse_at, pressed));
    }
    for _ in 0..20 {
        final_output = frame(vec![]);
        assert_eq!(center(&final_output, "]:443").x, initial_port.x);
        assert_eq!(
            center(&final_output, Lang::EN.t("connections.state.established")).x,
            initial_status.x
        );
    }
    assert!(texts(&final_output, Lang::EN.t("connections.copy_group"))
        .next()
        .is_none());
    assert!(
        !has_list_scrollbar(&final_output),
        "the scrollbar must disappear when collapsed content fits again"
    );
}

fn connection_card(output: &egui::FullOutput) -> egui::Rect {
    let port = center(output, "]:443");
    output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Rect(rect)
                if rect.rect.contains(port) && (rect.rect.height() - 58.0).abs() < 0.1 =>
            {
                Some(rect.rect)
            }
            _ => None,
        })
        .expect("connection card surrounding its endpoint")
}

fn list_scrollbars(output: &egui::FullOutput) -> impl Iterator<Item = egui::Rect> + '_ {
    output.shapes.iter().filter_map(|shape| match &shape.shape {
        egui::Shape::Rect(rect)
            if rect.fill.a() > 0
                && rect.rect.left() > WINDOW_W - 50.0
                && rect.rect.width() > 0.0
                && rect.rect.width() <= 8.0
                && rect.rect.height() > 24.0
                && shape.clip_rect.intersects(rect.rect) =>
        {
            Some(rect.rect)
        }
        _ => None,
    })
}

fn has_list_scrollbar(output: &egui::FullOutput) -> bool {
    list_scrollbars(output).next().is_some()
}
