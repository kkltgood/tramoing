use std::process::Command;

#[test]
fn export_reports_failure_to_scripts_without_overwriting_a_damaged_database() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("history.db");
    let damaged = b"not a SQLite database";
    std::fs::write(&database, damaged).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_tramoing"))
        .arg("--export")
        .env("TM_DB_PATH", &database)
        .env("TM_DATA_DIR", dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read(database).unwrap(), damaged);
    let log = std::fs::read_to_string(dir.path().join("ui.log")).unwrap();
    assert!(log.contains("ERROR") && log.contains("导出失败"));
}

#[test]
fn exporting_without_history_succeeds_without_creating_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("history.db");
    let output = Command::new(env!("CARGO_BIN_EXE_tramoing"))
        .arg("--export")
        .env("TM_DB_PATH", &database)
        .env("TM_DATA_DIR", dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!database.exists());
    assert!(dir.path().join("ui.log").is_file());
}

#[test]
fn diagnostics_can_be_exported_even_when_the_history_database_is_damaged() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("history.db");
    std::fs::write(&database, "damaged private history").unwrap();
    std::fs::write(
        dir.path().join("helper.log"),
        "ETW startup failed: access denied\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tramoing"))
        .arg("--export-diagnostics")
        .env("TM_DATA_DIR", dir.path())
        .env("TM_DB_PATH", &database)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let reports: Vec<_> = std::fs::read_dir(dir.path().join("diagnostics"))
        .unwrap()
        .collect();
    assert_eq!(reports.len(), 1);
    let report = std::fs::read_to_string(reports[0].as_ref().unwrap().path()).unwrap();
    assert!(report.contains("ETW startup failed: access denied"));
    assert!(report.contains("Version: 0.1."));
    assert!(!report.contains("damaged private history"));
    assert_eq!(
        std::fs::read_to_string(database).unwrap(),
        "damaged private history"
    );
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn default_paths_import_legacy_settings_before_export() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let base = root.path().to_path_buf();
    #[cfg(target_os = "macos")]
    let base = root.path().join("Library/Application Support");
    let old = base.join(tm_core::identity::LEGACY_APP_NAME);
    let new = base.join(tm_core::identity::APP_NAME);
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("settings.conf"), "max_apps=7\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tramoing"))
        .arg("--export")
        .env_remove("TM_DATA_DIR")
        .env_remove("TM_DB_PATH")
        .env("LOCALAPPDATA", root.path())
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(new.join("settings.conf")).unwrap(),
        "max_apps=7\n"
    );
    assert!(old.join("settings.conf").is_file());
    assert!(new.join(".legacy-data-imported").is_file());
    assert!(!new.join("history.db").exists());
}
