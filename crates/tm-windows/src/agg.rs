//! 按 PID 累加字节，按秒取一次快照。
//!
//! # 为什么是累计值而不是每秒增量
//!
//! `nettop` 给的是累计字节，`RateCalculator` 靠对两次采样求差算速率。
//! 如果 Windows 侧改成送「每秒增量」，下游就得走另一条代码路径，两边行为迟早对不上。
//! 所以这里坚持送**累计值**，只是快照不清零 —— 语义和 macOS 完全一致。
//!
//! # 为什么拆成两个角色
//!
//! ETW 回调跑在系统的实时线程里，每秒可能上万次调用，里面做的事情越少越好：
//! 它只往 [`Aggregator`] 里加数字；取快照交给独立的定时线程。
//! 这个模块不碰任何 Windows API，所以能在 macOS 上直接跑单测。

use std::collections::HashMap;

use crate::proto::{Frame, ProcEntry, ENTRY_FIXED, FRAME_OVERHEAD, MAX_FRAME_BYTES, MAX_NAME};

const OVERFLOW_NAME: &str = "其他应用（采集容量上限）";

/// 流量方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// 进程发出的字节
    Tx,
    /// 进程收到的字节
    Rx,
}

#[derive(Debug, Clone, Copy, Default)]
struct Counter {
    rx: u64,
    tx: u64,
}

/// 累计计数器。ETW 回调写，定时线程读。
#[derive(Debug, Default)]
pub struct Aggregator {
    map: HashMap<u32, Counter>,
    seq: u64,
}

