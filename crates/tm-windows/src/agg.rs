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

use crate::proto::{Frame, ProcEntry};

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
        procs.sort_by(|a, b| (b.rx + b.tx).cmp(&(a.rx + a.tx)));
        Frame {
            seq: self.seq,
            procs,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn total(&self) -> (u64, u64) {
        self.map
            .values()
            .fold((0u64, 0u64), |acc, c| (acc.0 + c.rx, acc.1 + c.tx))
    }

    /// 只保留这些 pid，丢掉已退出的进程（防止长跑时无限增长）。
    pub fn retain_pids(&mut self, alive: &[u32]) {
        self.map.retain(|pid, _| alive.contains(pid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
