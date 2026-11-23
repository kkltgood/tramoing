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
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
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
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
