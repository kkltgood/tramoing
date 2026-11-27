use super::*;
use crate::badge::{tests::order, Performance, Speed};

const DPIS: [i32; 5] = [96, 120, 144, 168, 192];

fn content(light: bool, colored: bool) -> Content {
    Content {
        speed: Some(Speed {
            down: "999M".into(),
            up: "1.5G".into(),
            colored: !colored,
        }),
        performance: Some(Performance {
            temperatures: None,
            cpu: "100%".into(),
            gpu: " --%".into(),
            colored,
        }),
        light,
        colors: Default::default(),
        order: order(false),
    }
}

fn rect_key(rect: RECT) -> (i32, i32, i32, i32) {
    (rect.left, rect.top, rect.right, rect.bottom)
}

fn area_pixels(pixels: &[u32], width: i32, area: RECT) -> Vec<u32> {
    (area.top..area.bottom)
        .flat_map(|y| {
            pixels[(y * width + area.left) as usize..(y * width + area.right) as usize]
                .iter()
                .copied()
        })
        .collect()
}

fn assert_runs_fit(content: &Content, dpi: i32, width: i32, height: i32) {
    let layout = layout(content, width, height, dpi);
    let surface = unsafe { Surface::new(1, 1).unwrap() };
    let fonts = unsafe { Fonts::new(dpi).unwrap() };
    for run in runs(content, &layout, dpi) {
        if run.text.is_empty() {
            continue;
        }
        let old = unsafe { SelectObject(surface.dc, fonts.get(run.face)) };
        assert!(!old.is_null() && old as isize != -1);
        let text = wide(run.text);
        let mut size = SIZE { cx: 0, cy: 0 };
        let measured = unsafe {
            GetTextExtentPoint32W(
                surface.dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut size,
            )
        };
        unsafe { SelectObject(surface.dc, old) };
        assert_ne!(measured, 0);
        assert!(
            size.cx <= run.area.right - run.area.left && size.cy <= run.area.bottom - run.area.top,
            "{} at {dpi} DPI needs {}x{}px, available {}x{}px in {width}x{height}",
            run.text,
            size.cx,
            size.cy,
            run.area.right - run.area.left,
            run.area.bottom - run.area.top,
        );
    }
}

fn assert_performance_is_compact(content: &Content, layout: &Layout, dpi: i32) {
    let perf = layout.rect(Group::Performance).unwrap();
    let text = runs(content, layout, dpi);
    let row_size = if content.performance.as_ref().unwrap().temperatures.is_some() {
        4
    } else {
        3
    };
    let start = if content.speed.is_some() { 4 } else { 0 };
    let text = &text[start..];
    let surface = unsafe { Surface::new(1, 1).unwrap() };
    let fonts = unsafe { Fonts::new(dpi).unwrap() };
    for column in 0..row_size {
        let first = &text[column];
        let second = &text[column + row_size];
        assert_eq!(first.area.left, second.area.left);
        assert_eq!(first.area.right, second.area.right);
        assert_eq!(first.right, column == 1);
        assert_eq!(second.right, column == 1);
        let mut maximum = 0;
        for run in [first, second] {
            let old = unsafe { SelectObject(surface.dc, fonts.get(run.face)) };
            let string = wide(run.text);
            let mut size = SIZE { cx: 0, cy: 0 };
            let measured = unsafe {
                GetTextExtentPoint32W(
                    surface.dc,
                    string.as_ptr(),
                    (string.len() - 1) as i32,
                    &mut size,
                )
            };
            unsafe { SelectObject(surface.dc, old) };
            assert_ne!(measured, 0);
            maximum = maximum.max(size.cx);
        }
        assert_eq!(
            first.area.right - first.area.left,
            maximum,
            "column {column} at {dpi} DPI reserves unused text width"
        );
    }
    for row in text.chunks(row_size) {
        assert_eq!(row[0].area.left, perf.left + dpi / 96);
        assert_eq!(row[1].area.left - row[0].area.right, 3 * dpi / 96);
        assert_eq!(row[2].area.left - row[1].area.right, dpi / 96);
        if row_size == 4 {
            let temperature_gap = row[3].area.left - row[2].area.right;
            assert_eq!(temperature_gap, 6 * dpi / 96 + 2 * (dpi / 96));
            if layout
                .rect(Group::Speed)
                .is_some_and(|speed| speed.left < perf.left)
            {
                let all = runs(content, layout, dpi);
                assert_eq!(temperature_gap, row[0].area.left - all[1].area.right);
                assert_eq!(temperature_gap, row[0].area.left - all[3].area.right);
            }
        }
        assert_eq!(perf.right - row.last().unwrap().area.right, dpi / 96);
    }
}

