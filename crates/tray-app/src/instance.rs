//! 每用户的 OS 文件锁，无陈旧 PID 文件；第二次启动请求已有界面打开。
use std::fs::{File, OpenOptions};
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

static ACTIVATE: OnceLock<std::path::PathBuf> = OnceLock::new();
pub struct Instance {
    _lock: File,
}

impl Instance {
    pub fn acquire(activate_existing: bool) -> io::Result<Option<Self>> {
        let dir = tm_store::data_dir();
        std::fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("ui.lock"))?;
        let activate = dir.join("ui.activate");
        match lock.try_lock() {
            Ok(()) => {
                // 锁在 OS 中，崩溃后自动释放；旧唤醒请求没有继续执行的意义。
                let _ = std::fs::remove_file(&activate);
                let _ = ACTIVATE.set(activate);
                Ok(Some(Self { _lock: lock }))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // A duplicate installer/update launch must not reveal an already
                // running panel. Normal user launches still activate it.
                if !activate_existing {
                    return Ok(None);
                }
                let request = if std::env::var("TM_VIEW").as_deref() == Ok("settings") {
                    "settings"
                } else {
                    "show"
                };
                std::fs::write(activate, request)?;
                Ok(None)
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// 每帧至多一次，主循环本身有 1Hz 心跳，不需要增加后台线程。
pub fn take_activation() -> Option<bool> {
    let path = ACTIVATE.get()?;
    let metadata = std::fs::metadata(path).ok()?;
    let fresh = metadata
        .modified()
        .ok()?
        .elapsed()
        .unwrap_or(Duration::ZERO)
        < Duration::from_secs(30);
    let request = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    fresh.then_some(request == "settings")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn os_file_lock_is_exclusive_and_recovers_without_deleting_a_pid_file() {
        let path = std::env::temp_dir().join(format!("tramoing-lock-test-{}", std::process::id()));
        let a = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let b = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        a.try_lock().unwrap();
        assert!(matches!(
            b.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(a);
        b.try_lock().unwrap();
        drop(b);
        std::fs::remove_file(path).unwrap();
    }
}
