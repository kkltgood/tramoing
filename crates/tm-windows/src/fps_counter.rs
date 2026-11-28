//! Source-clock counting; ETW delivers events in batches, so arrival time is wrong.
use std::collections::{HashMap, VecDeque};

const MAX_CHAINS: usize = 32;
const MAX_PENDING: usize = 256;
const MAX_FRAMES: usize = 4096;

#[derive(Default)]
pub(crate) struct Counter {
    pub pid: u32,
    selected_at: i64,
    pending: HashMap<(u8, u32), (u64, i64)>,
    chains: HashMap<(u8, u64), VecDeque<i64>>,
}

impl Counter {
    #[cfg(windows)]
    pub fn diagnostics(&self, now: i64, frequency: i64) -> String {
        let newest = self
            .chains
            .values()
            .filter_map(|frames| frames.back())
            .max();
        let age_ms =
            newest.map(|at| now.saturating_sub(*at) as f64 * 1000.0 / frequency.max(1) as f64);
        format!(
            "pending={} swapchains={} frames={} newest_age_ms={age_ms:?}",
            self.pending.len(),
            self.chains.len(),
            self.chains.values().map(VecDeque::len).sum::<usize>()
        )
    }

    pub fn restart(&mut self, now: i64) {
        *self = Self {
            pid: self.pid,
            selected_at: now,
            ..Self::default()
        };
    }

    pub fn select(&mut self, pid: u32, now: i64) {
        if self.pid != pid {
            *self = Self {
                pid,
                selected_at: now,
                ..Self::default()
            };
        }
    }

    pub fn start(&mut self, runtime: u8, tid: u32, chain: u64, time: i64, test: bool) {
        self.pending.remove(&(runtime, tid));
        if self.pid == 0 || test || time < self.selected_at {
            return;
        }
        if self.pending.len() >= MAX_PENDING {
            // Lost stop events or retired threads must not permanently fill
            // the bounded table and prevent a new render thread from sampling.
            if let Some((oldest, oldest_time)) = self
                .pending
                .iter()
                .min_by_key(|(_, (_, started))| *started)
                .map(|(key, (_, started))| (*key, *started))
            {
                if time < oldest_time {
                    return;
                }
                self.pending.remove(&oldest);
            }
        }
        self.pending.insert((runtime, tid), (chain, time));
    }

    pub fn stop(&mut self, runtime: u8, tid: u32, success: bool, frequency: i64) {
        let Some((chain, time)) = self.pending.remove(&(runtime, tid)) else {
            return;
        };
        if !success || frequency <= 0 {
            return;
        }
        let key = (runtime, chain);
        if !self.chains.contains_key(&key) && self.chains.len() >= MAX_CHAINS {
            // Evict the least recently active chain, never sum unrelated chains.
            if let Some(oldest) = self
                .chains
                .iter()
                .min_by_key(|(_, v)| v.back().copied().unwrap_or(0))
                .map(|(k, _)| *k)
            {
                self.chains.remove(&oldest);
            }
        }
        let frames = self.chains.entry(key).or_default();
        if frames.back().is_some_and(|last| time <= *last) {
            return;
        }
        frames.push_back(time);
        while frames.len() > MAX_FRAMES
            // Keep the sample immediately before the one-second boundary so
            // ~1 FPS with scheduling jitter still has an interval to measure.
            || frames
                .get(1)
                .is_some_and(|next| time.saturating_sub(*next) >= frequency)
            || frames
                .front()
                .is_some_and(|first| time.saturating_sub(*first) > frequency.saturating_mul(2))
        {
            frames.pop_front();
        }
    }