#[test]
fn columns_fit_valid_values_with_their_actual_fonts_at_each_dpi() {
    for dpi in DPIS {
        let mut content = content(false, true);
        for (down, up, cpu, gpu, temperatures) in [
            ("999M", "888K", "100%", " 99%", ("150°C", "148°C")),
            ("9.9G", "8.8T", " 88%", " --%", (" --°C", " 99°C")),
            ("100B", "  0B", "  0%", "  9%", ("  1°C", " 88°C")),
            ("1.5G", "875B", " 11%", " 44%", ("111°C", "100°C")),
        ] {
            let speed = content.speed.as_mut().unwrap();
            speed.down = down.into();
            speed.up = up.into();
            let performance = content.performance.as_mut().unwrap();
            performance.cpu = cpu.into();
            performance.gpu = gpu.into();
            performance.temperatures = Some((temperatures.0.into(), temperatures.1.into()));
            assert_runs_fit(&content, dpi, width_px(&content, dpi), 36 * dpi / 96);
            assert_runs_fit(
                &content,
                dpi,
                minimum_width_px(&content, dpi),
                72 * dpi / 96,
            );
            content.performance.as_mut().unwrap().temperatures = None;
            assert_runs_fit(&content, dpi, width_px(&content, dpi), 36 * dpi / 96);
            assert_runs_fit(&content, dpi, 80 * dpi / 96, 72 * dpi / 96);
        }
        // Cover every supported temperature, including all digit shapes and the boundaries.
        for temperature in 0..=150 {
            content.performance.as_mut().unwrap().temperatures = Some((
                format!("{temperature:3}°C"),
                format!("{:3}°C", 150 - temperature),
            ));
            assert_runs_fit(&content, dpi, width_px(&content, dpi), 36 * dpi / 96);
            assert_runs_fit(
                &content,
                dpi,
                minimum_width_px(&content, dpi),
                72 * dpi / 96,
            );
        }
    }
}

#[test]
fn performance_columns_follow_current_values_and_keep_equal_width_values_stable() {
    for dpi in DPIS {
        for performance_first in [false, true] {
            let mut content = content(false, true);
            content.order = order(performance_first);
            for temperatures in [false, true] {
                let mut baseline = None;
                for (cpu, gpu, cpu_temperature, gpu_temperature) in [
                    ("  0%", "  9%", " 48°C", " 41°C"),
                    ("  3%", "  2%", " 51°C", " 69°C"),
                    ("  8%", "  4%", " 99°C", " 10°C"),
                ] {
                    let performance = content.performance.as_mut().unwrap();
                    performance.cpu = cpu.into();
                    performance.gpu = gpu.into();
                    performance.temperatures =
                        temperatures.then(|| (cpu_temperature.into(), gpu_temperature.into()));
                    let width = width_px(&content, dpi);
                    let layout = layout(&content, width, 36 * dpi / 96, dpi);
                    assert_performance_is_compact(&content, &layout, dpi);
                    let anchors = runs(&content, &layout, dpi)
                        .into_iter()
                        .map(|run| (rect_key(run.area), run.right))
                        .collect::<Vec<_>>();
                    let current = (width, anchors);
                    if let Some(expected) = &baseline {
                        assert_eq!(&current, expected);
                    } else {
                        baseline = Some(current);
                    }
                }
                let compact = width_px(&content, dpi);
                let performance = content.performance.as_mut().unwrap();
                performance.cpu = "100%".into();
                performance.gpu = " 99%".into();
                if temperatures {
                    performance.temperatures = Some(("150°C".into(), " --°C".into()));
                }
                let expanded = width_px(&content, dpi);
                assert!(expanded > compact);
                assert_performance_is_compact(
                    &content,
                    &layout(&content, expanded, 36 * dpi / 96, dpi),
                    dpi,
                );
            }
        }
    }
}

