//! 同目录临时文件写完后再提交，失败时保留原文件并自动清理临时文件。

use std::io::{self, Write};
use std::path::Path;

pub fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = parent_dir(path);
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_saves_replace_the_file_and_failed_commits_leave_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.conf");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let blocked = dir.path().join("directory");
        std::fs::create_dir(&blocked).unwrap();
        assert!(atomic_write(&blocked, b"cannot replace a directory").is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        assert_eq!(parent_dir(Path::new("settings.conf")), Path::new("."));
    }
}