    pub fn fps(&self, now: i64, frequency: i64) -> Option<f64> {
        if frequency <= 0 {
            return None;
        }
        // Compare every swapchain over a common source-time window. An old
        // high-FPS chain must not outrank its live replacement for two seconds
        // just because it retained a larger historical frame count. Using the
        // latest source sample, rather than wall time, tolerates ETW batching.
        let newest = self
            .chains
            .values()
            .filter_map(|frames| frames.back().copied())
            .filter(|last| *last <= now && now.saturating_sub(*last) <= frequency.saturating_mul(2))
            .max()?;
        let cutoff = newest.saturating_sub(frequency);
        self.chains
            .values()
            .filter_map(|frames| {
                let last = *frames.back()?;
                if last < cutoff {
                    return None;
                }
                let mut start = frames.partition_point(|time| *time < cutoff);
                if start > 0 && frames.get(start).is_some_and(|time| *time > cutoff) {
                    start -= 1;
                }
                let first = *frames.get(start)?;
                let count = frames.len() - start;
                let elapsed = last.saturating_sub(first);
                // Publish a short source-time span only after four completed
                // presents. Keep the previous 250-ms path for low frame rates:
                // a one-second window at 1–2 FPS never contains four presents.
                let enough_samples = (count >= 4 && elapsed >= (frequency / 10).max(1))
                    || (count >= 2 && elapsed >= (frequency / 4).max(1));
                if !enough_samples
                    || now.saturating_sub(last) > frequency.saturating_mul(2)
                    || last > now
                {
                    return None;
                }
                Some((
                    count,
                    (count - 1) as f64 * frequency as f64 / elapsed as f64,
                ))
            })
            .max_by_key(|(count, _)| *count)
            .map(|(_, fps)| fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(counter: &mut Counter, chain: u64, step: i64, start: i64) {
        for n in 0..=60 {
            counter.start(1, 7, chain, start + n * step, false);
            counter.stop(1, 7, true, 60_000);
        }
    }

    #[test]
    fn batches_use_source_timestamps_and_choose_one_swapchain() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        frames(&mut counter, 1, 1000, 0);
        frames(&mut counter, 2, 2000, 0);
        // Chain 1 stopped one source-second ago; only chain 2 has a current
        // sampling span. Its 30 FPS is neither summed nor hidden by old data.
        assert_eq!(counter.fps(120_000, 60_000), Some(30.0));
        assert_eq!(counter.fps(240_001, 60_000), None);
    }

    #[test]
    fn orphaned_calls_are_evicted_without_blocking_a_new_render_thread() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        for tid in 0..MAX_PENDING as u32 {
            counter.start(1, tid, 1, i64::from(tid), false);
        }
        counter.start(1, 1000, 2, 60_000, false);
        assert_eq!(counter.pending.len(), MAX_PENDING);
        assert!(!counter.pending.contains_key(&(1, 0)));
        // A late stop for the evicted call cannot create a synthetic frame.
        counter.stop(1, 0, true, 60_000);
        assert!(counter.chains.is_empty());
        counter.stop(1, 1000, true, 60_000);
        for n in 1..=60 {
            counter.start(1, 1000, 2, 60_000 + n * 1000, false);
            counter.stop(1, 1000, true, 60_000);
        }
        assert_eq!(counter.fps(120_000, 60_000), Some(60.0));
        assert!(counter.pending.len() <= MAX_PENDING);
    }

