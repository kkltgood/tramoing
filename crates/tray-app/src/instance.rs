//! 每用户的 OS 文件锁，无陈旧 PID 文件；第二次启动请求已有界面打开。
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

static ACTIVATE: OnceLock<std::path::PathBuf> = OnceLock::new();
pub struct Instance {
    _lock: File,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Activation {
    pub settings: bool,
    pub check_update: bool,
}

impl Activation {
    fn encode(self) -> &'static str {
        match (self.settings, self.check_update) {
            (false, false) => "show",
            (true, false) => "settings",
            (false, true) => "check-update",
            (true, true) => "settings-check-update",
        }
    }

    fn decode(request: &str) -> Option<Self> {
        let (settings, check_update) = match request {
            "show" => (false, false),
            "settings" => (true, false),
            "check-update" => (false, true),
            "settings-check-update" => (true, true),
            _ => return None,
        };
        Some(Self {
            settings,
            check_update,
        })
    }
}

impl Instance {
    pub fn acquire(activation: Option<Activation>) -> io::Result<Option<Self>> {
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
                let Some(activation) = activation else {
                    return Ok(None);
                };
                tm_store::files::atomic_write(&activate, activation.encode().as_bytes())?;
                Ok(None)
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// 每帧至多一次，主循环本身有 1Hz 心跳，不需要增加后台线程。
pub fn take_activation() -> Option<Activation> {
    take_activation_from(ACTIVATE.get()?)
}

fn take_activation_from(path: &Path) -> Option<Activation> {
    let metadata = std::fs::metadata(path).ok()?;
    let fresh = metadata
        .modified()
        .ok()?
        .elapsed()
        .unwrap_or(Duration::ZERO)
        < Duration::from_secs(30);
    let request = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    fresh.then(|| Activation::decode(&request)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_commands_preserve_update_checks_and_are_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.activate");
        for settings in [false, true] {
            for check_update in [false, true] {
                let activation = Activation {
                    settings,
                    check_update,
                };
                tm_store::files::atomic_write(&path, activation.encode().as_bytes()).unwrap();
                assert_eq!(take_activation_from(&path), Some(activation));
                assert_eq!(take_activation_from(&path), None);
            }
        }
        std::fs::write(&path, "unknown-command").unwrap();
        assert_eq!(take_activation_from(&path), None);
        assert!(!path.exists());

        std::fs::write(&path, "check-update").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs(31)),
            )
            .unwrap();
        assert_eq!(take_activation_from(&path), None);
        assert!(!path.exists());
    }
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
