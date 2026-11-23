//! 首次使用新数据目录时导入旧数据；源目录保留为备份，已有新文件不覆盖。

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use tm_core::identity::{APP_NAME, LEGACY_APP_NAME};

use crate::{files, StoreError};

/// 必须在打开设置、数据库、恢复日志以及取得新 UI 锁之前调用。
pub(crate) fn migrate(base: &Path) -> Result<(), StoreError> {
    let source = base.join(LEGACY_APP_NAME);
    let target = base.join(APP_NAME);
    let marker = target.join(".legacy-data-imported");
    if !source.is_dir() || marker.exists() {
        return Ok(());
    }
    fs::create_dir_all(&target)?;
    let migration_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(target.join(".migration.lock"))?;
    migration_lock.lock()?;
    if marker.exists() {
        return Ok(());
    }
    let legacy_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(source.join("ui.lock"))?;
    legacy_lock.try_lock().map_err(|error| {
        io::Error::other(format!("请先退出旧版 Tramoing，再迁移历史数据：{error}"))
    })?;

    // SQLite 自己生成一致的快照，包含尚在 WAL 中的事务，不能仅复制 history.db。
    let database = target.join("history.db");
    let old_database = source.join("history.db");
    if old_database.is_file() && !database.exists() {
        let connection =
            Connection::open_with_flags(old_database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let snapshot = tempfile::NamedTempFile::new_in(&target)?.into_temp_path();
        connection.execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])?;
        snapshot.persist_noclobber(&database).map_err(|e| e.error)?;
    }
    // 不导入进程锁、唤醒请求、下载缓存或旧日志。保留 pending 防止丢失待落盘采样。
    for name in [
        "settings.conf",
        "network-rules-cache.json",
        "history.pending",
        "exports",
    ] {
        copy_missing(&source.join(name), &target.join(name))?;
    }
    files::atomic_write(
        &marker,
        b"Imported legacy data; original directory retained as backup.\n",
    )?;
    Ok(())
}

fn copy_missing(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "旧数据包含符号链接，未自动导入：{}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_missing(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if !target.exists() {
        let mut file = tempfile::NamedTempFile::new_in(files::parent_dir(target))?;
        io::copy(&mut fs::File::open(source)?, &mut file)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(target).map_err(|e| e.error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_wal_history_settings_and_pending_without_overwriting_new_files() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(LEGACY_APP_NAME);
        let new = root.path().join(APP_NAME);
        fs::create_dir_all(old.join("history.pending")).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("settings.conf"), "max_apps=9").unwrap();
        fs::write(new.join("settings.conf"), "max_apps=3").unwrap();
        fs::write(old.join("history.pending/batch.json"), "pending").unwrap();
        fs::write(old.join("network-rules-cache.json"), "[]").unwrap();
        let db = Connection::open(old.join("history.db")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture (bytes INTEGER); INSERT INTO fixture VALUES (42);").unwrap();
        migrate(root.path()).unwrap();
        let imported = Connection::open(new.join("history.db")).unwrap();
        let count: i64 = imported
            .query_row("SELECT bytes FROM fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 42);
        assert_eq!(
            fs::read_to_string(new.join("settings.conf")).unwrap(),
            "max_apps=3"
        );
        assert_eq!(
            fs::read_to_string(new.join("history.pending/batch.json")).unwrap(),
            "pending"
        );
        assert!(new.join("network-rules-cache.json").is_file());
        assert!(old.join("history.db").is_file());
        fs::remove_file(new.join("settings.conf")).unwrap();
        migrate(root.path()).unwrap();
        assert!(
            !new.join("settings.conf").exists(),
            "一次迁移后不能恢复用户已删除的设置"
        );
    }

    #[test]
    fn a_running_old_ui_or_damaged_database_prevents_partial_startup() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(LEGACY_APP_NAME);
        fs::create_dir(&old).unwrap();
        let lock = fs::File::create(old.join("ui.lock")).unwrap();
        lock.lock().unwrap();
        assert!(migrate(root.path()).is_err());
        drop(lock);
        fs::write(old.join("history.db"), "damaged").unwrap();
        assert!(migrate(root.path()).is_err());
        let new = root.path().join(APP_NAME);
        assert!(!new.join("history.db").exists());
        assert!(!new.join(".legacy-data-imported").exists());
        assert_eq!(
            fs::read_to_string(old.join("history.db")).unwrap(),
            "damaged"
        );
    }
}
