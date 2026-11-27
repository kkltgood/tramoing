//! Reverify the exact bytes handed to the native installer, off the UI thread.

use std::fs::File;
use std::path::Path;

use super::{download::valid_sha256, sha256_reader, Asset};

fn verify(file: &mut File, asset: &Asset) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};

    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || (asset.size > 0 && metadata.len() != asset.size) {
        return Err("更新安装包大小已改变，请重新检查更新".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    if !valid_sha256(&asset.sha256)
        || sha256_reader(file).map_err(|e| format!("校验更新安装包失败：{e}"))?
            != asset.sha256.trim().to_ascii_lowercase()
    {
        return Err("更新安装包校验失败，请重新检查更新".into());
    }
    Ok(())
}

pub(super) fn with_verified<T>(
    path: &Path,
    asset: &Asset,
    launch: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        // Deny writes/deletion until ShellExecute has opened the executable.
        // Directory handles also prevent replacing a parent with a new tree
        // between the digest check and the UAC/installer launch.
        let _directories = directory_guards(path.parent().ok_or("无效的更新路径")?)?;
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(path)
            .map_err(|e| format!("无法锁定更新安装包：{e}"))?;
        reject_reparse(&file)?;
        verify(&mut file, asset)?;
        let result = launch(path);
        drop(file);
        result
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        // Unix read handles do not prevent replacement or writes. Mount a private
        // snapshot whose digest is checked after copying, never reopen the cache.
        let directory = tempfile::Builder::new()
            .prefix("tramoing-verified-update-")
            .tempdir()
            .map_err(|e| format!("无法暂存更新安装包：{e}"))?;
        let snapshot = directory.path().join("Tramoing.dmg");
        let source = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("打开更新安装包失败：{e}"))?;
        let metadata = source.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("更新安装包不是普通文件".into());
        }
        if asset.size > 0 && metadata.len() != asset.size {
            return Err("更新安装包大小已改变，请重新检查更新".into());
        }
        // Bound the copy even if a concurrent writer keeps extending the source.
        let mut source = source.take(metadata.len().saturating_add(1));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&snapshot)
            .map_err(|e| format!("无法暂存更新安装包：{e}"))?;
        std::io::copy(&mut source, &mut file).map_err(|e| format!("暂存更新安装包失败：{e}"))?;
        verify(&mut file, asset)?;
        launch(&snapshot)
    }
}

#[cfg(windows)]
pub(super) fn reject_reparse(file: &File) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    if file
        .metadata()
        .map_err(|e| e.to_string())?
        .file_attributes()
        & 0x400
        != 0
    {
        return Err("更新缓存不能使用符号链接或目录联接".into());
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn directory_guards(path: &Path) -> Result<Vec<File>, String> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    if !path.is_absolute() {
        return Err("更新缓存必须使用绝对路径".into());
    }
    let mut guards = Vec::new();
    for directory in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let file = OpenOptions::new()
            .read(true)
            .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE, never DELETE
            .custom_flags(0x0200_0000 | 0x0020_0000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            .open(directory)
            .map_err(|e| format!("无法锁定更新缓存目录：{e}"))?;
        reject_reparse(&file)?;
        if !file.metadata().map_err(|e| e.to_string())?.is_dir() {
            return Err("更新缓存路径不是目录".into());
        }
        guards.push(file);
    }
    Ok(guards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn asset() -> Asset {
        Asset {
            url: "https://example.com/installer".into(),
            size: 3,
            sha256: format!("{:x}", Sha256::digest(b"abc")),
        }
    }

    #[test]
    fn missing_replaced_and_truncated_installers_never_reach_the_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("setup.exe");
        assert!(with_verified::<()>(&path, &asset(), |_| panic!("missing file launched")).is_err());
        for bytes in [b"bad".as_slice(), b"ab", b"abcd"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(
                with_verified::<()>(&path, &asset(), |_| panic!("changed file launched")).is_err()
            );
        }
    }

    #[test]
    fn cancellation_keeps_original_bytes_available_for_another_verified_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("setup.exe");
        std::fs::write(&path, b"abc").unwrap();
        let error = with_verified::<()>(&path, &asset(), |_| Err("cancelled".into())).unwrap_err();
        assert_eq!(error, "cancelled");
        with_verified(&path, &asset(), |verified| {
            assert_eq!(std::fs::read(verified).unwrap(), b"abc");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
    }

    #[cfg(not(windows))]
    #[test]
    fn changing_the_original_after_verification_does_not_change_the_mounted_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("setup.dmg");
        std::fs::write(&path, b"abc").unwrap();
        let snapshot = with_verified(&path, &asset(), |verified| {
            std::fs::write(&path, b"bad").unwrap();
            assert_eq!(std::fs::read(verified).unwrap(), b"abc");
            Ok(verified.to_path_buf())
        })
        .unwrap();
        assert!(!snapshot.exists());
    }

    #[cfg(windows)]
    #[test]
    fn installer_and_ancestor_replacement_are_denied_until_launch_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("cache");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("setup.exe");
        std::fs::write(&path, b"abc").unwrap();
        with_verified(&path, &asset(), |_| {
            assert!(std::fs::write(&path, b"bad").is_err());
            assert!(std::fs::remove_file(&path).is_err());
            assert!(std::fs::rename(&parent, directory.path().join("moved")).is_err());
            Ok(())
        })
        .unwrap();
        std::fs::write(&path, b"new").unwrap();
    }
}