impl Aggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// ETW 回调里调用。同一 pid 多次调用会累加。
    pub fn add(&mut self, pid: u32, dir: Dir, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let c = self.map.entry(pid).or_default();
        match dir {
            Dir::Rx => c.rx = c.rx.saturating_add(bytes),
            Dir::Tx => c.tx = c.tx.saturating_add(bytes),
        }
    }

    /// 取当前累计快照。**不清空**，只推进帧号。
    /// `name_of` 由调用方注入（解析进程路径），保持本模块纯粹。
    pub fn snapshot_with<F>(&mut self, mut name_of: F) -> Frame
    where
        F: FnMut(u32) -> String,
    {
        self.seq = self.seq.wrapping_add(1);
        let mut procs: Vec<ProcEntry> = self
            .map
            .iter()
            .map(|(pid, c)| ProcEntry {
                pid: *pid,
                rx: c.rx,
                tx: c.tx,
                name: name_of(*pid),
            })
            .collect();
        // 按总量降序，界面端即使不排序也大致是对的
        procs.sort_by_key(|p| std::cmp::Reverse(p.rx.saturating_add(p.tx)));
        Frame {
            seq: self.seq,
            procs,
            sampled_at_us: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn total(&self) -> (u64, u64) {
        self.map.values().fold((0u64, 0u64), |acc, c| {
            (acc.0.saturating_add(c.rx), acc.1.saturating_add(c.tx))
        })
    }

    /// 只保留这些 pid，丢掉已退出的进程（防止长跑时无限增长）。
    pub fn retain_pids(&mut self, alive: &[u32]) {
        let alive: std::collections::HashSet<_> = alive.iter().copied().collect();
        self.map.retain(|pid, _| alive.contains(pid));
    }

    /// 取走这一批原始增量；采样线程独立于管道连接，历史 PID 不在这里积累。
    pub fn drain_with<F>(&mut self, name_of: F) -> Frame
    where
        F: FnMut(u32) -> String,
    {
        let frame = self.snapshot_with(name_of);
        self.map.clear();
        frame
    }
}

/// 对外累计按应用，PID 只留在一秒的原始事件批次中。
/// 合成 ID 在同一次助手运行里不复用，过期应用再次出现从零累计。
#[derive(Default)]
pub struct AppAccumulator {
    apps: HashMap<String, (u32, Counter, u64)>,
    next_id: u32,
    seq: u64,
}

impl AppAccumulator {
    pub fn update(&mut self, deltas: Frame) -> Frame {
        self.seq += 1;
        self.apps
            .retain(|_, (_, _, seen)| self.seq.saturating_sub(*seen) <= 600);
        // Reserve one overflow row and use the v2 entry size even when this
        // batch can still be encoded as v1. Long paths never enlarge a frame
        // beyond the same bound enforced by both encoder and pipe receiver.
        let mut bytes = FRAME_OVERHEAD
            + ENTRY_FIXED
            + OVERFLOW_NAME.len()
            + self
                .apps
                .keys()
                .filter(|name| name.as_str() != OVERFLOW_NAME)
                .map(|name| ENTRY_FIXED + name.len())
                .sum::<usize>();
        for p in deltas.procs {
            let is_new = !self.apps.contains_key(&p.name);
            let name = if p.name.len() > MAX_NAME
                || (is_new
                    && (self.apps.len() >= crate::proto::MAX_PROCS as usize - 1
                        || bytes + ENTRY_FIXED + p.name.len() > MAX_FRAME_BYTES))
            {
                OVERFLOW_NAME.to_owned()
            } else {
                p.name
            };
            let entry = self.apps.entry(name).or_insert_with_key(|name| {
                if name != OVERFLOW_NAME {
                    bytes += ENTRY_FIXED + name.len();
                }
                self.next_id = self.next_id.wrapping_add(1);
                (self.next_id, Counter::default(), self.seq)
            });
            entry.1.rx = entry.1.rx.saturating_add(p.rx);
            entry.1.tx = entry.1.tx.saturating_add(p.tx);
            entry.2 = self.seq;
        }
        Frame {
            seq: self.seq,
            sampled_at_us: deltas.sampled_at_us,
            procs: self
                .apps
                .iter()
                .map(|(name, (pid, c, _))| ProcEntry {
                    pid: *pid,
                    name: name.clone(),
                    rx: c.rx,
                    tx: c.tx,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_churn_stays_bounded_and_preserves_final_bytes() {
        let mut raw = Aggregator::new();
        let mut apps = AppAccumulator::default();
        for pid in 1..5000 {
            raw.add(pid, Dir::Rx, 10);
            let frame = apps.update(raw.drain_with(|_| "same.exe".into()));
            assert!(raw.is_empty());
            assert_eq!(frame.procs.len(), 1);
            assert_eq!(frame.procs[0].rx, pid as u64 * 10);
            assert!(Frame::decode(&frame.encode().unwrap()).is_ok());
        }
    }

    #[test]
    fn application_capacity_never_generates_an_invalid_frame() {
        let mut raw = Aggregator::new();
        for pid in 1..5000 {
            raw.add(pid, Dir::Rx, 1);
        }
        let mut apps = AppAccumulator::default();
        let frame = apps.update(raw.drain_with(|pid| format!("{pid}.exe")));
        assert_eq!(frame.procs.iter().map(|p| p.rx).sum::<u64>(), 4999);
        assert!(Frame::decode(&frame.encode().unwrap()).is_ok());
    }

    #[test]
    fn long_path_capacity_keeps_total_bytes_and_encodes_within_transport_limit() {
        let mut apps = AppAccumulator::default();
        let mut expected = 0;
        for batch in 0..3 {
            let entries = (0..100)
                .map(|pid| {
                    expected += 1;
                    ProcEntry {
                        pid,
                        rx: 1,
                        tx: 0,
                        name: format!("C:\\{}-{}-{pid}.exe", "测".repeat(30000), batch),
                    }
                })
                .collect();
            let frame = apps.update(Frame::new(batch, entries));
            let encoded = frame.encode().unwrap();
            assert!(encoded.len() <= MAX_FRAME_BYTES);
            assert_eq!(Frame::decode(&encoded).unwrap(), frame);
            assert_eq!(frame.procs.iter().map(|p| p.rx).sum::<u64>(), expected);
            assert!(frame.procs.iter().any(|p| p.name == OVERFLOW_NAME));
            assert!(frame
                .procs
                .iter()
                .filter(|p| p.name != OVERFLOW_NAME)
                .all(|p| p.name.ends_with(".exe")));
        }
    }

    #[test]
    fn capacity_is_reclaimed_after_long_path_entries_expire() {
        let mut apps = AppAccumulator::default();
        let name = "微".repeat(MAX_NAME / 3);
        let first = apps.update(Frame::new(
            1,
            vec![ProcEntry {
                pid: 1,
                rx: 7,
                tx: 0,
                name,
            }],
        ));
        assert_eq!(first.procs.len(), 1);
        for seq in 0..601 {
            apps.update(Frame::new(seq, Vec::new()));
        }
        let frame = apps.update(Frame::new(
            700,
            vec![ProcEntry {
                pid: 2,
                rx: 9,
                tx: 0,
                name: "new.exe".into(),
            }],
        ));
        assert_eq!(frame.procs.len(), 1);
        assert_eq!(frame.procs[0].name, "new.exe");
        assert_eq!(frame.procs[0].rx, 9);
        assert!(frame.encode().is_ok());
    }

    fn name_of(pid: u32) -> String {
        format!("proc{pid}.exe")
    }

    #[test]
    fn accumulates_by_pid_and_direction() {
        let mut a = Aggregator::new();
        a.add(100, Dir::Rx, 1000);
        a.add(100, Dir::Rx, 500);
        a.add(100, Dir::Tx, 200);
        let f = a.snapshot_with(name_of);

        assert_eq!(f.procs.len(), 1);
        assert_eq!(f.procs[0].pid, 100);
        assert_eq!(f.procs[0].rx, 1500);
        assert_eq!(f.procs[0].tx, 200);
        assert_eq!(f.procs[0].name, "proc100.exe");
    }

    #[test]
    fn snapshot_does_not_clear() {
        let mut a = Aggregator::new();
        a.add(1, Dir::Rx, 10);

        let f1 = a.snapshot_with(name_of);
        assert_eq!(f1.procs[0].rx, 10);

        // 累计值语义：第二次快照必须 >= 第一次，否则下游差分算出负数
        a.add(1, Dir::Rx, 5);
        let f2 = a.snapshot_with(name_of);
        assert_eq!(f2.procs[0].rx, 15);
        assert_eq!(f2.seq, f1.seq + 1);
    }

    #[test]
    fn zero_byte_events_are_ignored() {
        let mut a = Aggregator::new();
        a.add(1, Dir::Rx, 0);
        assert!(a.is_empty());
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        let mut a = Aggregator::new();
        a.add(1, Dir::Rx, u64::MAX);
        a.add(1, Dir::Rx, 10);
        let f = a.snapshot_with(name_of);
        assert_eq!(f.procs[0].rx, u64::MAX);
    }

    #[test]
    fn sorted_by_total_bytes() {
        let mut a = Aggregator::new();
        a.add(1, Dir::Rx, 100);
        a.add(2, Dir::Rx, 5000);
        a.add(3, Dir::Tx, 900);
        let f = a.snapshot_with(name_of);
        assert_eq!(f.procs[0].pid, 2);
        assert_eq!(f.procs[1].pid, 3);
        assert_eq!(f.procs[2].pid, 1);
    }

    #[test]
    fn retains_only_alive_pids() {
        let mut a = Aggregator::new();
        a.add(1, Dir::Rx, 1);
        a.add(2, Dir::Rx, 7);
        a.retain_pids(&[2]);
        assert_eq!(a.len(), 1);
        let f = a.snapshot_with(name_of);
        assert_eq!(f.procs[0].pid, 2);
        assert_eq!(a.total(), (7, 0));
    }
}
