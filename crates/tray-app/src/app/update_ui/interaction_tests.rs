use super::{egui, tests::label_position, theme, Choice, Lang, UpdatePrompt};

fn text_center(output: &egui::FullOutput, text: &str) -> Option<egui::Pos2> {
    output
        .shapes
        .iter()
        .find_map(|shape| label_position(&shape.shape, text))
}

fn keystroke(key: egui::Key) -> Vec<egui::Event> {
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

fn compact_keyboard_choices(
    prompt: &UpdatePrompt,
    lang: Lang,
    can_install: bool,
    size: egui::Vec2,
    expand_details: bool,
) -> (Vec<Choice>, egui::FullOutput) {
    let ctx = egui::Context::default();
    theme::install_cjk(&ctx);
    ctx.style_mut(|style| style.animation_time = 0.0);
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
    let frame = |events| {
        let mut choice = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(viewport),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let background = ui.button("Background action");
                    assert!(!background.clicked() && !background.has_focus());
                });
                choice = prompt.show(ctx, &theme::palette(true), lang, can_install);
            },
        );
        (choice, output)
    };

    frame(vec![]);
    let (_, initial) = frame(vec![]);
    if let (UpdatePrompt::Download { error, .. }, true) = (prompt, expand_details) {
        assert!(text_center(&initial, error).is_none());
        let position = text_center(&initial, lang.t("update.details")).expect("details control");
        for pressed in [true, false] {
            let (choice, _) = frame(vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            assert_eq!(choice, None);
        }
        let (_, expanded) = frame(vec![]);
        assert!(text_center(&expanded, error).is_some());
    }

    let mut visited = std::collections::HashSet::new();
    let mut choices = Vec::new();
    for _ in 0..24 {
        let (choice, output) = frame(keystroke(egui::Key::Tab));
        assert_eq!(choice, None);
        let Some(focused) = ctx.memory(|memory| memory.focused()) else {
            continue;
        };
        if !visited.insert(focused) {
            break;
        }
        let response = ctx.read_response(focused).expect("focused widget");
        if text_center(&output, lang.t("update.details"))
            .is_some_and(|position| response.rect.contains(position))
        {
            continue;
        }
        if let (Some(choice), _) = frame(keystroke(egui::Key::Enter)) {
            assert!(
                viewport.contains_rect(response.rect)
                    && response.interact_rect.contains_rect(response.rect),
                "{choice:?} is clipped: {:?}, active {:?}, viewport {viewport:?}",
                response.rect,
                response.interact_rect,
            );
            choices.push(choice);
        }
    }
    (choices, initial)
}

#[test]
fn keyboard_reaches_the_enabled_choices_in_a_compact_panel_with_long_errors() {
    let restart = UpdatePrompt::restart("2.1.0-rc.1".to_owned());
    let failed = UpdatePrompt::Download {
        error: "The update server closed the connection before completing the download.\n"
            .repeat(15),
        browser_error: Some("No default browser is configured for HTTPS links.".to_owned()),
    };
    for lang in [Lang::EN, Lang::ZH_CN] {
        for (prompt, can_install, expected) in [
            (&restart, true, &[Choice::Restart, Choice::Later][..]),
            (&restart, false, &[Choice::Later][..]),
            (
                &failed,
                true,
                &[Choice::Manual, Choice::Resume, Choice::Later][..],
            ),
        ] {
            let (reached, _) =
                compact_keyboard_choices(prompt, lang, can_install, egui::vec2(360.0, 426.0), true);
            assert!(
                expected.len() == reached.len()
                    && expected.iter().all(|choice| reached.contains(choice)),
                "{}: expected {expected:?}, reached {reached:?}",
                lang.code()
            );
        }
    }
}

#[test]
fn narrow_english_prompts_keep_the_status_and_stacked_actions_visible() {
    let lang = Lang::EN;
    let size = egui::vec2(280.0, 330.0);
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
    for (prompt, status, end, primary, secondary, expected) in [
        (
            UpdatePrompt::restart("2.1.0".into()),
            "update.verified",
            "v2.1.0",
            "update.restart_now",
            "update.next_launch",
            &[Choice::Restart, Choice::Later][..],
        ),
        (
            UpdatePrompt::download_failed("offline".into()),
            "update.retry_status",
            lang.t("update.later"),
            "update.manual",
            "update.resume",
            &[Choice::Manual, Choice::Later, Choice::Resume][..],
        ),
    ] {
        let (choices, output) = compact_keyboard_choices(&prompt, lang, true, size, false);
        assert_eq!(choices.len(), expected.len());
        assert!(expected.iter().all(|choice| choices.contains(choice)));
        let bounds = |label| {
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == label => {
                        let rect = text.visual_bounding_rect();
                        assert!(
                            !text.galley.elided
                                && viewport.contains_rect(rect)
                                && shape.clip_rect.contains_rect(rect),
                            "{label}: {rect:?}, clip {:?}",
                            shape.clip_rect
                        );
                        Some(rect)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing visible label: {label}"))
        };
        let status_rect = bounds(lang.t(status));
        let end_rect = bounds(end);
        assert!(
            !status_rect.intersects(end_rect),
            "{status} {status_rect:?} overlaps {end} {end_rect:?}"
        );
        assert!(!bounds(lang.t(primary))
            .y_range()
            .intersects(bounds(lang.t(secondary)).y_range()));
    }
}
