//! Shared Windows icon, manifest and version resources for both executables.

use std::path::{Path, PathBuf};

pub fn embed(binary: &str, description: &str) {
    println!("cargo:rerun-if-changed=../../packaging/windows/resources.rs");
    println!("cargo:rerun-if-changed=../../packaging/windows/app.manifest");
    println!("cargo:rerun-if-changed=../../assets/tramoing.ico");
    println!("cargo:rerun-if-env-changed=RC_PATH");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Prefer the installed SDK when winresource cannot discover it via the registry.
    if std::env::var_os("RC_PATH").is_none() {
        if let Some(rc) = find_rc() {
            std::env::set_var("RC_PATH", rc);
        }
    }

    let mut res = winresource::WindowsResource::new();
    res.set_manifest_file("../../packaging/windows/app.manifest");
    res.set_icon("../../assets/tramoing.ico");
    res.set_language(0x0804);
    res.set("ProductName", "Tramoing");
    res.set("FileDescription", description);
    res.set("InternalName", binary);
    res.set("OriginalFilename", &format!("{binary}.exe"));
    res.set(
        "LegalCopyright",
        "Copyright (c) 2026 guolongkang. MIT License.",
    );

    if let Err(error) = res.compile() {
        let message = format!("Cannot embed resources for {binary}: {error}");
        if std::env::var("DEBUG").as_deref() == Ok("false") {
            panic!("{message}");
        }
        println!("cargo:warning={message}");
    }
}

fn find_rc() -> Option<PathBuf> {
    let kits = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"))
        .join("Windows Kits/10/bin");
    let mut versions: Vec<_> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("10."))
        })
        .collect();
    versions.sort_by_key(|path| version_key(path));
    versions
        .iter()
        .rev()
        .map(|path| path.join("x64/rc.exe"))
        .find(|path| path.is_file())
}

fn version_key(path: &Path) -> Vec<u32> {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}
