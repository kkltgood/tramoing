//! Run with `cargo bench -p tm-core --bench window_tracker --locked`.
//! Measures the engine's four window queries together with each new sample.

use std::hint::black_box;
use std::time::Instant;

use tm_core::{AppKey, WindowTracker, MAX_SLOTS};

fn main() {
    const APPS: usize = 64;
    const SAMPLES: usize = 1000;
    let deltas: Vec<_> = (0..APPS)
        .map(|index| {
            (
                AppKey::new(format!(r"C:\Apps\Application-{index}\application.exe")),
                4096 + index as u64,
                1024 + index as u64,
            )
        })
        .collect();
    let mut tracker = WindowTracker::default();
    for _ in 0..MAX_SLOTS {
        tracker.push(&deltas);
    }

    let mut timings = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..SAMPLES {
            tracker.push(black_box(&deltas));
            black_box(tracker.rates(60));
            black_box(tracker.rates(300));
            black_box(tracker.rates(3600));
            black_box(tracker.bytes(600));
        }
        timings.push(started.elapsed());
    }
    timings.sort_unstable();
    println!(
        "{APPS} apps, {MAX_SLOTS}s history, median of 5 runs: {:.2} us/sample (push + four queries)",
        timings[2].as_secs_f64() * 1_000_000.0 / SAMPLES as f64,
    );
}
