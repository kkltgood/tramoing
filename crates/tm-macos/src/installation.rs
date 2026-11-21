//! Application bundle installation. File replacement is also tested on Windows;
//! mounting disk images, signatures and AppKit dialogs stay in the native module.
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
pub use native::{finish_if_requested, offer_install, show_error, start_update};

/// Only the executable directly inside Contents/MacOS identifies our bundle.
pub fn app_bundle(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

pub fn is_distribution_copy(bundle: &Path) -> bool {
    bundle.starts_with("/Volumes")
        || bundle
            .components()
            .any(|part| part.as_os_str() == "AppTranslocation")
}

/// Read Mach-O headers directly: the user's Mac must not need Xcode/lipo.
#[cfg(any(target_os = "macos", test))]
fn supports_cpu(header: &[u8], cpu: u32) -> bool {
    let Some(magic) = header.get(..4) else {
        return false;
    };
    let number = |offset: usize, little: bool| -> Option<u32> {
        let bytes: [u8; 4] = header.get(offset..offset + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let (little, stride) = match magic {
        [0xcf, 0xfa, 0xed, 0xfe] => return number(4, true) == Some(cpu),
        [0xfe, 0xed, 0xfa, 0xcf] => return number(4, false) == Some(cpu),
        [0xca, 0xfe, 0xba, 0xbe] => (false, 20),
        [0xbe, 0xba, 0xfe, 0xca] => (true, 20),
        [0xca, 0xfe, 0xba, 0xbf] => (false, 32),
        [0xbf, 0xba, 0xfe, 0xca] => (true, 32),
        _ => return false,
    };
    let count = number(4, little).unwrap_or(0) as usize;
    count <= 64
        && header.len() >= 8 + count * stride
        && (0..count).any(|index| number(8 + index * stride, little) == Some(cpu))
}

/// Keep the previous bundle until the new one has been put in place and opened.
/// On a rollback failure the caller must preserve `backup` for manual recovery.
#[cfg(any(target_os = "macos", test))]
fn replace_bundle(
    staged: &Path,
    destination: &Path,
    backup: &Path,
    launch: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !staged.is_dir() || backup.exists() {
        return Err("安装暂存文件无效，原应用未改动".into());
    }
    if std::fs::symlink_metadata(destination).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("安装目标是符号链接，请选择实际的应用目录".into());
    }
    let had_previous = destination.exists();
    if had_previous {
        std::fs::rename(destination, backup).map_err(|e| format!("保留旧版本失败：{e}"))?;
    }
    let result = std::fs::rename(staged, destination)
        .map_err(|e| format!("替换应用失败：{e}"))
        .and_then(|()| launch(destination));
    if let Err(error) = result {
        // If the second rename failed, the staged bundle is still in place.
        // If launch failed, move the new bundle back before restoring the old one.
        if !staged.exists() && destination.exists() {
            std::fs::rename(destination, staged).map_err(|e| {
                format!(
                    "{error}；无法移开新版本：{e}。旧版本位于 {}",
                    backup.display()
                )
            })?;
        }
        if had_previous {
            std::fs::rename(backup, destination)
                .map_err(|e| format!("{error}；恢复失败：{e}。旧版本位于 {}", backup.display()))?;
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_check_handles_thin_and_universal_apps_and_rejects_truncated_headers() {
        let arm64 = 0x0100_000c;
        let x86_64 = 0x0100_0007;
        let thin = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1];
        assert!(supports_cpu(&thin, arm64));
        assert!(!supports_cpu(&thin, x86_64));
        let mut fat = vec![0; 48];
        fat[..8].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]);
        fat[8..12].copy_from_slice(&u32::to_be_bytes(arm64));
        fat[28..32].copy_from_slice(&u32::to_be_bytes(x86_64));
        assert!(supports_cpu(&fat, arm64));
        assert!(supports_cpu(&fat, x86_64));
        assert!(!supports_cpu(&fat[..28], x86_64));
        assert!(!supports_cpu(b"not an executable", arm64));
    }

    #[test]
    fn bundle_detection_does_not_confuse_parent_directories_or_development_builds() {
        let bundle = Path::new("apps").join("父目录.app").join("Tramoing.app");
        assert_eq!(
            app_bundle(&bundle.join("Contents/MacOS/tramoing")),
            Some(bundle)
        );
        assert_eq!(app_bundle(Path::new("target/release/tramoing")), None);
        assert!(is_distribution_copy(Path::new(
            "/Volumes/Tramoing/Tramoing.app"
        )));
        assert!(is_distribution_copy(Path::new(
            "/private/var/folders/x/AppTranslocation/y/d/Tramoing.app"
        )));
        assert!(!is_distribution_copy(Path::new(
            "/Applications/Tramoing.app"
        )));
        assert!(!is_distribution_copy(Path::new(
            "/Users/me/code/dist/Tramoing.app"
        )));
    }

    fn bundle(path: &Path, version: &str) {
        std::fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        std::fs::write(path.join("Contents/MacOS/tramoing"), version).unwrap();
    }

    #[test]
    fn upgrade_replaces_the_whole_bundle_and_keeps_user_data_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Tramoing.app");
        let staged = dir.path().join("staged.app");
        let backup = dir.path().join("previous.app");
        bundle(&target, "old");
        bundle(&staged, "new");
        std::fs::write(target.join("obsolete-resource"), "old").unwrap();
        std::fs::write(dir.path().join("history.db"), "history").unwrap();
        replace_bundle(&staged, &target, &backup, |app| {
            assert_eq!(
                std::fs::read_to_string(app.join("Contents/MacOS/tramoing")).unwrap(),
                "new"
            );
            Ok(())
        })
        .unwrap();
        assert!(!target.join("obsolete-resource").exists());
        assert_eq!(
            std::fs::read_to_string(backup.join("Contents/MacOS/tramoing")).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("history.db")).unwrap(),
            "history"
        );
    }

    #[test]
    fn failed_launch_restores_the_previous_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Tramoing.app");
        let staged = dir.path().join("staged.app");
        let backup = dir.path().join("previous.app");
        bundle(&target, "old");
        bundle(&staged, "new");
        assert!(
            replace_bundle(&staged, &target, &backup, |_| Err("launch failed".into())).is_err()
        );
        assert_eq!(
            std::fs::read_to_string(target.join("Contents/MacOS/tramoing")).unwrap(),
            "old"
        );
        assert!(staged.is_dir());
        assert!(!backup.exists());
    }

    #[test]
    fn missing_payload_or_existing_backup_never_removes_the_installed_app() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Tramoing.app");
        let staged = dir.path().join("staged.app");
        let backup = dir.path().join("previous.app");
        bundle(&target, "old");
        assert!(replace_bundle(&staged, &target, &backup, |_| panic!("must not launch")).is_err());
        bundle(&staged, "new");
        bundle(&backup, "recovery");
        assert!(replace_bundle(&staged, &target, &backup, |_| panic!("must not launch")).is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("Contents/MacOS/tramoing")).unwrap(),
            "old"
        );
    }

    #[test]
    fn fresh_install_and_failed_first_launch_do_not_leave_a_partial_app() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Tramoing.app");
        let staged = dir.path().join("staged.app");
        let backup = dir.path().join("previous.app");
        bundle(&staged, "new");
        assert!(
            replace_bundle(&staged, &target, &backup, |_| Err("launch failed".into())).is_err()
        );
        assert!(!target.exists());
        replace_bundle(&staged, &target, &backup, |_| Ok(())).unwrap();
        assert!(target.is_dir());
        assert!(!backup.exists());
    }
}