    #[test]
    fn completed_present_still_pairs_with_latest_start_on_the_same_thread() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        counter.start(1, 7, 1, 1000, false);
        counter.start(1, 7, 2, 2000, false);
        counter.stop(1, 7, true, 60_000);
        assert!(!counter.chains.contains_key(&(1, 1)));
        assert_eq!(counter.chains[&(1, 2)], VecDeque::from([2000]));
        counter.start(1, 7, 2, 3000, false);
        counter.start(1, 7, 2, 4000, true);
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.chains[&(1, 2)], VecDeque::from([2000]));
    }

    #[test]
    fn near_one_fps_jitter_is_measured_but_long_pauses_are_not_bridged() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        for n in 0..=10 {
            counter.start(1, 7, 1, n * 60_001, false);
            counter.stop(1, 7, true, 60_000);
        }
        let expected = 60_000.0 / 60_001.0;
        assert_eq!(counter.fps(600_010, 60_000), Some(expected));
        assert_eq!(counter.chains[&(1, 1)].len(), 2);
        assert_eq!(counter.fps(720_011, 60_000), None);
        counter.start(1, 7, 1, 720_011, false);
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.fps(720_011, 60_000), None); // over two-second pause
        counter.start(1, 7, 1, 780_012, false);
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.fps(780_012, 60_000), Some(expected));
    }

    #[test]
    fn current_swapchain_replaces_old_faster_chain_using_a_shared_source_window() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        for n in 0..=120 {
            counter.start(1, 7, 1, n * 500, false);
            counter.stop(1, 7, true, 60_000);
        }
        frames(&mut counter, 2, 1000, 66_000);
        assert_eq!(counter.fps(126_000, 60_000), Some(60.0));
        // Delivery latency does not shift the relative swapchain window.
        assert_eq!(counter.fps(216_000, 60_000), Some(60.0));
        assert_eq!(counter.fps(246_001, 60_000), None);
    }

    #[test]
    fn future_and_out_of_order_events_do_not_displace_the_current_chain() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        frames(&mut counter, 1, 1000, 0);
        counter.start(1, 7, 1, 59_000, false);
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.chains[&(1, 1)].back(), Some(&60_000));
        frames(&mut counter, 2, 1000, 1_000_000);
        assert_eq!(counter.fps(60_000, 60_000), Some(60.0));
    }

    #[test]
    fn first_reading_needs_four_successful_frames_and_one_tenth_second() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        for time in [0, 2000, 4000] {
            counter.start(1, 7, 1, time, false);
            counter.stop(1, 7, true, 60_000);
        }
        assert_eq!(counter.fps(6000, 60_000), None); // Three frames are insufficient.
        counter.start(1, 7, 1, 5999, false);
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.fps(6000, 60_000), None); // Four frames, but under 100 ms.
        counter.restart(0);
        for time in [0, 2000, 4000, 6000] {
            counter.start(1, 7, 1, time, false);
            counter.stop(1, 7, true, 60_000);
        }
        assert_eq!(counter.fps(6000, 60_000), Some(30.0));
        assert_eq!(counter.fps(126_001, 60_000), None); // Same stale-frame protection.
        counter.select(43, 126_001);
        assert_eq!(counter.fps(126_001, 60_000), None); // No previous target's value.
    }

    #[test]
    fn fast_sampling_still_uses_one_second_of_real_source_time() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        frames(&mut counter, 1, 1000, 0);
        assert_eq!(counter.fps(60_000, 60_000), Some(60.0));
        // Only four new 30-FPS frames have arrived; reporting must not suddenly
        // use only that short tail when a full source-time window is available.
        for time in [62_000, 64_000, 66_000, 68_000] {
            counter.start(1, 7, 1, time, false);
            counter.stop(1, 7, true, 60_000);
        }
        assert_eq!(counter.fps(68_000, 60_000), Some(56.0));
        assert_eq!(counter.fps(68_000, 0), None);
    }

    #[test]
    fn low_frame_rates_keep_the_existing_longer_observation_path() {
        for (step, expected) in [(60_000, 1.0), (30_000, 2.0)] {
            let mut counter = Counter::default();
            counter.select(42, 0);
            for n in 0..=10 {
                counter.start(1, 7, 1, n * step, false);
                counter.stop(1, 7, true, 60_000);
            }
            assert_eq!(counter.fps(10 * step, 60_000), Some(expected));
        }
    }

    #[test]
    fn target_switch_rejects_buffered_events_and_failed_or_test_presents() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        frames(&mut counter, 1, 1000, 0);
        counter.select(43, 70_000);
        frames(&mut counter, 1, 1000, 0);
        assert_eq!(counter.fps(100_000, 60_000), None);
        for n in 0..=60 {
            counter.start(1, 7, 1, 70_000 + n * 1000, n % 2 == 0);
            counter.stop(1, 7, false, 60_000);
        }
        assert_eq!(counter.fps(140_000, 60_000), None);
        frames(&mut counter, 1, 1000, 140_000);
        assert_eq!(counter.fps(200_000, 60_000), Some(60.0));
        counter.select(0, 200_001);
        assert_eq!(counter.fps(200_001, 60_000), None);
    }

    #[test]
    fn incomplete_and_extreme_producers_remain_bounded() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        for tid in 0..1000 {
            counter.start(1, tid, 1, 0, false);
        }
        assert_eq!(counter.pending.len(), MAX_PENDING);
        counter.pending.clear();
        for chain in 0..100 {
            frames(&mut counter, chain, 1, 0);
        }
        assert_eq!(counter.chains.len(), MAX_CHAINS);
        for n in 0..10_000 {
            counter.start(1, 7, 999, n, false);
            counter.stop(1, 7, true, 60_000);
        }
        assert_eq!(counter.chains[&(1, 999)].len(), MAX_FRAMES);
    }

    #[test]
    fn restarted_session_discards_incomplete_calls_and_readings_for_same_pid() {
        let mut counter = Counter::default();
        counter.select(42, 0);
        frames(&mut counter, 1, 1000, 0);
        counter.start(1, 7, 1, 61_000, false);
        counter.restart(70_000);
        assert_eq!(counter.pid, 42);
        assert!(counter.pending.is_empty());
        counter.stop(1, 7, true, 60_000);
        assert_eq!(counter.fps(70_000, 60_000), None);
        frames(&mut counter, 1, 1000, 80_000);
        assert_eq!(counter.fps(140_000, 60_000), Some(60.0));
    }
}
