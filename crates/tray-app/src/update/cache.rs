//! Bounded cleanup of obsolete download files, without recursive deletion.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn remove_obsolete(root: &Path, current: &str) -> io::Result<()> {
    let current = semver::Version::parse(current).map_err(io::Error::other)?;
    let root = match Directory::open(root) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for name in root.names()? {
        let Some(text) = name.to_str() else { continue };
        let Ok(version) = semver::Version::parse(text) else {
            continue;
        };
        // Only names we generate are eligible. Future/pending versions are never
        // touched, regardless of what another process writes to pending.json.
        if version.to_string() != text || version.cmp_precedence(&current).is_gt() {
            continue;
        }
        let Ok(directory) = root.child(&name) else {
            continue;
        };
        let names = directory.names()?;
        if names.iter().any(|name| !download_file(name)) {
            continue;
        }
        let mut files = Vec::new();
        for name in &names {
            let Ok(file) = directory.file(name) else {
                break;
            };
            if !file.metadata()?.is_file() || file.try_lock().is_err() {
                break;
            }
            files.push(file);
        }
        // Keep a whole version if any file is linked, nonregular or still in use.
        if files.len() != names.len() {
            continue;
        }
        for name in &names {
            directory.remove(name, false)?;
        }
        drop(files);
        drop(directory);
        // remove_dir/unlinkat(AT_REMOVEDIR) only removes an empty directory. New
        // or unknown files can never be recursively removed by this cleanup.
        if let Err(error) = root.remove(&name, true) {
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn download_file(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if matches!(name, "Tramoing-setup.exe" | "Tramoing.dmg") {
        return true;
    }
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "part" | "json" | "attempts") {
        return false;
    }
    [
        "windows-x86_64-",
        "windows-aarch64-",
        "macos-x86_64-",
        "macos-aarch64-",
    ]
    .iter()
    .any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    })
}

struct Directory {
    path: PathBuf,
    #[cfg(unix)]
    file: File,
    #[cfg(windows)]
    _guards: Vec<File>,
}

impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(path)?;
            Ok(Self {
                path: path.to_owned(),
                file,
            })
        }
        #[cfg(windows)]
        {
            // Probe first so a nonexistent cache is a quiet no-op.
            std::fs::symlink_metadata(path)?;
            let guards = super::install::directory_guards(path).map_err(io::Error::other)?;
            Ok(Self {
                path: path.to_owned(),
                _guards: guards,
            })
        }
    }

    fn names(&self) -> io::Result<Vec<OsString>> {
        std::fs::read_dir(&self.path)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }

    fn child(&self, name: &OsStr) -> io::Result<Self> {
        #[cfg(unix)]
        return Ok(Self {
            path: self.path.join(name),
            file: self.open_at(name, libc::O_DIRECTORY)?,
        });
        #[cfg(windows)]
        Self::open(&self.path.join(name))
    }

    fn file(&self, name: &OsStr) -> io::Result<File> {
        #[cfg(unix)]
        return self.open_at(name, libc::O_NONBLOCK);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
                .open(self.path.join(name))?;
            super::install::reject_reparse(&file).map_err(io::Error::other)?;
            Ok(file)
        }
    }

    #[cfg(unix)]
    fn open_at(&self, name: &OsStr, flags: i32) -> io::Result<File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_bytes())?;
        // All lookup and deletion is relative to our already-open directory.
        // Renaming a version directory to a symlink cannot redirect the cleanup.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn remove(&self, name: &OsStr, directory: bool) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(name.as_bytes())?;
            let result = unsafe {
                libc::unlinkat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        #[cfg(windows)]
        if directory {
            std::fs::remove_dir(self.path.join(name))
        } else {
            std::fs::remove_file(self.path.join(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_known_files_of_installed_versions() {
        let root = tempfile::tempdir().unwrap();
        for version in ["0.1.0", "0.2.3", "0.2.4", "0.1.1", "v0.1.0", "notes"] {
            std::fs::create_dir(root.path().join(version)).unwrap();
            std::fs::write(root.path().join(version).join("Tramoing.dmg"), b"cache").unwrap();
        }
        std::fs::write(root.path().join("0.1.1/user-file.txt"), b"keep").unwrap();
        std::fs::write(root.path().join("pending.json"), b"keep pending").unwrap();
        remove_obsolete(root.path(), "0.2.3").unwrap();
        assert!(!root.path().join("0.1.0").exists());
        assert!(!root.path().join("0.2.3").exists());
        for version in ["0.2.4", "0.1.1", "v0.1.0", "notes"] {
            assert_eq!(
                std::fs::read(root.path().join(version).join("Tramoing.dmg")).unwrap(),
                b"cache"
            );
        }
        assert_eq!(
            std::fs::read(root.path().join("pending.json")).unwrap(),
            b"keep pending"
        );
    }

    #[test]
    fn cleanup_preserves_a_version_while_its_partial_is_locked() {
        let root = tempfile::tempdir().unwrap();
        let version = root.path().join("0.2.3");
        std::fs::create_dir(&version).unwrap();
        let path = version.join(format!("macos-aarch64-{}.part", "a".repeat(64)));
        std::fs::write(&path, b"partial").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        locked.lock().unwrap();
        remove_obsolete(root.path(), "0.2.3").unwrap();
        assert!(path.exists());
        drop(locked);
        remove_obsolete(root.path(), "0.2.3").unwrap();
        assert!(!version.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_follows_root_version_or_payload_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Tramoing.dmg"), b"outside").unwrap();
        symlink(outside.path(), root.path().join("0.1.0")).unwrap();
        std::fs::create_dir(root.path().join("0.1.1")).unwrap();
        symlink(
            outside.path().join("Tramoing.dmg"),
            root.path().join("0.1.1/Tramoing.dmg"),
        )
        .unwrap();
        remove_obsolete(root.path(), "0.2.3").unwrap();
        assert!(root
            .path()
            .join("0.1.0")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(root
            .path()
            .join("0.1.1/Tramoing.dmg")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        symlink(outside.path(), root.path().join("linked-root")).unwrap();
        assert!(remove_obsolete(&root.path().join("linked-root"), "0.2.3").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("Tramoing.dmg")).unwrap(),
            b"outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn swapping_a_directory_for_a_link_cannot_redirect_deletion() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Tramoing.dmg"), b"outside").unwrap();
        let original = root.path().join("0.1.0");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("Tramoing.dmg"), b"old").unwrap();
        let opened = Directory::open(&original).unwrap();
        std::fs::rename(&original, root.path().join("moved")).unwrap();
        symlink(outside.path(), &original).unwrap();
        opened.remove(OsStr::new("Tramoing.dmg"), false).unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("Tramoing.dmg")).unwrap(),
            b"outside"
        );
        assert!(!root.path().join("moved/Tramoing.dmg").exists());
    }
}
