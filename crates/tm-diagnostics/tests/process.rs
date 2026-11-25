use std::process::Command;

#[test]
fn logger_child() {
    let Some(directory) = std::env::var_os("TM_TEST_LOG_DIR") else {
        return;
    };
    tm_diagnostics::init(std::path::Path::new(&directory), "ui", "test-version").unwrap();
    log::warn!(target: "window_backend", "window creation warning");
    log::error!(target: "tramoing::sampler", "collector unavailable: Win32 error 2");
    log::info!(target: "noisy_dependency", "must not persist");
    let errors = tm_diagnostics::take_errors();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("Win32 error 2"));
    log::error!(target: "tramoing::sampler", "collector unavailable: Win32 error 2");
    log::error!(target: "tramoing::diagnostics", "report could not be saved");
    assert!(
        tm_diagnostics::take_errors().is_empty(),
        "重复错误和导出自身失败不能递归弹框"
    );
    if std::env::var_os("TM_TEST_PANIC").is_some() {
        panic!("intentional diagnostic panic");
    }
}

#[test]
fn startup_framework_errors_and_panics_survive_process_exit() {
    for panic in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "logger_child", "--nocapture"])
            .env("TM_TEST_LOG_DIR", dir.path())
            .env_remove("TM_LOG")
            .env_remove("TM_TEST_PANIC");
        if panic {
            command.env("TM_TEST_PANIC", "1");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.success(), !panic);
        let log = std::fs::read_to_string(dir.path().join("ui.log")).unwrap();
        assert!(log.contains("test-version"));
        assert!(log.contains("window creation warning"));
        assert!(log.contains("Win32 error 2"));
        assert!(!log.contains("must not persist"));
        if panic {
            assert!(log.contains("intentional diagnostic panic"));
            assert!(log.contains("tramoing::panic"));
            assert!(log.contains("process.rs"));
        }
    }
}

#[test]
fn unwritable_data_directory_falls_back_and_records_the_original_error() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "keep").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "logger_child", "--nocapture"])
        .env("TM_TEST_LOG_DIR", &blocked)
        .env("TMPDIR", dir.path())
        .env("TMP", dir.path())
        .env("TEMP", dir.path())
        .env_remove("TM_TEST_PANIC")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let log = std::fs::read_to_string(dir.path().join("Tramoing-diagnostics/ui.log")).unwrap();
    assert!(log.contains("Cannot write") && log.contains("using temporary log directory"));
    assert!(log.contains("Win32 error 2"));
    assert_eq!(std::fs::read_to_string(blocked).unwrap(), "keep");
}
