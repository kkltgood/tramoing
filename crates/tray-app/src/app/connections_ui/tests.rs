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
}