#[test]
fn configured_colors_reach_entire_rows_and_mono_ignores_custom_colors() {
    use tm_core::color::DisplayColors;
    for dpi in DPIS {
        for light in [false, true] {
            let mut content = content(light, true);
            content.speed.as_mut().unwrap().colored = true;
            content.performance.as_mut().unwrap().temperatures =
                Some((" 65°C".into(), " 50°C".into()));
            content.colors = DisplayColors {
                download: Some(Rgb(255, 128, 0)),
                upload: Some(Rgb(0, 192, 255)),
                cpu: Some(Rgb(255, 80, 80)),
                gpu: Some(Rgb(160, 240, 32)),
            };
            let width = width_px(&content, dpi);
            let height = 48 * dpi / 96;
            let layout = layout(&content, width, height, dpi);
            let mut surface = unsafe { rasterize(&content, width, height, dpi).unwrap() };
            let pixels = surface.pixels_mut();
            for (index, run) in runs(&content, &layout, dpi).into_iter().enumerate() {
                let expected = match index {
                    0..=1 => content.colors.download,
                    2..=3 => content.colors.upload,
                    4..=7 => content.colors.cpu,
                    _ => content.colors.gpu,
                }
                .unwrap();
                assert_eq!(run.color, expected, "{} lost its row color", run.text);
                let ink: Vec<_> = area_pixels(pixels, width, run.area)
                    .into_iter()
                    .filter(|pixel| pixel >> 24 > HIT_ALPHA)
                    .collect();
                assert!(!ink.is_empty(), "{} has no ink at {dpi} DPI", run.text);
                assert!(ink
                    .iter()
                    .all(|pixel| { *pixel == premultiply((pixel >> 24) as u8, run.color) }));
            }
            for color in [
                content.colors.download,
                content.colors.upload,
                content.colors.cpu,
                content.colors.gpu,
            ] {
                assert!(pixels.contains(&premultiply(255, color.unwrap())));
            }
            content.speed.as_mut().unwrap().colored = false;
            content.performance.as_mut().unwrap().colored = false;
            let mut mono = unsafe { rasterize(&content, width, height, dpi).unwrap() };
            let pixels = mono.pixels_mut();
            let primary = premultiply(255, foreground(light));
            assert!(pixels.contains(&primary));
            assert!(pixels
                .iter()
                .filter(|pixel| *pixel >> 24 == 255)
                .all(|pixel| *pixel == primary));
        }
    }
}

#[test]
fn layout_supports_independent_switches_order_and_vertical_taskbars() {
    for dpi in DPIS {
        for performance_first in [false, true] {
            for temperatures in [false, true] {
                let mut content = content(false, true);
                content.order = order(performance_first);
                if temperatures {
                    content.performance.as_mut().unwrap().temperatures =
                        Some(("150°C".into(), " --°C".into()));
                }
                let mut performance_only = content.clone();
                performance_only.speed = None;
                let performance_width = width_px(&performance_only, dpi);
                let width = width_px(&content, dpi);
                let horizontal = layout(&content, width, 48 * dpi / 96, dpi);
                assert!(!horizontal.stacked());
                let speed = horizontal.rect(Group::Speed).unwrap();
                let perf = horizontal.rect(Group::Performance).unwrap();
                let (first, second) = if performance_first {
                    (perf, speed)
                } else {
                    (speed, perf)
                };
                assert_eq!(first.left, 0);
                assert_eq!(second.left - first.right, GROUP_GAP_DIP * dpi / 96);
                assert_eq!(second.right, width);
                assert_eq!(speed.right - speed.left, speed_columns(&content, dpi).0);
                assert_eq!(perf.right - perf.left, performance_width);
                assert_eq!(horizontal.group_at(speed.left, 0), Some(Group::Speed));
                assert_eq!(horizontal.group_at(perf.left, 0), Some(Group::Performance));
                assert_eq!(horizontal.group_at(first.right, 0), None);
                assert_eq!(horizontal.group_at(width, 0), None);
                assert_performance_is_compact(&content, &horizontal, dpi);
                // Extra container space must remain after the groups, not between them.
                let expanded = layout(&content, width + 120 * dpi / 96, 48 * dpi / 96, dpi);
                assert!(!expanded.stacked());
                for group in [Group::Speed, Group::Performance] {
                    assert_eq!(
                        rect_key(expanded.rect(group).unwrap()),
                        rect_key(horizontal.rect(group).unwrap())
                    );
                }
                assert_eq!(expanded.group_at(width, 0), None);
                let vertical_width = minimum_width_px(&content, dpi).max(80 * dpi / 96);
                let vertical = layout(&content, vertical_width, 72 * dpi / 96, dpi);
                assert!(vertical.stacked());
                let speed = vertical.rect(Group::Speed).unwrap();
                let perf = vertical.rect(Group::Performance).unwrap();
                let (first, second) = if performance_first {
                    (perf, speed)
                } else {
                    (speed, perf)
                };
                assert_eq!(first.bottom, second.top);
                assert_eq!(vertical.group_at(0, speed.top), Some(Group::Speed));
                assert_eq!(vertical.group_at(0, perf.top), Some(Group::Performance));
                assert_runs_fit(&content, dpi, vertical_width, 72 * dpi / 96);

                let mut speed_only = content.clone();
                speed_only.performance = None;
                assert_eq!(width_px(&speed_only, dpi), speed_columns(&content, dpi).0);
                assert_runs_fit(&speed_only, dpi, width_px(&speed_only, dpi), 36 * dpi / 96);
                content.speed = None;
                assert_eq!(width_px(&content, dpi), performance_width);
                let only = layout(&content, performance_width, 48 * dpi / 96, dpi);
                assert!(!only.stacked());
                assert!(only.rect(Group::Speed).is_none());
                assert_eq!(only.rect(Group::Performance).unwrap().left, 0);
                assert_runs_fit(&content, dpi, performance_width, 36 * dpi / 96);
                content.performance = None;
                assert_eq!(width_px(&content, dpi), 0);
                assert_eq!(minimum_width_px(&content, dpi), 0);
            }
        }
    }
}

