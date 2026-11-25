//! 向图形界面交付错误；有界缓存与去重防止后台重试反复弹窗。
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 8;
const MAX_RECENT: usize = 64;
const QUIET_PERIOD: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorNotice {
    pub target: String,
    pub message: String,
}

struct Notices {
    pending: VecDeque<ErrorNotice>,
    recent: VecDeque<(ErrorNotice, Instant)>,
}

impl Notices {
    const fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            recent: VecDeque::new(),
        }
    }

    fn push(&mut self, notice: ErrorNotice, now: Instant) -> bool {
        self.recent
            .retain(|(_, at)| now.duration_since(*at) < QUIET_PERIOD);
        if self.recent.iter().any(|(previous, _)| previous == &notice) {
            return false;
        }
        if self.recent.len() == MAX_RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((notice.clone(), now));
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back(notice);
        true
    }
}

static NOTICES: Mutex<Notices> = Mutex::new(Notices::new());
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// 只唤醒 UI，由主线程展示弹框；不能在日志线程调用原生界面。
pub fn set_error_waker(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(wake));
}

pub fn report_error(target: &str, message: &str) {
    let notice = ErrorNotice {
        target: target.chars().take(128).collect(),
        message: message.chars().take(2048).collect(),
    };
    let added = NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(notice, Instant::now());
    if added {
        if let Some(wake) = WAKE.get() {
            wake();
        }
    }
}

pub fn take_errors() -> Vec<ErrorNotice> {
    NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .drain(..)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(message: &str) -> ErrorNotice {
        ErrorNotice {
            target: "sampler".into(),
            message: message.into(),
        }
    }

    #[test]
    fn retries_are_quiet_but_a_later_incident_is_reported() {
        let mut notices = Notices::new();
        let now = Instant::now();
        assert!(notices.push(notice("offline"), now));
        notices.pending.clear(); // 用户已关闭弹框。
        assert!(!notices.push(notice("offline"), now + Duration::from_secs(1)));
        assert!(notices.push(notice("disk full"), now + Duration::from_secs(2)));
        assert!(notices.push(notice("offline"), now + QUIET_PERIOD));
    }

    #[test]
    fn error_burst_keeps_memory_and_dialog_size_bounded() {
        let mut notices = Notices::new();
        for i in 0..1000 {
            notices.push(notice(&i.to_string()), Instant::now());
        }
        assert_eq!(notices.pending.len(), MAX_PENDING);
        assert_eq!(notices.recent.len(), MAX_RECENT);
        assert_eq!(notices.pending.back().unwrap().message, "999");
    }
}
