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