#[test]
fn performance_labels_and_percent_glyphs_stay_consistent_across_values() {
    for dpi in DPIS {
        for performance_first in [false, true] {
            for light in [false, true] {
                for colored in [false, true] {
                    let mut content = content(light, colored);
                    content.order = order(performance_first);
                    for (width, height) in [
                        (width_px(&content, dpi), 48 * dpi / 96),
                        (80 * dpi / 96, 72 * dpi / 96),
                    ] {
                        let mut baseline = None;
                        for (cpu, gpu) in [("100%", " --%"), ("  0%", "  9%"), (" --%", "100%")] {
                            content.performance.as_mut().unwrap().cpu = cpu.into();
                            content.performance.as_mut().unwrap().gpu = gpu.into();
                            let layout = layout(&content, width, height, dpi);
                            let mut surface =
                                unsafe { rasterize(&content, width, height, dpi).unwrap() };
                            let pixels = surface.pixels_mut();
                            let labels = runs(&content, &layout, dpi)
                                .into_iter()
                                .filter(|run| matches!(run.text, "CPU" | "GPU" | "%"))
                                .map(|run| area_pixels(pixels, width, run.area))
                                .collect::<Vec<_>>();
                            assert_eq!(labels.len(), 4);
                            assert!(labels
                                .iter()
                                .all(|label| label.iter().any(|pixel| pixel >> 24 > HIT_ALPHA)));
                            if let Some(expected) = &baseline {
                                assert_eq!(&labels, expected);
                            } else {
                                baseline = Some(labels);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn compact_columns_keep_groups_separate_and_render_review_cases() {
    for dpi in DPIS {
        for performance_first in [false, true] {
            for light in [false, true] {
                for (case, down, up, cpu, gpu, temperatures) in [
                    ("reported", "118B", "144B", "  3%", "  1%", None),
                    ("reordered", "344K", " 18K", "  2%", "  5%", None),
                    ("widest", "999M", "1.5G", "100%", " 99%", None),
                    ("unavailable", "  0B", "  9B", " --%", "100%", None),
                    (
                        "temperature-reordered",
                        "2.4K",
                        "1.1K",
                        "  2%",
                        "  2%",
                        Some((" 48°C", " 41°C")),
                    ),
                    (
                        "temperature-widest",
                        "999M",
                        "1.5G",
                        "100%",
                        " 99%",
                        Some(("150°C", " --°C")),
                    ),
                    (
                        "temperature-unavailable",
                        "  0B",
                        "  9B",
                        " --%",
                        " --%",
                        Some((" --°C", " --°C")),
                    ),
                ] {
                    let mut content = content(light, true);
                    content.order = order(performance_first);
                    content.speed = Some(Speed {
                        down: down.into(),
                        up: up.into(),
                        colored: true,
                    });
                    content.performance = Some(Performance {
                        temperatures: temperatures.map(|(cpu, gpu)| (cpu.into(), gpu.into())),
                        cpu: cpu.into(),
                        gpu: gpu.into(),
                        colored: true,
                    });
                    let width = width_px(&content, dpi);
                    let height = 36 * dpi / 96;
                    let layout = layout(&content, width, height, dpi);
                    let speed = layout.rect(Group::Speed).unwrap();
                    let perf = layout.rect(Group::Performance).unwrap();
                    assert_runs_fit(&content, dpi, width, height);
                    assert_eq!(layout.group_at(speed.left, 0), Some(Group::Speed));
                    assert_eq!(layout.group_at(perf.right - 1, 0), Some(Group::Performance));
                    let mut reversed = content.clone();
                    reversed.order = order(!performance_first);
                    assert_eq!(width_px(&reversed, dpi), width);
                    let mut surface = unsafe { rasterize(&content, width, height, dpi).unwrap() };
                    let pixels = surface.pixels_mut();
                    let (first, second) = if performance_first {
                        (perf, speed)
                    } else {
                        (speed, perf)
                    };
                    let gap = RECT {
                        left: first.right,
                        right: second.left,
                        top: 0,
                        bottom: height,
                    };
                    assert_eq!(gap.right - gap.left, 6 * dpi / 96);
                    assert!(area_pixels(pixels, width, gap)
                        .iter()
                        .all(|pixel| *pixel == HIT_ALPHA << 24));
                    assert_eq!(
                        layout.group_at((gap.left + gap.right) / 2, height / 2),
                        None
                    );
                    assert_performance_is_compact(&content, &layout, dpi);
                    // These review images come directly from the production GDI rasterizer.
                    if let Some(dir) = std::env::var_os("TM_BADGE_REVIEW_DIR") {
                        use std::io::Write;
                        let dir = std::path::PathBuf::from(dir);
                        std::fs::create_dir_all(&dir).unwrap();
                        let mut file = std::io::BufWriter::new(
                            std::fs::File::create(dir.join(format!(
                                "{case}-{dpi}-{}{}.ppm",
                                if performance_first {
                                    "performance"
                                } else {
                                    "network"
                                },
                                if light { "-light" } else { "" },
                            )))
                            .unwrap(),
                        );
                        write!(file, "P6\n{width} {height}\n255\n").unwrap();
                        let background = if light { [240, 243, 249] } else { [26, 32, 44] };
                        for &pixel in pixels.iter() {
                            let alpha = pixel >> 24;
                            for (shift, background) in [16, 8, 0].into_iter().zip(background) {
                                let channel = ((pixel >> shift) & 0xff)
                                    + (background * (255 - alpha) + 127) / 255;
                                file.write_all(&[channel.min(255) as u8]).unwrap();
                            }
                        }
                        file.flush().unwrap();
                    }
                }
            }
        }
    }
}

#[test]
fn transparent_raster_keeps_arrows_fixed_across_rates_and_dpi() {
    for dpi in DPIS {
        for light in [false, true] {
            for colored in [false, true] {
                let height = 48 * dpi / 96;
                let mut baseline = None;
                for (down, up) in [("  0B", "  9B"), ("1.6K", "875B"), ("999M", "1.5G")] {
                    let content = Content {
                        speed: Some(Speed {
                            down: down.into(),
                            up: up.into(),
                            colored,
                        }),
                        performance: None,
                        light,
                        colors: Default::default(),
                        order: order(false),
                    };
                    let width = width_px(&content, dpi);
                    let mut surface = unsafe { rasterize(&content, width, height, dpi).unwrap() };
                    let pixels = surface.pixels_mut();
                    assert_eq!(pixels[0], HIT_ALPHA << 24);
                    assert!(
                        pixels
                            .iter()
                            .filter(|pixel| **pixel == HIT_ALPHA << 24)
                            .count()
                            > pixels.len() / 2
                    );
                    assert!(pixels.iter().any(|pixel| pixel >> 24 == 255));
                    assert!(pixels.iter().any(|pixel| (2..255).contains(&(pixel >> 24))));
                    assert!(pixels.iter().all(|pixel| [
                        pixel & 0xff,
                        (pixel >> 8) & 0xff,
                        (pixel >> 16) & 0xff
                    ]
                    .iter()
                    .all(|channel| *channel <= pixel >> 24)));
                    let arrows = area_pixels(
                        pixels,
                        width,
                        RECT {
                            left: 0,
                            right: NUMBER_LEFT_DIP * dpi / 96,
                            top: 0,
                            bottom: height,
                        },
                    );
                    assert!(arrows.iter().any(|pixel| pixel >> 24 > HIT_ALPHA));
                    if let Some(expected) = &baseline {
                        assert_eq!(&arrows, expected);
                    } else {
                        baseline = Some(arrows);
                    }
                }
            }
        }
    }
}
