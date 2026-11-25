//! Bounded, persistent diagnostics shared by the UI and privileged helper.
mod notices;
pub use notices::{report_error, set_error_waker, take_errors, ErrorNotice};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_LOG_BYTES: u64 = 1024 * 1024;
pub const BACKUPS: usize = 3;
const MAX_RECORD_BYTES: usize = 32 * 1024;

struct Logger {
    path: PathBuf,
    stderr: bool,
    write_error: Mutex<Option<String>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn timestamp() -> String {
    timestamp_at(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn timestamp_at(seconds: u64) -> String {
    // Gregorian civil date from days since the Unix epoch; always UTC.
    let z = (seconds / 86400) as i64 + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join("Tramoing-diagnostics")
}

pub fn log_path() -> Option<&'static Path> {
    LOGGER.get().map(|logger| logger.path.as_path())
}

pub fn write_error() -> Option<String> {
    LOGGER
        .get()
        .and_then(|logger| logger.write_error.lock().ok()?.clone())
}

/// Call before initializing the window, collector, settings, or database.
pub fn init(directory: &Path, component: &str, version: &str) -> io::Result<()> {
    let name = format!("{component}.log");
    let preferred = directory.join(&name);
    let header = format!(
        "{} INFO [startup] component={component} version={version} os={} arch={} pid={}\n",
        timestamp(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id()
    );
    let (path, primary_error) = match append(&preferred, &header, MAX_LOG_BYTES) {
        Ok(()) => (preferred, None),
        Err(error) => {
            let fallback = fallback_dir().join(name);
            append(&fallback, &header, MAX_LOG_BYTES)?;
            (
                fallback,
                Some(format!(
                    "Cannot write {}: {error}; using temporary log directory",
                    preferred.display()
                )),
            )
        }
    };
    let logger = LOGGER.get_or_init(|| Logger {
        path,
        stderr: std::env::var_os("TM_LOG").is_some(),
        write_error: Mutex::new(None),
    });
    log::set_logger(logger).map_err(|error| io::Error::other(error.to_string()))?;
    log::set_max_level(log::LevelFilter::Info);
    if let Some(error) = primary_error {
        log::warn!(target: "tramoing::logging", "{error}");
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Write synchronously: release builds abort immediately after this hook.
        log::error!(target: "tramoing::panic", "{info}\n{}", std::backtrace::Backtrace::force_capture());
        previous(info);
    }));
    Ok(())
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
            || (metadata.level() <= log::Level::Info
                && (metadata.target().starts_with("tramoing")
                    || metadata.target().starts_with("tm_")))
            || (self.stderr && metadata.level() <= log::Level::Info)
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let mut line = format!(
            "{} {} [{}] pid={} thread={} {}\n",
            timestamp(),
            record.level(),
            record.target(),
            std::process::id(),
            thread.name().unwrap_or("unnamed"),
            record.args()
        );
        if line.len() > MAX_RECORD_BYTES {
            let mut end = MAX_RECORD_BYTES - 32;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push_str("\n[record truncated]\n");
        }
        // Also serializes writes by threads; the OS lock below covers other processes.
        let mut error = self.write_error.lock().unwrap_or_else(|e| e.into_inner());
        match append(&self.path, &line, MAX_LOG_BYTES) {
            Ok(()) => *error = None,
            Err(e) => {
                *error = Some(format!("{}: {e}", self.path.display()));
                eprintln!("[diagnostics] cannot save log: {e}");
            }
        }
        if self.stderr {
            eprint!("{line}");
        }
        // 先释放日志锁再唤醒界面；诊断导出失败由当前导出界面显示，避免递归弹窗。
        let write_failure = error.clone();
        drop(error);
        if let Some(error) = write_failure {
            report_error("tramoing::logging", &format!("日志写入失败：{error}"));
        }
        if record.level() == log::Level::Error && record.target() != "tramoing::diagnostics" {
            report_error(record.target(), &record.args().to_string());
        }
    }

    fn flush(&self) {}
}

pub fn backup_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    name.into()
}

fn append(path: &Path, line: &str, limit: u64) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("log.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock()?;
    if fs::metadata(path).is_ok_and(|m| m.len().saturating_add(line.len() as u64) > limit) {
        let oldest = backup_path(path, BACKUPS);
        match fs::remove_file(oldest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for index in (1..=BACKUPS).rev() {
            let source = if index == 1 {
                path.to_path_buf()
            } else {
                backup_path(path, index - 1)
            };
            match fs::rename(source, backup_path(path, index)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    // Close after every record, so logs survive a panic/abort and can be exported.
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_readable_utc_including_leap_day() {
        assert_eq!(timestamp_at(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp_at(1709251199), "2024-02-29T23:59:59Z");
    }

    #[test]
    fn rotation_keeps_recent_records_and_caps_disk_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.log");
        for index in 0..20 {
            append(&path, &format!("record {index:02}\n"), 20).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "record 18\nrecord 19\n");
        for index in 1..=BACKUPS {
            assert!(fs::metadata(backup_path(&path, index)).unwrap().len() <= 20);
        }
        assert!(!backup_path(&path, BACKUPS + 1).exists());
    }

    #[test]
    fn concurrent_writers_preserve_whole_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.log");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        append(&path, "完整记录\n", MAX_LOG_BYTES).unwrap();
                    }
                });
            }
        });
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 100);
        assert!(text.lines().all(|line| line == "完整记录"));
    }
}
